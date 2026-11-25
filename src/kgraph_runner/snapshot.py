"""Build a serializable view of workflow topology and persisted run telemetry."""

from collections import defaultdict
from datetime import datetime, timezone
import json
from pathlib import Path
import re

from .interactions import (
    _ACTIVE_SUBTASK_PARENTS, _CREDENTIAL, _PACKET_LIMIT, _TASK_KINDS,
    _match_report_node, _read_json, _reject_constant, _report_segments, _resolve_packet, _wave_edges,
    build_node_sub_graph, extract_input_gate, read_node_progress,
)
from .workflow import _load_workflow, _require_object, _resolve_context, _resolve_graph


_CONTAINERS = {"subgraph", "loop_subgraph", "splitter"}
_STATUSES = (
    "running", "circuit_broken", "budget_exceeded", "failed", "awaiting_subagents", "pending", "passed", "cached",
)
_ATTENTION_KINDS = {
    "running": "running",
    "awaiting_subagents": "waiting",
    "circuit_broken": "stuck",
    "budget_exceeded": "stuck",
    "failed": "stuck",
    "passed": "done",
    "cached": "done",
}


def _object(value):
    return value if isinstance(value, dict) else {}


def _array(value):
    return value if isinstance(value, (list, tuple)) else []


def _timestamp(value):
    try:
        stamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return stamp.astimezone(timezone.utc) if stamp.tzinfo else stamp.replace(tzinfo=timezone.utc)
    except (AttributeError, ValueError, OverflowError):
        return datetime.min.replace(tzinfo=timezone.utc)


def _read_telemetry(path, warnings):
    if path is None:
        return {}
    try:
        with path.open(encoding="utf-8") as file:
            return _require_object(json.load(file, parse_constant=_reject_constant), str(path))
    except FileNotFoundError:
        warnings.append(f"Waiting for {path}")
    except (OSError, ValueError, RecursionError) as exc:
        warnings.append(f"Could not read {path}: {exc}")
    return {}


def _attention_text(value, limit=200):
    if not isinstance(value, str) or _CREDENTIAL.search(value):
        return None
    value = " ".join(value.split())
    if len(value) > limit:
        prefix, _, _ = value[:limit].rpartition(" ")
        return (prefix if len(prefix) >= limit // 2 else value[:limit - 1]) + "…"
    return value or None


def _read_attention_packet(value, *, state_file, report_file, project_dir):
    if state_file is None or report_file is None:
        return None, {}
    try:
        path = _resolve_packet(value, state_file.resolve(), report_file.resolve(), project_dir)
        return str(path), _read_json(path, limit=_PACKET_LIMIT)[0]
    except (OSError, ValueError, RuntimeError):
        return None, {}


def _attention_candidates(nodes, kind):
    matches = []
    for node in nodes:
        children = _attention_candidates(node["children"], kind)
        matches.extend(children or ([node] if _ATTENTION_KINDS.get(node["status"]) == kind else []))
    return matches


def _question_previews(gate):
    items = []
    for value in _array(gate.get("questions"))[:6]:
        question = _object(value)
        ticket = _attention_text(question.get("ticket"), 80)
        if preview := _attention_text(question.get("question"), 180):
            if ticket:
                preview = _attention_text(f"[{ticket}] {preview}", 180)
            items.append(preview)
    return items


def _pending_subtask_inputs(nodes):
    for value in _array(nodes):
        node = _object(value)
        if node.get("status") in _ACTIVE_SUBTASK_PARENTS:
            for entry in _array(_object(node.get("sub_graph")).get("nodes")):
                subtask = _object(entry)
                gate = _object(subtask.get("input_gate"))
                if subtask.get("status") == "awaiting_subagents" and gate and not gate.get("submitted"):
                    yield subtask
        yield from _pending_subtask_inputs(node.get("children"))


def _subtask_attention(nodes):
    subtask = max(_pending_subtask_inputs(nodes), key=lambda item: _timestamp(item.get("updated_at")), default=None)
    if subtask is None:
        return None
    gate = subtask["input_gate"]
    fallback = str(subtask.get("id", "")).replace("_", " ").replace("-", " ").capitalize()
    return {"kind": "decision", "headline": subtask.get("title") or fallback,
            "detail": _attention_text(subtask.get("detail")), "node_path": gate.get("node_path"),
            "input_gate": gate, "items": _question_previews(gate), "packet_path": None}


def _build_attention(status, nodes, report, use_report, *, state_file, report_file, project_dir):
    kind = _ATTENTION_KINDS.get(status)
    if kind in (None, "done"):
        if attention := _subtask_attention(nodes):
            return attention
        if kind is None:
            return None
        if _attention_candidates(nodes, None):
            return None
        return {"kind": "done", "headline": "Workflow complete", "detail": None,
                "node_path": None, "items": [], "packet_path": None}

    error = report.get("error") if use_report else None
    segments = _report_segments(report) if use_report else []
    packet_path, packet = (None, {})
    if use_report and kind in ("waiting", "decision", "stuck"):
        packet_path, packet = _read_attention_packet(report.get("packet_path"), state_file=state_file,
                                                     report_file=report_file, project_dir=project_dir)
    stage = _attention_text(packet.get("stage"), 80)
    bug = _attention_text(packet.get("bug_id"), 80)
    candidates = _attention_candidates(nodes, kind)
    by_path = {node["path"]: node for node in candidates}
    node = by_path.get(_match_report_node(by_path, segments))
    if node is None:
        node = max(candidates, key=lambda item: (
            bug in item["path"].split("/"), item["id"] == stage,
            -segments.index(item["id"]) if item["id"] in segments else -len(segments),
            _timestamp(item["updated_at"]),
        ), default={})
    fallback = node.get("id", "").replace("_", " ").strip().capitalize()
    detail = _attention_text(node.get("detail")) or _attention_text(node.get("error_signature"))
    if not detail and isinstance(error, str):
        detail = _attention_text(re.sub(r"^\[[^\]]+\] ", "", error, count=1))

    title = _attention_text(packet.get("title") or _object(packet.get("bug_spec")).get("title"), 130)
    headline = title or bug or (stage.replace("_", " ").capitalize() if stage else fallback)
    if title and bug and bug != title:
        headline = f"{bug} · {title}"
    summary, reason = (_attention_text(packet.get(key)) for key in ("summary", "reason"))
    detail = (reason or detail or summary) if kind == "stuck" else (summary or reason or detail)
    items = [line for item in _array(packet.get("instructions"))[:3] if (line := _attention_text(item, 240))]
    for item in _array(packet.get("bugs"))[:8]:
        if title := _attention_text(_object(item).get("title") if isinstance(item, dict) else item, 130):
            items.append(title)

    review_gate = stage == "user_verify_gate" or node.get("id") == "user_verify_gate"
    approval = packet.get("approval_path")
    if review_gate and not approval:
        outputs = _array(packet.get("outputs"))
        approval = outputs[0] if outputs else None
    input_gate = None
    if packet and kind in ("waiting", "decision"):
        input_gate = extract_input_gate(packet, packet_path, node_path=node.get("path"), state_path=state_file,
                                        project_dir=project_dir, allow_respond=state_file is not None)
    if input_gate is None:
        if attention := _subtask_attention(nodes):
            return attention
    if kind == "waiting" and (input_gate or review_gate or approval or packet.get("approved_fields")):
        kind = "decision"
    review = _attention_text(packet.get("review_summary_path") or packet.get("review_summary"), 160)
    if review:
        items.append(f"Review {review}")
    if approval := _attention_text(approval, 160):
        items.append(f"Record the decision in {approval}")
    default_headline = {"running": "Workflow running", "waiting": "Workflow paused",
                        "stuck": "Workflow blocked", "decision": "Approval needed"}
    attention = {"kind": kind, "headline": headline or default_headline[kind], "detail": detail,
                 "node_path": node.get("path"), "items": items,
                 "packet_path": packet_path if _attention_text(packet_path) else None}
    if input_gate:
        attention["input_gate"] = input_gate
        if not items:
            items.extend(_question_previews(input_gate))
    return attention


def _cached_paths(report, records):
    # The wrapper's reports record the latest cache event per path, including
    # when a path executes more than once during a repair loop.
    if "cached_nodes" in report:
        return set(path for path in _array(report["cached_nodes"]) if isinstance(path, str))

    # Older engine reports keep aggregate event counts and at most five timed
    # entries. Infer cache hits only when those records identify them uniquely.
    summary = _object(report.get("summary"))
    reached = {path for path in _array(_object(report.get("scope")).get("reached_nodes"))
               if isinstance(path, str)}
    if not summary.get("cached") or summary.get("total_events") != len(reached):
        return set()
    timed = {entry.get("node") for entry in _array(report.get("slowest_nodes"))
             if isinstance(entry, dict) and isinstance(entry.get("node"), str)}
    candidates = {
        path for path in reached - timed
        if records.get(path, {}).get("status") in ("passed", "cached")
        and records[path].get("kind") not in _CONTAINERS
    }
    return candidates if len(candidates) == summary["cached"] else set()


def _aggregate(nodes):
    statuses = {node["status"] for node in nodes}
    for status in _STATUSES[:-2]:
        if status in statuses:
            return status
    if "passed" in statuses:
        return "passed"
    return "cached" if statuses else "pending"


def _record_budget_block(records, report):
    # The engine checks its budget before recording a worker attempt. Its
    # final report identifies the blocked task even when state has no entry.
    error = report.get("error")
    if report.get("status") != "budget_exceeded" or not isinstance(error, str) or not error.startswith("["):
        return
    path, separator, detail = error[1:].partition("] ")
    if not separator or not all(path.split("/")):
        return
    generated_at = report.get("generated_at")
    report_time = _timestamp(generated_at)
    if any(_timestamp(record.get("updated_at") or record.get("started_at")) > report_time
           for record in records.values()):
        return
    records[path] = {"kind": "worker", **records.get(path, {}), "status": "budget_exceeded",
                     "detail": detail, "updated_at": generated_at}


def _retry_edge(defined, children, max_retries):
    by_id = {child["id"]: child for child in children}
    source = by_id.get(getattr(defined, "retry_from", None))
    target = by_id.get(getattr(defined, "retry_to", None))
    inferred_source = max(
        enumerate(children),
        key=lambda pair: (pair[1]["wave"], pair[1]["kind"] in ("gate", "verifier"), pair[0]),
    )[1]
    by_wave = sorted(children, key=lambda child: child["wave"])
    candidates = [child for child in by_wave if child["kind"] in ("worker", "code")
                  or child["node_type"] in ("splitter", "loop_subgraph")]
    repair_steps = [child for child in candidates if child["kind"] != "code"]
    inferred_target = (repair_steps or candidates or by_wave)[0]
    if len(children) >= 2 and inferred_source is inferred_target:
        inferred_source, inferred_target = children[-1], children[0]
    if len(children) >= 2:
        if source is inferred_target and target is None:
            inferred_target = next(child for child in by_wave if child is not source)
        if target is inferred_source and source is None:
            inferred_source = next(child for child in reversed(by_wave) if child is not target)
    source, target = source or inferred_source, target or inferred_target
    default = "if fails → retry" if max_retries is None else f"if fails → retry (max {max_retries})"
    return {"from": source["path"], "to": target["path"], "kind": "retry",
            "label": getattr(defined, "retry_label", None) or default}


def _split_branches(node, ctx, path, warnings, child_types):
    try:
        items = list(node.items_fn(ctx))
    except Exception as exc:
        warnings.append(f"{path} items are unavailable: {exc}")
        return []
    children = []
    for index, item in enumerate(items):
        try:
            child = node.child_builder_fn(item, ctx)
            if not isinstance(child, child_types):
                raise TypeError(f"Unsupported child type: {type(child).__name__}")
        except Exception as exc:
            warnings.append(f"{path} branch {index + 1} is unavailable: {exc}")
        else:
            children.append(child)
    return children


class _SnapshotTree:
    def __init__(self, records, report, ctx, warnings, node_types, progress, state_file):
        self.records = records
        self.report_time = _timestamp(report.get("generated_at"))
        self.cached = _cached_paths(report, records)
        self.ctx = ctx
        self.warnings = warnings
        self.node_types = node_types
        self.progress = progress
        self.state_file = state_file
        self.paths = defaultdict(dict)
        for path in records.keys() | {key for key, entry in progress.items() if entry["items"]}:
            parent = ""
            for part in path.split("/"):
                child = f"{parent}/{part}" if parent else part
                self.paths[parent][part] = child
                parent = child
        self.counts = dict.fromkeys(_STATUSES, 0)

    def level(self, defined, parent="", *, include_recorded=False, parallel=False):
        by_id = {str(node.id): node for node in defined}
        if include_recorded:
            for node_id in sorted(self.paths.get(parent, {})):
                by_id.setdefault(node_id, None)
        nodes = [
            self.node(node_id, node, f"{parent}/{node_id}" if parent else node_id, include_recorded)
            for node_id, node in by_id.items()
        ]
        return nodes, [] if parallel else _wave_edges(nodes, self.warnings, parent)

    def node(self, node_id, defined, path, include_recorded):
        record = self.records.get(path, {})
        recorded_kind = record.get("kind")
        if defined is None:
            node_type = record.get("node_type", recorded_kind)
            if not isinstance(node_type, str) or node_type not in _CONTAINERS:
                node_type = "subgraph" if path in self.paths else "task"
        else:
            node_type = next((name for cls, name in self.node_types if isinstance(defined, cls)), None)
            if node_type is None:
                raise TypeError(f"Unsupported node type at {path}: {type(defined).__name__}")

        kind = node_type if node_type != "task" else getattr(defined, "kind", recorded_kind)
        if not isinstance(kind, str) or kind not in _TASK_KINDS | _CONTAINERS:
            kind = "code"
        children, edges = [], []
        max_retries = getattr(defined, "max_retries", record.get("max_retries"))
        if node_type in _CONTAINERS:
            branches = getattr(defined, "nodes", ())
            if node_type == "splitter" and defined is not None:
                child_types = tuple(cls for cls, name in self.node_types if name != "splitter")
                branches = _split_branches(defined, self.ctx, path, self.warnings, child_types)
            children, edges = self.level(
                branches, path, include_recorded=include_recorded or node_type == "splitter",
                parallel=node_type == "splitter",
            )
            if node_type == "loop_subgraph" and children:
                edges.append(_retry_edge(defined, children, max_retries))

        status = record.get("status")
        if status not in _STATUSES:
            status = _aggregate(children)
        timestamp = record.get("updated_at") or record.get("started_at")
        if status == "passed" and path in self.cached and _timestamp(timestamp) <= self.report_time:
            status = "cached"
        if node_type != "task" and children:
            latest_child = max(children, key=lambda child: _timestamp(child["updated_at"]))
            if _timestamp(latest_child["updated_at"]) > _timestamp(timestamp):
                timestamp = latest_child["updated_at"]
                status = _aggregate(children)
                if node_type == "loop_subgraph" and status in ("passed", "cached"):
                    status = "running"

        self.counts[status] += 1
        node = {
            "id": node_id,
            "path": path,
            "node_type": node_type,
            "kind": kind,
            "deps": [dep for dep in _array(getattr(defined, "deps", record.get("deps"))) if isinstance(dep, str)],
            "wave": 0,
            "resource": getattr(defined, "resource", record.get("resource")),
            "max_retries": max_retries,
            "timeout_s": getattr(defined, "timeout_s", record.get("timeout_s")),
            "cacheable": getattr(defined, "cacheable", record.get("cacheable")),
            "max_workers": getattr(defined, "max_workers", record.get("max_workers")),
            "status": status,
            "attempts": record.get("attempts", 0),
            "duration_s": record.get("duration_s"),
            "input_hash": record.get("input_hash"),
            "outputs": _array(record.get("outputs")),
            "output_hashes": _object(record.get("output_hashes")),
            "error_signature": record.get("error_signature"),
            "detail": record.get("detail"),
            "started_at": record.get("started_at"),
            "updated_at": timestamp,
            "children": children,
            "edges": edges,
        }
        if path in self.progress:
            graph, summary = build_node_sub_graph(path, self.progress[path], self.warnings, state_path=self.state_file,
                                                  allow_respond=status in _ACTIVE_SUBTASK_PARENTS)
            if graph:
                node.update(sub_graph=graph, sub_progress=summary)
        return node


def build_town_snapshot(workflow_path=None, state_path=None, report_path=None,
                        factory="build_graph", ctx_inline=None, ctx_file=None, title=None) -> dict:
    """Read the workflow definition, run files, or both for the town visualizer."""
    if workflow_path is None and state_path is None:
        raise ValueError("Provide a workflow or a state file")
    if workflow_path is None:
        return _snapshot_from_graph(None, state_path, report_path, title=title)
    workflow = Path(workflow_path).resolve()
    with _load_workflow(workflow) as module:
        ctx = _resolve_context(module, ctx_inline, ctx_file)
        return _snapshot_from_graph(workflow, state_path, report_path, title=title,
                                    definition=(_resolve_graph(module, factory, ctx), ctx))


def _snapshot_from_graph(workflow, state_path, report_path, *, title=None, definition=None,
                         status_override=None, project_dir=None):
    state_file = Path(state_path) if state_path is not None else None
    report_file = Path(report_path) if report_path is not None else None
    warnings = []
    # The runner publishes the report after writing its final state.
    report = _read_telemetry(report_file, warnings)
    state = _read_telemetry(state_file, warnings)
    records = {
        path: entry for path, entry in _object(state.get("nodes")).items()
        if all(path.split("/")) and isinstance(entry, dict)
    }
    _record_budget_block(records, report)
    progress = read_node_progress(state_file, warnings) if state_file is not None else {}
    if definition is None:
        tree = _SnapshotTree(records, report, {}, warnings, (), progress, state_file)
        nodes, edges = tree.level((), include_recorded=True)
    else:
        from .engine import LoopSubgraphNode, SplitterNode, SubgraphNode, TaskNode
        node_types = (
            (LoopSubgraphNode, "loop_subgraph"), (SplitterNode, "splitter"),
            (SubgraphNode, "subgraph"), (TaskNode, "task"),
        )
        graph, ctx = definition
        tree = _SnapshotTree(records, report, ctx, warnings, node_types, progress, state_file)
        nodes, edges = tree.level(graph)

    latest = max((_timestamp(node["updated_at"]) for node in nodes), default=tree.report_time)
    status = _aggregate(nodes)
    if status != "running" and latest <= tree.report_time and isinstance(report.get("status"), str):
        status = report["status"]
    status = status_override or status
    workflow_ref = str(workflow) if workflow else report.get("workflow_ref") or state.get("workflow_ref")
    if not isinstance(workflow_ref, str):
        workflow_ref = None
    project = Path(project_dir) if project_dir is not None else Path(workflow_ref).parent if workflow_ref else None
    use_report = report.get("status") == status and latest <= tree.report_time
    return {
        "version": 1,
        "title": title or (Path(workflow_ref).stem if isinstance(workflow_ref, str) else state_file.stem),
        "status": status,
        "counts": tree.counts,
        "nodes": nodes,
        "edges": edges,
        "attention": _build_attention(status, nodes, report, use_report, state_file=state_file,
                                      report_file=report_file, project_dir=project),
        "summary": _object(report.get("summary")),
        "scope": _object(report.get("scope")),
        "failure_signatures": _array(report.get("failure_signatures")),
        "slowest_nodes": _array(report.get("slowest_nodes")),
        "evolution_hints": _array(report.get("evolution_hints")),
        "workflow_ref": workflow_ref,
        "state_file": str(state_file) if state_file is not None else None,
        "report_file": str(report_file) if report_file is not None else None,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "warning": " • ".join(warnings) or None,
    }
