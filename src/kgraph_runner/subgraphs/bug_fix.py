"""Resumable repository bug fixes with command-based verification."""

from __future__ import annotations

from collections import deque
from pathlib import Path
import re
import subprocess
from typing import Any, Callable, Dict, List, Optional, Sequence, Tuple, Union

from .._files import read_object, require_id, write_json
from ..engine import LoopSubgraphNode, TaskNode
from ..helpers import hash_globs
from ..qa_metrics import init_qa_checklist, write_qa_scorecard
from ._common import Repairs, files_hash, fingerprint, packet_worker, record_verification, verification, with_retry_route

Command = Union[str, Sequence[str]]
Verifier = Callable[[Dict[str, Any]], Tuple[bool, str, str]]
_STAGES = ("triage_and_plan", "apply_fix", "build_check", "run_tests", "verify_fix")
_FAILURE_LINE = re.compile(r"\b(?:fail(?:ed|ure)?|traceback|\w*(?:error|exception))\b", re.I)
_TRACE_FRAME = re.compile(r'^File ".*", line \d+(?:, in .*)?$')
_VOLATILE_OUTPUT = (
    (re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]"), ""),
    (re.compile(r"\b(?:\d{4}-\d{2}-\d{2}[ T])?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b"), "<time>"),
    (re.compile(r"\b\d+(?:\.\d+)?\s*(?:ms|s)\b"), "<duration>"),
    (re.compile(r"\b0x[0-9a-fA-F]+\b"), "<address>"),
    (re.compile(r"\bline \d+\b|:\d+(?::\d+)?(?=[:)\s]|$)"), "<line>"),
)


def require_bug_id(value):
    bug_id = require_id(value)
    # These are reserved directory names on Windows, even with an extension.
    if re.fullmatch(r"CON|PRN|AUX|NUL|(?:COM|LPT)[1-9]", bug_id, re.I):
        raise ValueError(f"Invalid bug ID {bug_id!r}: reserved Windows filename")
    return bug_id


def _command(value, name):
    if value is None:
        return None
    if isinstance(value, str):
        if value.strip():
            return value
    elif isinstance(value, Sequence) and value and all(isinstance(arg, str) for arg in value):
        if value[0].strip():
            return tuple(value)
    raise ValueError(f"{name} must be a non-empty command string or argument sequence")


def _failure_output(log_path, repo_dir):
    def normalize(line):
        line = line.replace(str(repo_dir), "<repo>")
        for pattern, replacement in _VOLATILE_OUTPUT:
            line = pattern.sub(replacement, line)
        return " ".join(line.split())

    # Include traceback frames and their source expressions so that a bare
    # AssertionError can still distinguish distinct failing assertions.
    tail = deque(maxlen=20)
    selected = deque(maxlen=12)
    previous_frame = False
    with log_path.open(encoding="utf-8", errors="replace") as log:
        for line in log:
            line = line.strip()
            if not line:
                continue
            tail.append(line)
            frame = bool(_TRACE_FRAME.fullmatch(line))
            if frame or previous_frame or _FAILURE_LINE.search(line):
                selected.append(line)
            previous_frame = frame
    failure_lines = selected or list(tail)[-5:]
    return "\n".join(tail)[-1200:], fingerprint([normalize(line) for line in failure_lines])[:8]


def _run_command(stage, command, log_path, repo_dir, timeout_s):
    with log_path.open("wb") as log:
        try:
            result = subprocess.run(
                command, shell=isinstance(command, str), cwd=repo_dir, timeout=timeout_s,
                stdout=log, stderr=subprocess.STDOUT,
            )
        except subprocess.TimeoutExpired:
            signature, message = f"{stage}_timeout", f"{stage} timed out after {timeout_s}s"
        except OSError as exc:
            signature, message = f"{stage}_launch_error", f"Could not start {stage} command"
            log.write(f"{exc}\n".encode("utf-8", errors="replace"))
        else:
            if result.returncode == 0:
                return True, "", f"{stage} command passed"
            signature = f"{stage}_exit_{result.returncode}"
            message = f"{stage} command exited with code {result.returncode}"

    tail, failure_hash = _failure_output(log_path, repo_dir)
    if signature.startswith(f"{stage}_exit_"):
        signature = f"{signature}:{failure_hash}"
    return False, signature, f"{message}\n{tail}" if tail else message


def _verification_passed(path):
    try:
        return verification(path)[0]
    except (OSError, ValueError, KeyError):
        return False


def _nonempty_text(value):
    return isinstance(value, str) and bool(value.strip())


def _bug_qa_case(spec, fix):
    platform = spec.get("platform")
    return {"id": fix["bug_id"], "title": fix["title"],
            "platform": platform if isinstance(platform, str) else None,
            "kind": "bug", "automated_status": "pass", "ai_attempts": fix["attempts"]}


def create_desktop_bug_fix_subgraph(
    bug_spec: dict, *, work_dir: Path, repo_dir: Path,
    repro_cmd: Optional[Command] = None, build_cmd: Optional[Command] = None,
    test_cmd: Optional[Command] = None, watch_globs: Sequence[str] = (),
    verify_fn: Optional[Verifier] = None, resource: Optional[str] = "repo",
    timeout_s: Optional[float] = 300.0, max_retries: int = 4,
    deps: Tuple[str, ...] = (), node_id: Optional[str] = None,
    review_feedback: Optional[dict] = None,
) -> LoopSubgraphNode:
    """Pause for a fix report, then build, test, and request repairs as needed."""
    spec = dict(bug_spec)
    bug_id = require_bug_id(spec.get("id"))
    commands = {
        "build": _command(build_cmd, "build_cmd"),
        "repro": _command(repro_cmd, "repro_cmd"),
        "test": _command(test_cmd, "test_cmd"),
    }
    if verify_fn is None and not any(commands.values()):
        raise ValueError("Configure at least one of repro_cmd, build_cmd, test_cmd, or verify_fn")
    if verify_fn is not None and not callable(verify_fn):
        raise ValueError("verify_fn must be callable")
    if type(max_retries) is not int or max_retries < 0:
        raise ValueError("max_retries must be a nonnegative integer")

    repo_dir = Path(repo_dir).resolve()
    watch_globs = tuple(watch_globs)
    directory = Path(work_dir).resolve() / "bugs" / bug_id
    spec_path = directory / "bug_spec.json"
    fix_report = directory / "fix_report.json"
    build_result = directory / "build_result.json"
    test_result = directory / "test_result.json"
    verify_result = directory / "verification.json"
    final_summary = directory / "final_summary.json"
    qa_checklist = directory / "manual_qa.json"
    logs = {stage: directory / f"{stage}.log" for stage in commands}
    settings = {"repo_dir": str(repo_dir), "commands": commands, "timeout_s": timeout_s,
                "watch_globs": watch_globs}
    verifier_name = getattr(verify_fn, "__qualname__", type(verify_fn).__name__) if verify_fn is not None else None
    config_hash = fingerprint(spec, settings, verifier_name)

    def fix_hash():
        return fingerprint(files_hash((fix_report,)), hash_globs(watch_globs, root=repo_dir))

    # Source edits change the rejected artifact, while the repair history must
    # persist across those same edits. A verified delivery can be checked again
    # after a source edit without requiring another worker call.
    repairs = Repairs(directory / "repair_state.json", lambda: config_hash, (fix_report,),
                      metadata=fix_report, artifact_hash=fix_hash,
                      minimum_revision=review_feedback["fix_revision"] if review_feedback else 0)

    def triage(_ctx, _attempt):
        write_json(spec_path, {
            **spec, **settings, "log_paths": {stage: str(path) for stage, path in logs.items()},
            **repairs.instructions(),
        })

    def existing_logs():
        return {stage: str(path) for stage, path in logs.items() if path.is_file()}

    def fix_packet(_ctx):
        review = {"review_feedback": review_feedback} if review_feedback else {}
        instructions = ["Investigate the bug and apply the fix in repo_dir."]
        if review_feedback:
            instructions.append("Address the reviewer notes in review_feedback before delivering the revised fix.")
        return {
            "stage": "apply_fix", "bug_id": bug_id, "bug_spec": spec, "spec_path": str(spec_path),
            **settings, **review, "log_paths": existing_logs(),
            "instructions": [
                *instructions,
                "Use the commands, logs, and repair_feedback to address verification failures.",
                "Deliver fix_report.json with the requested integer revision or newer, "
                "a root_cause, a summary, and a list of changed_files.",
            ],
        }

    def verification_inputs(*result_paths):
        return fingerprint(config_hash, fix_hash(), files_hash(result_paths))

    def run_stage(stage):
        return _run_command(stage, commands[stage], logs[stage], repo_dir, timeout_s)

    def check_build(_ctx):
        if commands["build"] is not None:
            return run_stage("build")
        logs["build"].write_text("No build command configured.\n", encoding="utf-8")
        return True, "", "No build command configured"

    test_stages = tuple(stage for stage in ("repro", "test") if commands[stage] is not None)

    def check_tests(ctx):
        build = verification(build_result)
        for stage in ("repro", "test"):
            if not build[0] or stage not in test_stages:
                logs[stage].unlink(missing_ok=True)
        if not build[0]:
            return build
        results = [run_stage(stage) for stage in test_stages]
        if verify_fn is not None:
            results.append(verify_fn(ctx))
        return next((result for result in results if not result[0]),
                    (True, "", "Reproduction and regression checks passed" if results else "No tests configured"))

    def check_fix(_ctx):
        for result_path in (build_result, test_result):
            result = verification(result_path)
            if not result[0]:
                final_summary.unlink(missing_ok=True)
                return result
        try:
            report = read_object(fix_report)
        except (OSError, ValueError):
            report = {}
        instructions = repairs.instructions()
        revision = report.get("revision")
        changed_files = report.get("changed_files")
        if (type(revision) is not int or revision < instructions["revision"]
                or not _nonempty_text(report.get("summary")) or not _nonempty_text(report.get("root_cause"))
                or not isinstance(changed_files, list) or not all(map(_nonempty_text, changed_files))):
            final_summary.unlink(missing_ok=True)
            return (False, "invalid_fix_report", "Provide the required revision, a non-empty summary and root_cause, "
                    "and a changed_files list of paths in fix_report.json.")
        review_attempts = review_feedback.get("review_attempts", 1) if review_feedback else 0
        fix = {
            "bug_id": bug_id, "title": spec.get("title") or bug_id,
            "root_cause": report["root_cause"], "summary": report["summary"], "changed_files": changed_files,
            "revision": revision, "attempts": len(instructions["repair_feedback"]) + 1 + review_attempts,
            "log_paths": existing_logs(),
        }
        init_qa_checklist(qa_checklist, batch_id=bug_id, workflow_type="desktop-bug-fix",
                          title=fix["title"], cases=[_bug_qa_case(spec, fix)])
        write_json(final_summary, fix)
        return True, "", f"Verified fix for {bug_id}"

    build = record_verification(
        "build_check", path=build_result, input_hash=verification_inputs,
        check=check_build, deps=("apply_fix",), resource=resource,
    )
    build.outputs_fn = lambda _ctx: (build_result, logs["build"])
    tests = record_verification(
        "run_tests", path=test_result, input_hash=lambda: verification_inputs(build_result),
        check=check_tests, deps=("build_check",), resource=resource,
    )
    tests.outputs_fn = lambda _ctx: (
        (test_result, *(logs[stage] for stage in test_stages))
        if _verification_passed(build_result) else (test_result,)
    )
    if verify_fn is not None:
        # The runner writes telemetry into the same context as the verifier.
        # Keep telemetry out of the cache key so warm runs remain cache hits.
        tests.input_hash_fn = lambda ctx: fingerprint(
            verification_inputs(build_result),
            {key: value for key, value in ctx.items() if key not in ("verification_results", "repair_feedback")},
        )
    final = record_verification(
        "verify_fix", path=verify_result,
        input_hash=lambda: fingerprint(verification_inputs(build_result, test_result), repairs.instructions(),
                                       "qa-v1", review_feedback),
        check=check_fix, deps=("run_tests",),
    )
    final.outputs_fn = lambda _ctx: (
        (verify_result, final_summary) if _verification_passed(verify_result) else (verify_result,)
    )

    def verify(_ctx):
        result = verification(verify_result)
        if result[0]:
            repairs.resolved()
        return result

    def repair(ctx, _attempt, signature, detail):
        return repairs.repair(ctx, signature, detail, _STAGES, max_retries)

    loop = LoopSubgraphNode(
        id=bug_id if node_id is None else require_id(node_id), deps=deps, resource=resource,
        max_retries=max_retries, verify_fn=verify, repair_fn=repair,
        nodes=[
            TaskNode(
                id="triage_and_plan", kind="code", run_fn=triage, outputs_fn=lambda _ctx: (spec_path,),
                input_hash_fn=lambda _ctx: fingerprint(config_hash, repairs.instructions()),
            ),
            packet_worker(
                "apply_fix", packet_path=directory / "fix_packet.json", artifacts=(fix_report,),
                input_hash=lambda: config_hash, packet=fix_packet, deps=("triage_and_plan",), repairs=repairs,
            ),
            build, tests, final,
        ],
    )
    loop.qa_checklist_path = qa_checklist
    return with_retry_route(loop, "verify_fix", "apply_fix")


def create_bug_batch_graph(
    bugs: Sequence[dict], *, work_dir: Path, repo_dir: Path,
    repro_cmd: Optional[Command] = None, build_cmd: Optional[Command] = None,
    test_cmd: Optional[Command] = None, watch_globs: Sequence[str] = (),
    verify_fn: Optional[Verifier] = None, resource: Optional[str] = "repo",
    timeout_s: Optional[float] = 300.0, max_retries: int = 4, batch_id: Optional[str] = None,
) -> List[Any]:
    """Fix bugs in one checkout and publish a single report after verification."""
    work_dir = Path(work_dir).resolve()
    batch_id = batch_id or work_dir.name or "bug-batch"
    qa_checklist = work_dir / "manual_qa.json"
    defaults = {"repro_cmd": repro_cmd, "build_cmd": build_cmd, "test_cmd": test_cmd,
                "watch_globs": watch_globs, "max_retries": max_retries}
    nodes, specs = [], []
    used_ids = {"consolidated_report"}
    for bug in bugs:
        spec = dict(bug)
        bug_id = require_bug_id(spec.get("id"))
        if bug_id.casefold() in used_ids:
            raise ValueError(f"Duplicate or reserved batch bug ID {bug_id!r}")
        used_ids.add(bug_id.casefold())
        overrides = {key: spec.pop(key, default) for key, default in defaults.items()}
        specs.append(spec)
        nodes.append(create_desktop_bug_fix_subgraph(
            spec, work_dir=work_dir, repo_dir=repo_dir, verify_fn=verify_fn,
            resource=resource, timeout_s=timeout_s,
            deps=(nodes[-1].id,) if resource and nodes else (), **overrides,
        ))
        nodes[-1].qa_checklist_path = qa_checklist

    summaries = tuple(work_dir / "bugs" / node.id / "final_summary.json" for node in nodes)
    json_report = work_dir / "batch_report.json"
    markdown_report = work_dir / "batch_report.md"

    def report(_ctx, _attempt):
        fixes = [read_object(path) for path in summaries]
        init_qa_checklist(qa_checklist, batch_id=batch_id, workflow_type="bug-batch", title="Bug fix batch",
                          cases=[_bug_qa_case(spec, fix) for spec, fix in zip(specs, fixes)])
        write_qa_scorecard(qa_checklist)
        write_json(json_report, {"fixed_count": len(fixes), "bugs": fixes})
        noun = "fix" if len(fixes) == 1 else "fixes"
        lines = ["# Bug fix batch report", "", f"Verified {len(fixes)} {noun}."]
        for fix in fixes:
            lines.extend([
                "", f"## {fix['bug_id']} — {fix['title']}", "",
                fix["summary"], "", "### Root cause", "", fix["root_cause"], "",
                f"Verified on attempt {fix['attempts']} (revision {fix['revision']}).",
            ])
            if fix["changed_files"]:
                lines.extend(["", "### Changed files", "", *(f"- {path}" for path in fix["changed_files"])])
        markdown_report.write_text("\n".join(lines) + "\n", encoding="utf-8")

    report_node = TaskNode(
        id="consolidated_report", kind="code", deps=tuple(node.id for node in nodes),
        run_fn=report, input_hash_fn=lambda _ctx: fingerprint("qa-v1", batch_id, specs, files_hash(summaries)),
        outputs_fn=lambda _ctx: (json_report, markdown_report),
    )
    report_node.qa_checklist_path = qa_checklist
    return [*nodes, report_node]
