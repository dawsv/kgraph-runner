"""Manual re-test checklists, scorecards, and batch history."""

from collections import Counter, defaultdict
from datetime import datetime, timezone
from html import escape
import json
import os
from pathlib import Path

from ._files import file_lock, read_object, write_json, write_text

MANUAL_QA_VALUES = ("pass", "fail", "blocked", "not_tested", "skipped")
GAP_TYPES = (None, "missing_verifier", "logic_bug", "ui_regression", "spec_changed", "env_blocker", "other")
AUTOMATED_STATUSES = ("pass", "fail", "pending", "blocked")
_DEFAULT_TESTER = "Daniel"
_MANUAL_FIELDS = ("manual_qa", "resolved_on_attempt", "gap_type", "notes")
_CASE_IDENTITY = ("title", "platform", "kind", "ai_attempts")
_MARKDOWN_ESCAPES = {ord(char): f"&#{ord(char)};" for char in "\\|`*[]~"}
_RUN_STATUSES = {
    "passed": "pass", "cached": "pass", "failed": "fail", "circuit_broken": "fail",
    "awaiting_subagents": "blocked", "budget_exceeded": "blocked",
}


def _now():
    return datetime.now(timezone.utc).isoformat()


def _required_text(value, name):
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} must be non-empty text")
    return value.strip()


def _optional_text(value, name):
    if value is not None and not isinstance(value, str):
        raise ValueError(f"{name} must be text or null")
    return value


def _choice(value, allowed, name):
    if value not in allowed:
        raise ValueError(f"Invalid {name} {value!r}. Expected one of {allowed}")
    return value


def _integer(value, minimum, name):
    if type(value) is not int or value < minimum:
        raise ValueError(f"{name} must be an integer of at least {minimum}")
    return value


def _normalize_case(case):
    if not isinstance(case, dict):
        raise ValueError("Each QA case must be a JSON object")
    case_id = _required_text(case.get("id"), "case id")
    auto = _choice(case.get("automated_status", "pending"), AUTOMATED_STATUSES, "automated_status")
    attempts = _integer(case.get("ai_attempts", 0), 0, "ai_attempts")
    manual = _choice(case.get("manual_qa", "not_tested"), MANUAL_QA_VALUES, "manual_qa")
    resolved = case.get("resolved_on_attempt")
    if manual != "pass":
        resolved = None
    elif resolved is None:
        resolved = max(1, attempts)
    else:
        resolved = _integer(resolved, 1, "resolved_on_attempt")
    return {
        "id": case_id,
        "title": _required_text(case.get("title") or case_id, "case title"),
        "platform": _required_text(case.get("platform") or "unspecified", "case platform"),
        "kind": _required_text(case.get("kind") or "custom", "case kind"),
        "automated_status": auto,
        "ai_attempts": attempts,
        "manual_qa": manual,
        "resolved_on_attempt": resolved,
        "gap_type": _choice(case.get("gap_type"), GAP_TYPES, "gap_type"),
        "notes": _optional_text(case.get("notes"), "notes"),
    }


def _normalize_cases(cases):
    normalized = {}
    for raw_case in cases:
        case = _normalize_case(raw_case)
        if case["id"] in normalized:
            raise ValueError(f"Duplicate QA case ID {case['id']!r}")
        normalized[case["id"]] = case
    return normalized


def _checklist(data):
    if not isinstance(data, dict) or data.get("version", 1) != 1 or not isinstance(data.get("cases"), list):
        raise ValueError("Expected a version 1 QA checklist with a cases list")
    return {
        "version": 1,
        "batch_id": _required_text(data.get("batch_id"), "batch_id"),
        "workflow_type": _required_text(data.get("workflow_type", "custom"), "workflow_type"),
        "title": _optional_text(data.get("title"), "title"),
        "tester": _required_text(data.get("tester", _DEFAULT_TESTER), "tester"),
        "build_ref": _optional_text(data.get("build_ref"), "build_ref"),
        "updated_at": _optional_text(data.get("updated_at"), "updated_at"),
        "cases": list(_normalize_cases(data["cases"]).values()),
    }


def _state_case_index(state_path):
    nodes = {}
    if state_path is not None:
        try:
            nodes = read_object(state_path).get("nodes", {})
        except FileNotFoundError:
            pass
    if not isinstance(nodes, dict):
        raise ValueError("State nodes must be a JSON object")
    by_id = defaultdict(list)
    for path, record in nodes.items():
        if isinstance(record, dict):
            by_id[path.rsplit("/", 1)[-1]].append(record)
    return nodes, by_id


def _apply_state(case, nodes, by_id):
    reference = case.get("node_path") or case.get("node_id") or case.get("id")
    record = nodes.get(reference) if isinstance(reference, str) else None
    if not isinstance(record, dict) and isinstance(reference, str) and len(by_id[reference]) == 1:
        record = by_id[reference][0]
    if isinstance(record, dict):
        case = dict(case)
        status = record.get("status")
        status = status if isinstance(status, str) else ""
        case["automated_status"] = status if status in AUTOMATED_STATUSES else _RUN_STATUSES.get(status, "pending")
        attempts = record.get("attempts")
        if type(attempts) is int and attempts >= 0:
            case["ai_attempts"] = attempts
    return case


def _lock_path(path):
    return path.with_name(path.name + ".lock")


def _configured_tester(tester):
    if tester is not None:
        return _required_text(tester, "tester")
    return os.environ.get("KGRAPH_TESTER", "").strip() or None


def init_qa_checklist(output_path, *, batch_id, cases, workflow_type="custom", title=None, tester=None,
                      build_ref=None, state_path=None, preserve_existing=True):
    """Refresh evidence and retain the tester for preserved manual results."""
    path = Path(output_path)
    explicit_tester = tester is not None
    tester = _configured_tester(tester)
    nodes, by_id = _state_case_index(state_path)
    current = _checklist({
        "batch_id": batch_id, "workflow_type": workflow_type, "title": title, "tester": tester or _DEFAULT_TESTER,
        "build_ref": build_ref, "updated_at": _now(),
        "cases": [_apply_state(case, nodes, by_id) if isinstance(case, dict) else case for case in cases],
    })
    with file_lock(_lock_path(path)):
        if preserve_existing and path.is_file():
            previous = _checklist(read_object(path))
            if previous["batch_id"] == current["batch_id"]:
                if tester is None:
                    current["tester"] = previous["tester"]
                for key in ("title", "build_ref"):
                    if current[key] is None:
                        current[key] = previous[key]
                prior_cases = {case["id"]: case for case in previous["cases"]}
                for case in current["cases"]:
                    prior = prior_cases.get(case["id"])
                    if prior and current["build_ref"] == previous["build_ref"] and all(
                        prior[key] == case[key] for key in _CASE_IDENTITY
                    ):
                        case.update({key: prior[key] for key in _MANUAL_FIELDS})
                        if not explicit_tester and (prior["manual_qa"] != "not_tested"
                                                    or prior["gap_type"] is not None or prior["notes"] is not None):
                            current["tester"] = previous["tester"]
                if {**current, "updated_at": previous["updated_at"]} == previous:
                    return previous
        write_json(path, current, ensure_ascii=False)
    return current


def _record_results(checklist_path, results, tester=None):
    """Validate a set of results together before replacing the checklist."""
    path = Path(checklist_path)
    with file_lock(_lock_path(path)):
        raw = read_object(path)
        checklist = _checklist(raw)
        cases = {case["id"]: case for case in checklist["cases"]}
        for result in results:
            case_id = _required_text(result["case_id"], "case id")
            if case_id not in cases:
                raise ValueError(f"Unknown QA case ID {case_id!r}")
            case = cases[case_id]
            manual = {key: result.get(key) for key in _MANUAL_FIELDS}
            if manual["manual_qa"] == case["manual_qa"] == "pass" and manual["resolved_on_attempt"] is None:
                manual["resolved_on_attempt"] = case["resolved_on_attempt"]
            cases[case_id] = _normalize_case({**case, **manual})
        updated = {**checklist, "cases": list(cases.values())}
        if tester is not None:
            updated["tester"] = _required_text(tester, "tester")
        if updated != raw:
            updated["updated_at"] = _now()
            write_json(path, updated, ensure_ascii=False)
        return updated


def record_qa_result(checklist_path, *, case_id, manual_qa, resolved_on_attempt=None, gap_type=None,
                     notes=None, tester=None):
    """Atomically record a manual re-test result and return the checklist."""
    return _record_results(checklist_path, [{
        "case_id": case_id, "manual_qa": manual_qa, "resolved_on_attempt": resolved_on_attempt,
        "gap_type": gap_type, "notes": notes,
    }], _configured_tester(tester))


def _pct(numerator, denominator):
    return round(numerator / denominator * 100, 1) if denominator else None


def _case_counts(cases):
    results = Counter(case["manual_qa"] for case in cases)
    resolved = Counter(case["resolved_on_attempt"] for case in cases if case["manual_qa"] == "pass")
    counts = {
        "total": len(cases), "tested": results["pass"] + results["fail"],
        "passed": results["pass"], "failed": results["fail"], "blocked": results["blocked"],
        "not_tested": results["not_tested"], "skipped": results["skipped"],
    }
    return counts, resolved[1], resolved[2]


def compute_qa_scorecard(checklist):
    """Compute rates over manually tested cases (pass and fail)."""
    data = _checklist(checklist)
    del data["version"]
    cases = data["cases"]
    counts, one_shot, second = _case_counts(cases)
    platforms = defaultdict(list)
    for case in cases:
        platforms[case["platform"]].append(case)
    by_platform = {}
    for platform, entries in platforms.items():
        platform_counts, first, second_attempt = _case_counts(entries)
        del platform_counts["skipped"]
        by_platform[platform] = {
            **platform_counts, "one_shot_passed": first, "within_two_attempts_passed": first + second_attempt,
        }
    return {
        **data, "counts": counts,
        "rates": {
            "manual_pass_pct": _pct(counts["passed"], counts["tested"]),
            "one_shot_passed": one_shot, "one_shot_pct": _pct(one_shot, counts["tested"]),
            "second_attempt_passed": second, "within_two_attempts_passed": one_shot + second,
            "within_two_attempts_pct": _pct(one_shot + second, counts["tested"]),
        },
        "auto_pass_manual_fail": [
            {key: case[key] for key in ("id", "title", "platform", "gap_type", "notes")}
            for case in cases if case["automated_status"] == "pass" and case["manual_qa"] == "fail"
        ],
        "gap_breakdown": dict(Counter(case["gap_type"] or "unspecified"
                                      for case in cases if case["manual_qa"] in ("fail", "blocked"))),
        "by_platform": by_platform,
    }


def _cell(value):
    if value is None or value == "":
        return "—"
    return escape(" ".join(str(value).split()), quote=False).translate(_MARKDOWN_ESCAPES)


def _table(headers, rows):
    def row(cells):
        return "| " + " | ".join(map(_cell, cells)) + " |"

    return [row(headers), row(["---"] * len(headers)), *(row(cells) for cells in rows)]


def _format_pct(value):
    return "—" if value is None else f"{value:.1f}%"


def render_qa_scorecard_markdown(scorecard):
    """Render the current manual QA results for a leadership review."""
    counts, rates = scorecard["counts"], scorecard["rates"]
    tested = counts["tested"]
    lines = [f"# QA Scorecard — {_cell(scorecard['title'] or scorecard['batch_id'])}", "",
             f"Batch **{_cell(scorecard['batch_id'])}** · "
             f"{_cell(scorecard['workflow_type'])} · Tester **{_cell(scorecard['tester'])}**"]
    if scorecard["build_ref"]:
        lines.append(f"Build **{_cell(scorecard['build_ref'])}**")
    if scorecard["updated_at"]:
        lines.append(f"Updated {_cell(scorecard['updated_at'])}")
    lines.extend(["", *_table(("KPI", "Cases", "Rate"), [
        ("Manual QA Pass %", f"{counts['passed']} / {tested}", _format_pct(rates["manual_pass_pct"])),
        ("One-Shot %", f"{rates['one_shot_passed']} / {tested}", _format_pct(rates["one_shot_pct"])),
        ("≤2-Shot %", f"{rates['within_two_attempts_passed']} / {tested}", _format_pct(rates["within_two_attempts_pct"])),
    ]), "", f"Tested **{tested}/{counts['total']}** · Failed **{counts['failed']}** · "
            f"Blocked **{counts['blocked']}** · Awaiting re-test **{counts['not_tested']}** · "
            f"Skipped **{counts['skipped']}**", "",
        "Rates use manual passes and failures as the denominator.", "",
        "## Auto-Pass → Manual-Fail Graph Gaps", "",
    ])
    gaps = scorecard["auto_pass_manual_fail"]
    if gaps:
        lines.extend(_table(("ID", "Case", "Platform", "Gap", "Notes"), (
            (case["id"], case["title"], case["platform"], case["gap_type"], case["notes"]) for case in gaps
        )))
    else:
        lines.append("None recorded.")
    lines.extend(["", "## By Platform", "", *_table(
        ("Platform", "Total", "Tested", "Passed", "Failed", "Blocked", "Awaiting re-test", "One-shot", "Within two"),
        ((platform, *[values[key] for key in ("total", "tested", "passed", "failed", "blocked", "not_tested",
                                             "one_shot_passed", "within_two_attempts_passed")])
         for platform, values in sorted(scorecard["by_platform"].items())),
    ), "", "## Cases", "", *_table(
        ("ID", "Case", "Platform", "AI attempts", "Automated", "Manual", "Passed on attempt", "Gap", "Notes"),
        ((case["id"], case["title"], case["platform"], case["ai_attempts"], case["automated_status"],
          case["manual_qa"], case["resolved_on_attempt"], case["gap_type"], case["notes"]) for case in scorecard["cases"]),
    )])
    return "\n".join(lines) + "\n"


def _history_path(path):
    if path is not None:
        return Path(path).expanduser()
    home = Path(os.environ.get("KGRAPH_HOME") or Path.home() / ".kgraph")
    return home.expanduser().resolve() / "qa_history.jsonl"


def _history_rows(path):
    rows = {}
    try:
        with path.open(encoding="utf-8") as file:
            for line in file:
                if line.strip():
                    row = json.loads(line)
                    rows[row["batch_id"]] = row
    except FileNotFoundError:
        pass
    return rows


def write_qa_scorecard(checklist_path, *, json_path=None, md_path=None, record_history=False, history_path=None):
    """Publish scorecards beside the checklist and optionally update batch history."""
    path = Path(checklist_path)
    json_path = Path(json_path) if json_path is not None else path.with_name("qa_scorecard.json")
    md_path = Path(md_path) if md_path is not None else path.with_name("qa_scorecard.md")
    output_paths = (path, json_path, md_path)
    if record_history:
        output_paths += (_history_path(history_path),)
    if len({item.resolve() for item in output_paths}) != len(output_paths):
        raise ValueError("The QA checklist, scorecard outputs, and history must be separate files")
    with file_lock(_lock_path(path)):
        scorecard = compute_qa_scorecard(read_object(path))
        write_json(json_path, scorecard, ensure_ascii=False)
        write_text(md_path, render_qa_scorecard_markdown(scorecard))
        if record_history:
            history = output_paths[-1]
            with file_lock(_lock_path(history)):
                rows = _history_rows(history)
                rows[scorecard["batch_id"]] = scorecard
                write_text(history, "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows.values()))
    return scorecard


def aggregate_qa_history(history_path=None):
    """Aggregate unique batch scorecards, weighted by the number of tested cases."""
    rows = list(_history_rows(_history_path(history_path)).values())
    totals, rates, gaps = Counter(), Counter(), Counter()
    for row in rows:
        totals.update(row["counts"])
        rates.update({key: row["rates"][key] for key in ("one_shot_passed", "within_two_attempts_passed")})
        gaps.update(row["gap_breakdown"])
    tested = totals["tested"]
    recent = sorted(reversed(rows), key=lambda row: row.get("updated_at") or "", reverse=True)[:10]
    return {
        "batches": len(rows), "total_cases": totals["total"], "tested": tested, "passed": totals["passed"],
        "failed": totals["failed"], "blocked": totals["blocked"], "manual_pass_pct": _pct(totals["passed"], tested),
        "one_shot_passed": rates["one_shot_passed"], "one_shot_pct": _pct(rates["one_shot_passed"], tested),
        "within_two_attempts_passed": rates["within_two_attempts_passed"],
        "within_two_attempts_pct": _pct(rates["within_two_attempts_passed"], tested),
        "auto_pass_manual_fail_count": sum(len(row["auto_pass_manual_fail"]) for row in rows),
        "gap_breakdown": dict(gaps),
        "recent_batches": [{key: row[key] for key in (
            "batch_id", "workflow_type", "title", "tester", "build_ref", "updated_at", "counts", "rates",
        )} for row in recent],
    }


def _graph_qa_checklist(graph):
    paths = {node.qa_checklist_path for node in graph if getattr(node, "qa_checklist_path", None) is not None}
    return next(iter(paths)) if len(paths) == 1 else None


def _run_qa_checklist(state_path):
    state = Path(state_path).resolve()
    return state.parent / f"{state.stem}_qa" / "manual_qa.json"
