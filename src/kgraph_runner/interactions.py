"""Live sub-task telemetry and delivery of decisions to paused workflows."""

from __future__ import annotations

from codecs import getincrementaldecoder
from datetime import datetime, timezone
from graphlib import TopologicalSorter
from hashlib import sha256
import json
import os
from pathlib import Path, PureWindowsPath
import re
import stat
from typing import Optional

from ._files import file_lock, valid_id, write_json
from .qa_metrics import (
    GAP_TYPES, MANUAL_QA_VALUES, _run_qa_checklist, compute_qa_scorecard, record_qa_result, write_qa_scorecard,
)


_PROGRESS_STATUSES = ("pending", "running", "passed", "cached", "failed", "awaiting_subagents")
_ACTIVE_SUBTASK_PARENTS = ("awaiting_subagents", "running")
_HUMAN_AGENTS = {"reviewer", "user", "human", "approver", "owner", "lead"}
_QUESTION_FIELDS = ("question", "questions", "options", "recommended", "ticket", "multi_select", "allow_note")
_TASK_KINDS = {"code", "worker", "verifier", "gate"}
_PACKET_LIMIT = 256 * 1024
_UNSET = object()
_SECRET_VALUE = r"""(?:"[^"\r\n]*"|'[^'\r\n]*'|(?:https?://)?[^\s"'&,;]+)"""
_CREDENTIAL = re.compile(
    r"(?i)(?:\b(?:bearer|basic)\s+[\w.+/=-]+|"
    r"\b[\w-]*(?:token|password|passwd|secret|api[_ -]?key|credential|private[_ -]?key)[\w-]*"
    r"[\"']?\s*(?:=|:)\s*" + _SECRET_VALUE + r"|"
    r"--(?:[\w-]+-)?(?:token|password|secret|api-key)\s+" + _SECRET_VALUE + r"|"
    r"https?://[^\s/]+:[^\s/@]+@|"
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\Z)|"
    r"\b(?:gh[pousr]_|github_pat_|xox[baprs]-|sk-(?:proj-)?|npm_|AIza)[\w-]{16,}|"
    r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b)"
)
_PRIVATE_FIELD = re.compile(r"token|password|passwd|secret|key|credential", re.IGNORECASE)
_RESERVED_FIELDS = {
    "approved", "approved_by", "rejected_by", "answered_at", "notes", "resolved", "decisions",
    "summary", "review_sha256", "rejected_bugs", "source", "node_path", "packet_hash", "submitted_at",
}


def _object(value):
    return value if isinstance(value, dict) else {}


def _text(value, redact_fn=None):
    if not isinstance(value, str):
        return None
    value = _CREDENTIAL.sub("[redacted]", value).strip()
    return (redact_fn(value) if redact_fn else value) if value else None


def _now():
    return datetime.now(timezone.utc).isoformat()


def _reject_constant(value):
    raise ValueError(f"{value} is not valid JSON")


def _read_json(path, *, limit=None):
    flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0)
    with os.fdopen(os.open(path, flags), "rb") as file:
        info = os.fstat(file.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("Expected a regular JSON file")
        raw = file.read() if limit is None else file.read(limit + 1)
        modified = info.st_mtime_ns
    if limit is not None and len(raw) > limit:
        raise ValueError("JSON file exceeds the size limit")
    value = json.loads(raw.decode("utf-8"), parse_constant=_reject_constant)
    if not isinstance(value, dict):
        raise ValueError("Expected a JSON object")
    return value, modified, sha256(raw).hexdigest() if limit is not None else None


def _valid_node_path(value):
    return (isinstance(value, str) and "\\" not in value and ":" not in value and
            not any(ord(char) < 32 for char in value) and
            all(part.strip() and part not in (".", "..") for part in value.split("/")))


def progress_path_for_state(state_path: Path) -> Path:
    """Return the live progress sidecar beside the state file."""
    state = Path(state_path)
    return state.with_name(f"{state.stem}_progress.json")


def _progress_items(entry):
    items = _object(entry).get("items")
    if isinstance(items, dict):
        pairs = items.items()
    elif isinstance(items, list):
        pairs = ((item.get("id"), item) for item in items if isinstance(item, dict))
    else:
        return {}
    return {key: {**item, "id": key} for key, item in pairs
            if valid_id(key) and isinstance(item, dict)}


def _read_progress_file(path):
    data, _, _ = _read_json(path)
    return {node: {**entry, "items": _progress_items(entry)} for node, entry in data.items()
            if _valid_node_path(node) and isinstance(entry, dict)}


def read_node_progress(state_path, warnings=None) -> dict:
    """Read sidecar telemetry, tolerating absent and malformed files."""
    path = progress_path_for_state(state_path)
    try:
        return _read_progress_file(path)
    except FileNotFoundError:
        return {}
    except (OSError, ValueError, RecursionError) as exc:
        if warnings is not None:
            warnings.append(f"Could not read {path}: {exc}")
        return {}


def _order(item):
    value = item.get("order")
    return value if type(value) is int and value >= 0 else 0


def _valid_progress(value):
    return value is None or (type(value) in (int, float) and 0 <= value <= 100)


def update_node_progress(state_path, node_path, *, item_id=None, status=_UNSET, title=_UNSET,
                         kind=_UNSET, detail=_UNSET, deps=_UNSET, agent=_UNSET,
                         progress=_UNSET, outputs=_UNSET, clear=False, question=_UNSET,
                         questions=_UNSET, options=_UNSET, recommended=_UNSET, ticket=_UNSET,
                         multi_select=_UNSET, allow_note=_UNSET) -> dict:
    """Merge the supplied item fields and return the complete updated sidecar."""
    if not _valid_node_path(node_path):
        raise ValueError("Invalid node path")
    if (item_id is not None or not clear) and not valid_id(item_id):
        raise ValueError("A safe item ID is required")
    if status is not _UNSET and status not in _PROGRESS_STATUSES:
        raise ValueError("Invalid sub-task status")
    if kind is not _UNSET and kind not in tuple(_TASK_KINDS):
        raise ValueError("Invalid sub-task kind")
    if progress is not _UNSET and not _valid_progress(progress):
        raise ValueError("Progress must be a number from 0 to 100")
    fields = {"status": status, "title": title, "kind": kind, "detail": detail,
              "deps": deps, "agent": agent, "progress": progress, "outputs": outputs}
    question_fields = dict(zip(_QUESTION_FIELDS, (question, questions, options, recommended, ticket,
                                                  multi_select, allow_note)))
    fields.update(_validated_question_fields(question_fields))
    for key in ("title", "detail", "agent"):
        if fields[key] is not _UNSET and fields[key] is not None and not isinstance(fields[key], str):
            raise ValueError(f"{key} must be text")
    for key in ("deps", "outputs"):
        if fields[key] is _UNSET:
            continue
        if isinstance(fields[key], (str, bytes)):
            raise ValueError(f"{key} must be a sequence")
        fields[key] = list(fields[key])
        if any(not (valid_id(value) if key == "deps" else isinstance(value, str)) for value in fields[key]):
            raise ValueError(f"Invalid {key} entry")

    path = progress_path_for_state(state_path)
    with file_lock(path.with_suffix(".lock")):
        try:
            data = _read_progress_file(path)
        except FileNotFoundError:
            data = {}
        entry = data.get(node_path, {})
        items = entry.get("items", {})
        if clear:
            if item_id is None:
                data.pop(node_path, None)
            elif item_id in items:
                del items[item_id]
        else:
            item = items.get(item_id)
            if item is None:
                order = max(max((_order(value) for value in items.values()), default=-1) + 1,
                            _order({"order": entry.get("next_order")}))
                item = {"id": item_id, "order": order, "status": "pending", "kind": "code",
                        "title": item_id, "detail": None, "deps": [], "agent": None,
                        "progress": None, "outputs": []}
                items[item_id] = item
                entry["next_order"] = order + 1
            previous = item.copy()
            if fields.get("question") is not None:
                item.pop("questions", None)
            if fields.get("questions") is not None:
                for key in _QUESTION_FIELDS:
                    item.pop(key, None)
            item.update((key, value) for key, value in fields.items() if value is not _UNSET)
            if previous.get("submitted"):
                if _subtask_questions(previous) != _subtask_questions(item):
                    item.pop("submitted", None)
                    item.pop("decision", None)
                    item.pop("decision_summary", None)
                elif status == "awaiting_subagents" and previous.get("status") == "passed":
                    item.update(status="passed", progress=previous.get("progress"), detail=previous.get("detail"))
            item["updated_at"] = _now()
            data[node_path] = {**entry, "items": items}
        write_json(path, data)
    return data


def ask_node_question(state_path, node_path, *, item_id, question=None, questions=None, title=None,
                      ticket=None, detail=None, options=(), recommended=None, multi_select=False,
                      allow_note=True, agent="Reviewer", deps=()) -> dict:
    """Publish an interactive question while its parent stage is active."""
    if (question is None) == (questions is None):
        raise ValueError("Provide either question or questions")
    shorthand = {"question": question, "ticket": ticket, "options": options, "recommended": recommended,
                 "multi_select": multi_select, "allow_note": allow_note}
    fields = _validated_question_fields({"questions": questions} if questions is not None else shorthand)
    normalized = _questions(fields, None)
    return update_node_progress(state_path, node_path, item_id=item_id, status="awaiting_subagents",
                                kind="gate", agent=agent, title=title or question or item_id,
                                detail=detail or question, deps=deps, progress=None, questions=normalized)


def _wave_edges(nodes, warnings, parent):
    by_id = {node["id"]: node for node in nodes}
    dependencies = {}
    edges = []
    for node in nodes:
        deps = dependencies[node["id"]] = []
        for dep in dict.fromkeys(node["deps"]):
            if dep in by_id:
                deps.append(dep)
                edges.append({"from": by_id[dep]["path"], "to": node["path"]})
            else:
                warnings.append(f"{node['path']} depends on unknown sibling {dep}")
    sorter = TopologicalSorter(dependencies)
    try:
        for node_id in sorter.static_order():
            by_id[node_id]["wave"] = max(
                (by_id[dep]["wave"] + 1 for dep in dependencies[node_id]), default=0,
            )
    except ValueError:
        warnings.append(f"Dependency cycle in {parent or 'the root graph'}")
    return edges


def build_node_sub_graph(node_path, entry, warnings, *, state_path=None, allow_respond=True,
                         redact_fn=None) -> tuple[Optional[dict], Optional[dict]]:
    """Convert progress items to the same topology consumed by the town canvas."""
    items = _progress_items(entry)
    if not items:
        return None, None
    counts = dict.fromkeys(_PROGRESS_STATUSES, 0)
    nodes = []
    for key, item in sorted(items.items(), key=lambda pair: (_order(pair[1]), pair[0])):
        status, kind = item.get("status"), item.get("kind")
        status = status if status in _PROGRESS_STATUSES else "pending"
        kind = kind if kind in tuple(_TASK_KINDS) else "code"
        counts[status] += 1
        deps, outputs = item.get("deps"), item.get("outputs")
        progress = item.get("progress")
        node = {
            "id": key, "title": _text(item.get("title"), redact_fn) or key, "path": f"{node_path}::{key}",
            "node_type": "task", "kind": kind,
            "deps": [dep for dep in deps if valid_id(dep)] if isinstance(deps, list) else [],
            "wave": 0, "status": status, "attempts": item.get("attempts", 0), "duration_s": None,
            "detail": _text(item.get("detail"), redact_fn), "agent": _text(item.get("agent"), redact_fn),
            "progress": progress if _valid_progress(progress) else None,
            "outputs": [text for value in outputs if (text := _text(value, redact_fn))]
            if isinstance(outputs, list) else [],
            "output_hashes": {}, "error_signature": None, "started_at": None,
            "updated_at": _text(item.get("updated_at")), "children": [], "edges": [],
        }
        for field in ("ticket", "question", "recommended"):
            if (value := _text(item.get(field), redact_fn)) is not None:
                node[field] = value
        if isinstance(options := item.get("options"), list):
            node["options"] = [label for option in options if (label := _text(option, redact_fn))]
        if item.get("questions"):
            node["questions"] = _questions(item, redact_fn)
        if gate := extract_subtask_input_gate(node_path, item, state_path=state_path,
                                             allow_respond=allow_respond, redact_fn=redact_fn):
            node["input_gate"] = gate
        if submitted := _object(item.get("submitted")):
            node["submitted"] = _subtask_submission(submitted, redact_fn)
        if summary := _text(item.get("decision_summary"), redact_fn) or _object(node.get("submitted")).get("summary"):
            node["decision_summary"] = summary
        nodes.append(node)
    graph = {"nodes": nodes, "edges": _wave_edges(nodes, warnings, node_path), "counts": counts}
    progress = {"total": len(nodes), "done": counts["passed"] + counts["cached"],
                "running": counts["running"], "failed": counts["failed"],
                "waiting": counts["awaiting_subagents"], "pending": counts["pending"]}
    return graph, progress


def _allowed_root(state_path):
    state = Path(state_path).resolve()
    for parent in state.parents:
        if parent.name == ".kgraph":
            return parent
    raise PermissionError("Interactive responses require the state file to be inside a .kgraph directory")


def _reject_path_traversal(path):
    if ".." in PureWindowsPath(path).parts:
        raise PermissionError("Gate paths cannot contain parent traversal")


def _inside(path, root):
    resolved = path.resolve(strict=False)
    if resolved == root or not resolved.is_relative_to(root):
        raise PermissionError("Gate paths must stay inside the workflow's .kgraph directory")
    return resolved


def validate_kgraph_output_path(raw_output_path, *, state_path, packet_path=None, project_dir=None) -> Path:
    """Resolve and validate a packet's declared writable JSON destination."""
    if not isinstance(raw_output_path, str) or not raw_output_path.strip() or "\0" in raw_output_path:
        raise ValueError("The packet must declare a JSON output path")
    output = Path(raw_output_path)
    if output.suffix.lower() != ".json":
        raise ValueError("The gate output must be a .json file")
    _reject_path_traversal(output)
    root = _allowed_root(state_path)
    if packet_path is not None:
        packet_path = Path(packet_path)
        _reject_path_traversal(packet_path)
        _inside(packet_path, root)
    base = packet_path.parent if packet_path is not None else Path(project_dir or Path(state_path).parent)
    resolved = _inside(base / output, root)
    if (resolved.suffix.lower() != ".json" or resolved in
            {Path(state_path).resolve(), packet_path.resolve() if packet_path is not None else None}
            or (resolved.exists() and not resolved.is_file())):
        raise PermissionError("The gate requires a separate JSON response file")
    return resolved


def _output_path(packet):
    outputs = packet.get("outputs")
    return (packet.get("output_path") or packet.get("approval_path") or
            (outputs[0] if isinstance(outputs, (list, tuple)) and outputs else None))


def _questions(packet, redact_fn):
    raw = packet.get("questions")
    if not isinstance(raw, list) or not raw:
        raw = [packet] if isinstance(packet.get("question"), str) else []
    questions, identifiers = [], set()
    for index, entry in enumerate(raw, 1):
        entry = {"question": entry} if isinstance(entry, str) else _object(entry)
        question = _text(entry.get("question"), redact_fn)
        if not question:
            continue
        key = _text(entry.get("id"), redact_fn) or f"q{index}"
        if key in identifiers:
            key = f"q{index}"
            while key in identifiers:
                key += "_"
        identifiers.add(key)
        options = entry.get("options")
        options = [label for option in options if (label := _text(option, redact_fn))] if isinstance(options, list) else []
        questions.append({
            "id": key, "ticket": _text(entry.get("ticket") or entry.get("ticket_id"), redact_fn),
            "question": question, "options": list(dict.fromkeys(options)),
            "recommended": _text(entry.get("recommended"), redact_fn),
            "multi_select": entry.get("multi_select") is True,
            "allow_note": entry.get("allow_note") is not False,
        })
    return questions


def _validated_question_fields(fields):
    result = {key: value for key, value in fields.items() if value is not _UNSET}
    for key in ("id", "question", "recommended", "ticket"):
        value = result.get(key)
        if value is not None and (not isinstance(value, str) or not value.strip() or "\0" in value):
            raise ValueError(f"{key} must be nonempty text")
    for key in ("multi_select", "allow_note"):
        if key in result and type(result[key]) is not bool:
            raise ValueError(f"{key} must be a boolean")
    if (options := result.get("options")) is not None:
        if (not isinstance(options, (list, tuple)) or
                any(not isinstance(option, str) or not option.strip() or "\0" in option for option in options)):
            raise ValueError("options must be a list of nonempty strings")
        result["options"] = list(options)
    if (questions := result.get("questions")) is not None:
        if result.get("question") is not None:
            raise ValueError("Provide either question or questions")
        if not isinstance(questions, list) or not questions:
            raise ValueError("questions must be a nonempty list of question objects")
        result["questions"] = []
        for entry in questions:
            if not isinstance(entry, dict) or entry.get("question") is None:
                raise ValueError("Each question must be an object with question text")
            result["questions"].append(_validated_question_fields({
                key: entry[key] for key in ("id", "question", "options", "recommended", "ticket",
                                            "multi_select", "allow_note") if key in entry
            }))
    return result


def _subtask_questions(item):
    if item.get("questions") or item.get("question"):
        return _questions(item, None)
    agent = _text(item.get("agent")) or ""
    if item.get("kind") == "gate" or agent.casefold() in _HUMAN_AGENTS:
        return _questions({"id": item["id"], "ticket": item.get("ticket"),
                           "question": _text(item.get("detail")) or _text(item.get("title")) or item["id"]}, None)
    return []


def _subtask_submission(doc, redact_fn=None):
    def redact_answer(value):
        if isinstance(value, list):
            return [_text(answer, redact_fn) for answer in value]
        return _text(value, redact_fn)

    return {"answered_at": _text(doc.get("answered_at")), "approved_by": _text(doc.get("approved_by"), redact_fn),
            "summary": _text(doc.get("summary"), redact_fn),
            "decisions": {name: redact_answer(value) for key, value in _object(doc.get("decisions")).items()
                          if (name := _text(key, redact_fn))},
            "notes": _text(doc.get("notes"), redact_fn) or ""}


def extract_subtask_input_gate(node_path, item, *, state_path, allow_respond=True, redact_fn=None) -> Optional[dict]:
    """Build a gate for a waiting human sub-task using its public question fields."""
    item_id = item.get("id")
    if item.get("status") != "awaiting_subagents" or not (_valid_node_path(node_path) and valid_id(item_id)):
        return None
    questions = _subtask_questions(item)
    if not questions:
        return None
    full_path = f"{node_path}::{item_id}"
    identity = json.dumps([full_path, questions], sort_keys=True, separators=(",", ":"))
    if redact_fn:
        questions = _questions({"questions": questions}, redact_fn)
    if not questions:
        return None
    gate = {"gate_type": "questions", "node_path": full_path, "parent_node_path": node_path,
            "subtask_item_id": item_id, "questions": questions,
            "packet_hash": sha256(identity.encode()).hexdigest(), "can_respond": bool(allow_respond)}
    if state_path is not None and allow_respond:
        try:
            _subtask_decision_path(Path(state_path).resolve(), item_id)
        except (ValueError, OSError, RuntimeError):
            gate["can_respond"] = False
    if submitted := _object(item.get("submitted")):
        gate["submitted"] = _subtask_submission(submitted, redact_fn)
    return gate


def _approval_fields(packet, redact_fn):
    fields = packet.get("approved_fields")
    if isinstance(fields, dict):
        fields = [{"key": key, **(value if isinstance(value, dict) else {"default": value, "label": value})}
                  for key, value in fields.items()]
    elif not isinstance(fields, list):
        fields = [{"key": key, "default": value} for key, value in _object(packet.get("approval_example")).items()
                  if key.startswith("approve_") and isinstance(value, bool)]
    allowed = {}
    for field in fields:
        field = {"key": field} if isinstance(field, str) else _object(field)
        key = field.get("key")
        if (isinstance(key, str) and re.fullmatch(r"[A-Za-z][A-Za-z0-9_]{0,79}", key)
                and key not in _RESERVED_FIELDS and not _PRIVATE_FIELD.search(key)):
            label = key.replace("_", " ").capitalize()
            allowed[key] = {"key": key, "label": _text(field.get("label"), redact_fn) or label,
                            "default": field.get("default") is not False}
    return list(allowed.values())


def _bugs(packet, redact_fn):
    bugs = packet.get("bugs")
    result = {}
    for bug in bugs if isinstance(bugs, list) else []:
        bug = {"id": bug} if isinstance(bug, str) else _object(bug)
        if key := _text(bug.get("id"), redact_fn):
            result[key] = {"id": key, "title": _text(bug.get("title"), redact_fn) or key}
    return list(result.values())


def _submission(doc, *, answered_at=None):
    return {
        "answered_at": _text(doc.get("answered_at")) or answered_at,
        "approved_by": _text(doc.get("approved_by") or doc.get("rejected_by")),
        "approved": doc.get("approved") if type(doc.get("approved")) is bool else None,
        "summary": _text(doc.get("summary")),
    }


def extract_input_gate(packet, packet_path, *, node_path, state_path, project_dir,
                       allow_respond=True, redact_fn=None) -> Optional[dict]:
    """Build a public gate description without exposing the writable target."""
    if not isinstance(packet, dict):
        return None
    questions = _questions(packet, redact_fn)
    fields = _approval_fields(packet, redact_fn)
    approval = (packet.get("stage") == "user_verify_gate" or packet.get("node_id") == "user_verify_gate"
                or bool(packet.get("approval_path")) or "approved_fields" in packet or "approval_example" in packet)
    if not questions and not approval:
        return None
    gate = {
        "gate_type": ("hybrid" if questions else "approval") if approval else "questions",
        "node_path": node_path, "questions": questions, "approved_fields": fields,
        "review_sha256": _text(packet.get("review_sha256"), redact_fn),
        "review_summary": _text(packet.get("review_summary") or packet.get("review_summary_path"), redact_fn),
        "bugs": _bugs(packet, redact_fn),
    }
    raw_output = _output_path(packet)
    identity = {**gate, "target": raw_output if isinstance(raw_output, str) else None,
                "revision": packet.get("revision"), "stale_approval": packet.get("stale_approval_sha256")}
    gate["packet_hash"] = sha256(json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    gate["can_respond"] = bool(allow_respond and _valid_node_path(node_path) and state_path is None)
    if state_path is None:
        return gate
    try:
        output = validate_kgraph_output_path(raw_output, state_path=state_path, packet_path=packet_path,
                                            project_dir=project_dir)
    except (ValueError, OSError, RuntimeError):
        return gate
    gate["can_respond"] = bool(allow_respond and _valid_node_path(node_path))
    try:
        packet_modified = Path(packet_path).stat().st_mtime_ns if packet_path is not None else 0
        doc, modified, digest = _read_json(output, limit=_PACKET_LIMIT)
    except (ValueError, OSError, RecursionError):
        return gate
    review = gate["review_sha256"]
    valid = ((not questions or doc.get("resolved") is True)
             and (not approval or type(doc.get("approved")) is bool)
             and (not review or doc.get("review_sha256") == review))
    if valid and modified >= packet_modified and digest != packet.get("stale_approval_sha256"):
        stamp = datetime.fromtimestamp(modified / 1_000_000_000, timezone.utc).isoformat()
        gate["submitted"] = _submission(doc, answered_at=stamp)
    return gate


def _bounded_text(value, label, limit, *, required=False):
    if not isinstance(value, str) or len(value) > limit or "\0" in value:
        raise ValueError(f"{label} must be text of at most {limit} characters")
    cleaned = _text(value) or ""
    if required and not cleaned:
        raise ValueError(f"{label} is required")
    return cleaned


def _answer(question, value):
    label = f"Answer for {question['id']}"
    multiple = question["multi_select"] and bool(question["options"])
    if multiple:
        if not isinstance(value, list) or not value:
            raise ValueError(f"{label} must contain at least one choice")
        answers = list(dict.fromkeys(_bounded_text(item, label, 8000, required=True) for item in value))
    else:
        answers = [_bounded_text(value, label, 8000, required=True)]
    choices = question["options"] or ([question["recommended"]] if question["recommended"] else [])
    if choices and not question["allow_note"] and any(answer not in choices for answer in answers):
        raise ValueError(f"{label} must use the choices in the packet")
    return answers if multiple else answers[0]


def _decision(gate, payload):
    responder = _bounded_text(payload.get("approved_by", "Reviewer"), "Responder name", 80, required=True)
    notes = _bounded_text(payload.get("notes", ""), "Notes", 8000)
    doc = {"approved_by": responder, "answered_at": _now(), "notes": notes, "source": "hero_town_hub"}
    summary = []
    if gate["questions"]:
        decisions = payload.get("decisions")
        if not isinstance(decisions, dict):
            raise ValueError("Decisions must be an object with an answer for each question")
        answers = {q["id"]: _answer(q, decisions.get(q["id"])) for q in gate["questions"]}
        doc.update(resolved=True, decisions=answers)
        summary += [f"{key}: {', '.join(answer) if isinstance(answer, list) else answer}"
                    for key, answer in answers.items()]

    if gate["gate_type"] in ("approval", "hybrid"):
        approved = payload.get("approved")
        if type(approved) is not bool:
            raise ValueError("Approval requires an approved boolean")
        fields = payload.get("approved_fields", {})
        if not isinstance(fields, dict):
            raise ValueError("Approval fields must be an object")
        for field in gate["approved_fields"]:
            key = field["key"]
            value = fields.get(key, payload.get(key, field["default"]))
            if type(value) is not bool:
                raise ValueError(f"{field['label']} must be a boolean")
            doc[key] = value
        doc["approved"] = approved
        if gate.get("review_sha256"):
            doc["review_sha256"] = gate["review_sha256"]
        if not approved:
            doc["rejected_by"] = responder
            rejected = payload.get("rejected_bugs", [])
            bug_ids = {bug["id"] for bug in gate["bugs"]}
            if (not isinstance(rejected, list)
                    or any(not isinstance(key, str) or key not in bug_ids for key in rejected)):
                raise ValueError("Rejected bugs must be listed in the review packet")
            if rejected:
                doc["rejected_bugs"] = list(dict.fromkeys(rejected))
        summary.insert(0, "Approved" if approved else "Fixes requested")
    clean_summary = " · ".join(summary)
    doc["summary"] = clean_summary[:1199] + "…" if len(clean_summary) > 1200 else clean_summary
    if len(json.dumps(doc, indent=2).encode()) >= _PACKET_LIMIT - 512:
        raise ValueError("The decision exceeds the size limit")
    return doc


def _optional_object(path):
    try:
        return _read_json(path)[0]
    except FileNotFoundError:
        return {}


def _subtask_path(value):
    if not isinstance(value, str):
        return None
    parent, _, item_id = value.partition("::")
    return (parent, item_id) if _valid_node_path(parent) and valid_id(item_id) else None


def _subtask_decision_path(state, item_id):
    path = state.parent / "artifacts" / f"{item_id}_decision.json"
    if path.is_symlink():
        raise PermissionError("A sub-task decision file cannot be a symlink")
    return validate_kgraph_output_path(str(path), state_path=state)


def read_subtask_answer(state_path, node_path, item_id) -> Optional[dict]:
    """Read the answer to the current question for this sub-task."""
    if not (_valid_node_path(node_path) and valid_id(item_id)):
        raise ValueError("Invalid sub-task path")
    state = Path(state_path).resolve()
    item = read_node_progress(state).get(node_path, {}).get("items", {}).get(item_id)
    if item is not None:
        if decision := _object(item.get("decision")):
            return decision
        if item.get("status") != "passed":
            return None
    try:
        doc = _read_json(_subtask_decision_path(state, item_id), limit=_PACKET_LIMIT)[0]
    except (OSError, ValueError, RuntimeError):
        return None
    if doc.get("node_path") != f"{node_path}::{item_id}":
        return None
    if item is not None:
        gate = extract_subtask_input_gate(node_path, {**item, "status": "awaiting_subagents"}, state_path=state)
        if gate is None or gate["packet_hash"] != doc.get("packet_hash"):
            return None
    return doc


def _report_segments(report):
    error = report.get("error")
    segments = re.findall(r"\[([^\[\]]+)\] ", error) if isinstance(error, str) else []
    declared = report.get("node_path")
    return [declared, *segments] if _valid_node_path(declared) else segments


def _match_report_node(paths, segments):
    # The runner names the failing leaf after any enclosing subgraphs.
    # Parallel failures list the packet from the first leaf in the error.
    paths = set(paths)
    prefixes = []
    for segment in segments:
        if segment in paths:
            return segment
        matches = [path for path in paths if path.endswith("/" + segment)]
        for prefix in reversed(prefixes):
            scoped = [path for path in matches if path.startswith(prefix + "/")]
            if scoped:
                matches = scoped
                break
        if len(matches) == 1:
            return matches[0]
        prefixes.append(segment)
    return None


def _active_gate(state, report, requested):
    records = {path: record for path, record in _object(state.get("nodes")).items()
               if _valid_node_path(path) and isinstance(record, dict)}
    containers = {path.rpartition("/")[0] for path in records if "/" in path}
    tasks = {path: record for path, record in records.items() if path not in containers}
    waiting = {path: record for path, record in tasks.items() if record.get("status") == "awaiting_subagents"}
    segments = _report_segments(report)
    active = _match_report_node(tasks, segments)
    if active is None and len(waiting) == 1:
        active = next(iter(waiting))
    target = requested or active
    if target in waiting:
        if target == active and report.get("packet_path"):
            return target, report["packet_path"]
        fallback = waiting[target].get("packet_path")
        if fallback is None and len(waiting) == 1:
            fallback = state.get("packet_path")
        if fallback or target == active:
            return target, fallback
    if not records and report.get("status") == "awaiting_subagents":
        declared = report.get("node_path") or next((part for part in reversed(segments) if _valid_node_path(part)), None)
        if _valid_node_path(declared) and (requested is None or requested == declared):
            return declared, report.get("packet_path")
    return None, None


def _resolve_packet(raw, state, report, project, *, watch=None):
    if not isinstance(raw, str) or not raw.strip() or "\0" in raw:
        raise ValueError("The active gate has no packet")
    path = Path(raw)
    _reject_path_traversal(path)
    root = _allowed_root(state)
    bases = [project, root.parent, report.parent, state.parent]
    candidates = [path] if path.is_absolute() else [base / path for base in bases if base is not None]
    if watch is not None:
        for candidate in candidates:
            watch(candidate)
    candidate = next((value for value in candidates if value.is_file()), candidates[0])
    resolved = _inside(candidate, root)
    if not resolved.is_file():
        raise ValueError("The active gate packet is unavailable")
    return candidate


def _local_run_state(run):
    raw = run.get("state_path")
    if not isinstance(raw, (str, Path)) or not raw:
        raise PermissionError("This operation requires a local run")
    # As with gate responses, only a locally registered report authorizes
    # using the state path when the current snapshot was pushed to the Hub.
    if run.get("source") == "remote" and not (run.get("report_path") and Path(raw).is_file()):
        raise PermissionError("This operation requires a local run")
    state = Path(raw).resolve()
    return state, _allowed_root(state)


def _resolve_log_path(raw, root, bases):
    if not isinstance(raw, str) or not raw.strip() or "\0" in raw:
        raise ValueError("A valid log path is required")
    path = Path(raw)
    _reject_path_traversal(path)
    if path.is_absolute():
        return _inside(path, root)

    # Engine outputs are relative to the project. Progress can also refer to
    # files relative to the run directory or the .kgraph root.
    candidates = [base / path for base in bases]
    candidate = next((item for item in candidates if item.exists() or item.is_symlink()), None)
    if candidate is None:
        candidate = next((item for item in candidates if item.is_relative_to(root)), None)
    if candidate is None:
        raise PermissionError("The log must be inside the workflow's .kgraph directory")
    return _inside(candidate, root)


def _recorded_output_paths(state, root):
    for value in _object(_optional_object(state).get("nodes")).values():
        record = _object(value)
        outputs = record.get("outputs")
        if isinstance(outputs, list):
            yield from outputs
        yield from _object(record.get("output_hashes"))
    progress_path = progress_path_for_state(state)
    try:
        progress = _read_progress_file(_inside(progress_path, root))
    except (OSError, ValueError, RuntimeError):
        return
    for entry in progress.values():
        for item in entry["items"].values():
            outputs = item.get("outputs")
            if isinstance(outputs, list):
                yield from outputs


def read_node_log_tail(run, raw_path, *, max_bytes=32768) -> tuple[int, dict]:
    """Read a bounded, redacted tail from a recorded local output."""
    if raw_path is None or (isinstance(raw_path, str) and not raw_path.strip()):
        return 400, {"error": "A log path is required"}
    try:
        limit = max(256, min(131072, int(max_bytes)))
    except (TypeError, ValueError, OverflowError):
        return 400, {"error": "max_bytes must be an integer"}
    try:
        state, root = _local_run_state(run)
        project = Path(run.get("project_dir") or root.parent).resolve()
        bases = tuple(dict.fromkeys((project, root.parent, state.parent, root)))
        resolved = _resolve_log_path(raw_path, root, bases)
        for output in _recorded_output_paths(state, root):
            try:
                if _resolve_log_path(output, root, bases) == resolved:
                    break
            except (OSError, ValueError, RuntimeError):
                continue
        else:
            return 403, {"error": "The log path is not a recorded output"}
        if not stat.S_ISREG(resolved.stat().st_mode):
            raise PermissionError("The log must be a regular file")
        flags = os.O_RDONLY | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0)
        flags |= getattr(os, "O_NOFOLLOW", 0)
        with os.fdopen(os.open(resolved, flags), "rb") as file:
            info = os.fstat(file.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise PermissionError("The log must be a regular file")
            size = info.st_size
            start = max(0, size - limit - 4096)
            file.seek(start)
            window = file.read(size - start)
            lookback = max(0, size - limit - start)
            decoder = getincrementaldecoder("utf-8")(errors="replace")
            prefix = decoder.decode(window[:lookback])
            content = prefix + decoder.decode(window[lookback:], final=True)
            cutoff = trim = len(prefix)
            replacement = "[redacted]"

            def redact(match):
                nonlocal trim
                start, end = match.span()
                if end <= cutoff:
                    trim += len(replacement) - (end - start)
                elif start < cutoff:
                    # Start at the placeholder when the cutoff crosses a credential.
                    trim -= cutoff - start
                return replacement

            content = _CREDENTIAL.sub(redact, content)[trim:][-limit:]
    except FileNotFoundError:
        return 404, {"error": "Log file not found"}
    except (OSError, ValueError, RuntimeError):
        return 403, {"error": "The log must be a recorded regular file inside the workflow's .kgraph directory"}
    return 200, {"path": str(resolved), "name": resolved.name, "content": content,
                 "size_bytes": size, "truncated": size > limit}


def _find_qa_checklist(state, root, run):
    def checked(path):
        try:
            resolved = _inside(path, root)
            # An implicit checklist directory must not alias another run's files.
            if resolved == path and not path.is_symlink() and resolved.is_file():
                return resolved
        except (OSError, RuntimeError):
            pass
        return None

    report = run.get("report_path")
    if isinstance(report, (str, Path)) and report:
        report = Path(report)
        try:
            declared = _optional_object(report).get("qa_checklist_path")
        except (OSError, ValueError, RecursionError):
            declared = None
        if isinstance(declared, str) and declared.strip() and "\0" not in declared:
            path = Path(declared)
            try:
                _reject_path_traversal(path)
            except PermissionError:
                pass
            else:
                bases = (root.parent, report.resolve().parent, state.parent, root)
                candidates = (path,) if path.is_absolute() else (base / path for base in bases)
                for candidate in candidates:
                    if resolved := checked(candidate):
                        return resolved

    preferred = [_run_qa_checklist(state)]
    if state.parent != root:
        preferred.extend((state.parent / "review" / "manual_qa.json", state.parent / "manual_qa.json"))
    for candidate in preferred:
        if resolved := checked(candidate):
            return resolved
    return None


def find_run_qa_checklist(run) -> Optional[Path]:
    """Find the first safe checklist for a local run."""
    try:
        state, root = _local_run_state(run)
        return _find_qa_checklist(state, root, run)
    except (OSError, ValueError, RuntimeError):
        return None


def read_run_qa(run, *, can_edit=False) -> tuple[int, dict]:
    """Return the run's current manual QA scorecard."""
    checklist = find_run_qa_checklist(run)
    if checklist is None:
        return 200, {"available": False}
    try:
        root = _allowed_root(run["state_path"])
        scorecard = compute_qa_scorecard(_read_json(checklist)[0])
    except FileNotFoundError:
        return 200, {"available": False}
    except PermissionError as exc:
        return 403, {"error": str(exc)}
    except (ValueError, RecursionError) as exc:
        return 400, {"error": str(exc)}
    return 200, {"available": True, "can_edit": bool(can_edit),
                 "checklist_path": str(checklist.relative_to(root)), "scorecard": scorecard,
                 "manual_qa_values": list(MANUAL_QA_VALUES), "gap_types": [gap for gap in GAP_TYPES if gap is not None]}


def submit_run_qa(run, payload) -> tuple[int, dict]:
    """Record a manual QA result and regenerate the checklist's scorecards."""
    if not isinstance(payload, dict):
        return 400, {"error": "Payload must be a JSON object"}
    checklist = find_run_qa_checklist(run)
    if checklist is None:
        return 404, {"error": "QA checklist not found"}
    try:
        attempt = payload.get("resolved_on_attempt")
        if attempt is not None and (type(attempt) is not int or attempt < 1):
            raise ValueError("resolved_on_attempt must be a positive integer")
        lock_path = checklist.with_name(checklist.name + ".lock")
        if lock_path.is_symlink() or (lock_path.exists() and not lock_path.is_file()):
            raise PermissionError("The QA checklist lock must be a regular file")
        record_qa_result(checklist, case_id=payload.get("case_id"), manual_qa=payload.get("manual_qa"),
                         gap_type=payload.get("gap_type"), notes=payload.get("notes"),
                         resolved_on_attempt=attempt, tester=payload.get("tester"))
        scorecard = write_qa_scorecard(checklist)
    except FileNotFoundError:
        return 404, {"error": "QA checklist not found"}
    except PermissionError as exc:
        return 403, {"error": str(exc)}
    except (ValueError, RecursionError) as exc:
        return 400, {"error": str(exc)}
    return 200, {"ok": True, "available": True, "can_edit": True, "scorecard": scorecard}


def _response_path(run_id):
    from .registry import kgraph_home

    if (not valid_id(run_id) or len(run_id) > 128
            or re.fullmatch(r"con|prn|aux|nul|(?:com|lpt)[0-9]", run_id, re.IGNORECASE)):
        raise ValueError("Invalid run ID")
    return kgraph_home() / "responses" / f"{run_id}.json"


def read_remote_response(run_id: str) -> Optional[dict]:
    """Read the latest queued decision for an origin host."""
    try:
        return _read_json(_response_path(run_id), limit=_PACKET_LIMIT)[0]
    except (ValueError, OSError, RecursionError):
        return None


def _remote_subtask(nodes, parent_path, item_id):
    pending = list(nodes) if isinstance(nodes, list) else []
    while pending:
        node = _object(pending.pop())
        if node.get("path") == parent_path and node.get("status") in _ACTIVE_SUBTASK_PARENTS:
            graph = _object(node.get("sub_graph"))
            items = graph.get("nodes")
            if isinstance(items, list):
                for item in items:
                    if _object(item).get("path") == f"{parent_path}::{item_id}":
                        return node, item
        children = node.get("children")
        if isinstance(children, list):
            pending.extend(children)
    return {}, {}


def _remote_gate(snapshot, requested=None):
    attention = _object(snapshot.get("attention"))
    gate = _object(attention.get("input_gate"))
    target = requested or gate.get("node_path")
    if _subtask_path(gate.get("node_path")) and target == gate.get("parent_node_path"):
        target = gate["node_path"]
    top_gate = gate.get("node_path") == attention.get("node_path") == target
    if (top_gate and _valid_node_path(target) and attention.get("kind") in ("decision", "waiting")
            and snapshot.get("status") == "awaiting_subagents"):
        return gate
    if path := _subtask_path(target):
        if top_gate and attention.get("kind") == "decision" and snapshot.get("status") in _ACTIVE_SUBTASK_PARENTS:
            return gate
        _, item = _remote_subtask(snapshot.get("nodes"), *path)
        child_gate = _object(item.get("input_gate"))
        if (child_gate.get("node_path") == target and
                (item.get("status") == "awaiting_subagents" or child_gate.get("submitted"))):
            return child_gate
    return {}


def _mark_remote_response(snapshot, gate, doc):
    from .snapshot import _subtask_attention

    if not (path := _subtask_path(gate["node_path"])):
        gate["submitted"] = _submission(doc)
        return
    submitted = _subtask_submission(doc)
    gate["submitted"] = submitted
    parent, item = _remote_subtask(snapshot.get("nodes"), *path)
    if not item or _object(item.get("input_gate")).get("packet_hash") != gate.get("packet_hash"):
        return
    if item.get("status") == "awaiting_subagents":
        for counts, waiting, done in ((parent.get("sub_progress"), "waiting", "done"),
                                      (parent["sub_graph"].get("counts"), "awaiting_subagents", "passed")):
            if isinstance(counts, dict) and type(counts.get(waiting)) is int and type(counts.get(done)) is int:
                counts[waiting] = max(0, counts[waiting] - 1)
                counts[done] += 1
    item["input_gate"]["submitted"] = submitted
    item.update(status="passed", progress=100, detail=f"Answered by {doc['approved_by']}: {doc['summary']}",
                submitted=submitted, decision_summary=doc["summary"], updated_at=doc["answered_at"])
    if _object(snapshot.get("attention")).get("node_path") == gate["node_path"]:
        if attention := _subtask_attention(snapshot.get("nodes")):
            snapshot["attention"] = attention


def _apply_remote_response(snapshot, run_id):
    # A pushed progress snapshot can arrive after the Hub queues the decision.
    # Only show a queued answer when it belongs to this exact gate.
    doc = read_remote_response(run_id)
    if doc and (gate := _remote_gate(snapshot, doc.get("node_path"))):
        if (doc.get("node_path") == gate["node_path"] and doc.get("packet_hash") == gate.get("packet_hash")
                and doc.get("answered_at", "") > (_object(gate.get("submitted")).get("answered_at") or "")):
            _mark_remote_response(snapshot, gate, doc)
    return snapshot


def _check_gate(gate, payload):
    if "packet_hash" in payload and payload["packet_hash"] != gate.get("packet_hash"):
        return "The gate changed. Refresh and review the current packet before submitting"
    answered_at = _object(gate.get("submitted")).get("answered_at")
    if "submitted_at" in payload and payload["submitted_at"] != answered_at:
        return "A newer decision was submitted. Refresh before editing it"
    return None


def _result(doc, node_path, delivery):
    return 200, {"ok": True, "delivery": delivery, "node_path": node_path,
                 "answered_at": doc["answered_at"], "summary": doc["summary"], "decision": doc}


def _submit_local_subtask(state, report, requested, payload):
    node_path, item_id = _subtask_path(requested)
    progress = progress_path_for_state(state)
    with file_lock(progress.with_suffix(".lock")):
        records = _object(_optional_object(state).get("nodes"))
        data = read_node_progress(state)
        item = data.get(node_path, {}).get("items", {}).get(item_id)
        inactive = {"error": "The requested sub-task is no longer awaiting input"}
        if _object(records.get(node_path)).get("status") not in _ACTIVE_SUBTASK_PARENTS or item is None:
            return 409, inactive
        submitted_at = _object(item.get("submitted")).get("answered_at")
        if item.get("status") != "awaiting_subagents" and not (
                item.get("status") == "passed" and submitted_at and submitted_at == payload.get("submitted_at")):
            return 409, inactive
        gate = extract_subtask_input_gate(node_path, {**item, "status": "awaiting_subagents"}, state_path=state)
        if gate is None:
            return 409, inactive
        if error := _check_gate(gate, payload):
            return 409, {"error": error}
        output = _subtask_decision_path(state, item_id)
        if output in {progress.resolve(), report.resolve() if report else None}:
            raise PermissionError("The gate requires a separate JSON response file")
        doc = {**_decision(gate, payload), "node_path": requested, "packet_hash": gate["packet_hash"]}
        if not (item.get("question") or item.get("questions")):
            item["questions"] = gate["questions"]
        outputs = item.get("outputs")
        item.update(submitted=_subtask_submission(doc), decision=doc, decision_summary=doc["summary"],
                    status="passed", progress=100,
                    detail=f"Answered by {doc['approved_by']}: {doc['summary']}", updated_at=doc["answered_at"],
                    outputs=list(dict.fromkeys([*(outputs if isinstance(outputs, list) else []), str(output)])))
        write_json(output, doc)
        write_json(progress, data)
        status, result = _result(doc, requested, "local_subtask")
        return status, {**result, "output_path": str(output)}


def _submit_elevated_subtask(state, report, project, requested, payload):
    from .snapshot import _snapshot_from_graph

    snapshot = _snapshot_from_graph(None, state, report, project_dir=project)
    gate = _object(_object(snapshot.get("attention")).get("input_gate"))
    target = gate.get("node_path")
    if _subtask_path(target) and requested in (None, gate.get("parent_node_path")):
        return _submit_local_subtask(state, report, target, payload)
    return None


def _submit_remote(run, payload):
    from .registry import _locked_registry

    response_path = _response_path(run["id"])
    with _locked_registry():
        snapshot_path = Path(run["remote_snapshot_path"])
        snapshot = _apply_remote_response(_read_json(snapshot_path)[0], run["id"])
        gate = _remote_gate(snapshot, payload.get("node_path"))
        node_path = gate.get("node_path")
        if not gate:
            return 409, {"error": "The requested node is no longer awaiting input"}
        if gate.get("can_respond") is not True:
            return 403, {"error": "This gate cannot receive a response"}
        if error := _check_gate(gate, payload):
            return 409, {"error": error}
        if (_subtask_path(node_path) and (stamp := _object(gate.get("submitted")).get("answered_at"))
                and payload.get("submitted_at") != stamp):
            return 409, {"error": "A newer decision was submitted. Refresh before editing it"}
        if gate.get("gate_type") not in ("questions", "approval", "hybrid"):
            return 400, {"error": "Active node does not accept interactive input"}
        # Reuse the packet normalizers for the public schema. The remote origin
        # still owns validation of its destination when it applies the decision.
        gate["questions"] = _questions(gate, None)
        gate["approved_fields"] = _approval_fields(gate, None)
        gate["bugs"] = _bugs(gate, None)
        gate["review_sha256"] = _text(gate.get("review_sha256"))
        if gate["gate_type"] in ("questions", "hybrid") and not gate["questions"]:
            return 400, {"error": "Active node does not accept interactive input"}
        doc = _decision(gate, payload)
        write_json(response_path, {**doc, "node_path": node_path, "packet_hash": gate.get("packet_hash")})
        _mark_remote_response(snapshot, gate, doc)
        write_json(snapshot_path, snapshot, indent=None)
        return _result(doc, node_path, "remote_queue")


def submit_gate_response(run, payload, *, snapshot=None) -> tuple[int, dict]:
    """Deliver a decision using current telemetry, even if a snapshot was supplied."""
    if not isinstance(payload, dict):
        return 400, {"error": "Payload must be a JSON object"}
    requested = payload.get("node_path")
    subtask = _subtask_path(requested)
    if requested is not None and not (_valid_node_path(requested) or subtask):
        return 400, {"error": "Invalid node path"}
    raw_state, raw_report = run.get("state_path"), run.get("report_path")
    # Only a registered local report can authorize writes on the Hub host.
    # The paths carried by an ingest-only run describe the origin host.
    try:
        if run.get("source") == "remote" and not (raw_state and raw_report and Path(raw_state).is_file()):
            return _submit_remote(run, payload)
        if not raw_state:
            return 409, {"error": "The requested node is no longer awaiting input"}
        state = Path(raw_state).resolve()
        _allowed_root(state)
        report = Path(raw_report) if raw_report else None
        project = Path(run["project_dir"]) if run.get("project_dir") else None
        if subtask:
            return _submit_local_subtask(state, report, requested, payload)
        if report is None:
            return (_submit_elevated_subtask(state, report, project, requested, payload) or
                    (409, {"error": "The requested node is no longer awaiting input"}))
        with file_lock(state.with_name(f"{state.stem}_response.lock")):
            telemetry, overview = _optional_object(state), _optional_object(report)
            node_path, raw_packet = _active_gate(telemetry, overview, requested)
            if not raw_packet:
                if result := _submit_elevated_subtask(state, report, project, requested, payload):
                    return result
            if node_path is None:
                return 409, {"error": "The requested node is no longer awaiting input"}
            if not raw_packet:
                return 400, {"error": "Active node does not accept interactive input"}
            try:
                packet_path = _resolve_packet(raw_packet, state, report, project)
                packet = _read_json(packet_path, limit=_PACKET_LIMIT)[0]
            except (FileNotFoundError, ValueError, RecursionError):
                if result := _submit_elevated_subtask(state, report, project, requested, payload):
                    return result
                raise
            gate = extract_input_gate(packet, packet_path, node_path=node_path, state_path=state, project_dir=project)
            if gate is None:
                return (_submit_elevated_subtask(state, report, project, requested, payload) or
                        (400, {"error": "Active node does not accept interactive input"}))
            output = validate_kgraph_output_path(_output_path(packet), state_path=state, packet_path=packet_path,
                                                project_dir=project)
            if output == report.resolve() or output == progress_path_for_state(state).resolve():
                raise PermissionError("The gate requires a separate JSON response file")
            if error := _check_gate(gate, payload):
                return 409, {"error": error}
            doc = _decision(gate, payload)
            write_json(output, doc)
            return _result(doc, node_path, "local")
    except PermissionError as exc:
        return 403, {"error": str(exc)}
    except (ValueError, RecursionError) as exc:
        return 400, {"error": str(exc)}
    except RuntimeError:
        return 403, {"error": "The packet or response path could not be resolved safely"}
    except OSError:
        return 500, {"error": "The gate response could not be stored"}
