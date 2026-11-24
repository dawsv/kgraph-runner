"""Resumable game regression suites with command and screenshot verification."""

from __future__ import annotations

from pathlib import Path
from typing import List, Mapping, Optional, Sequence, Tuple, Union

from .._files import read_object, require_id, write_json
from ..engine import SubgraphNode, TaskNode
from ..helpers import hash_globs, hash_paths, shell_task, subagent_gate
from ..qa_metrics import init_qa_checklist, write_qa_scorecard
from ..verifiers import verify_png_file
from ._common import files_hash, fingerprint
from .bug_fix import Command, _command

VisualCheck = Union[str, Mapping[str, Union[str, Path, int]]]
_LOG_TAIL_BYTES = 4096


def _dimension(value, name):
    if type(value) is not int or value < 1:
        raise ValueError(f"{name} must be a positive integer")
    return value


def _visual_checks(checks, directory, min_width, min_height):
    results = []
    ids, paths = set(), set()
    for check in checks:
        if isinstance(check, str):
            check_id = require_id(check)
            check = {"id": check_id, "path": Path("visual") / f"{check_id}.png"}
        check_id = require_id(check["id"])
        path = (directory / check["path"]).resolve()
        if check_id in ids or path in paths:
            raise ValueError(f"Duplicate visual check ID or path for {check_id!r}")
        ids.add(check_id)
        paths.add(path)
        results.append({
            "id": check_id, "path": str(path),
            "min_width": _dimension(check.get("min_width", min_width), "min_width"),
            "min_height": _dimension(check.get("min_height", min_height), "min_height"),
        })
    return results


def _log_summary(path):
    with path.open("rb") as log:
        size = log.seek(0, 2)
        log.seek(max(0, size - _LOG_TAIL_BYTES))
        tail = log.read().decode("utf-8", errors="replace").rstrip()
    return {"path": str(path), "size_bytes": size, "tail": tail, "truncated": size > _LOG_TAIL_BYTES}


def create_game_regression_subgraph(
    suite_id: str, *, work_dir: Path, repo_dir: Union[str, Path] = ".",
    build_cmd: Optional[Command] = None, test_cmd: Command, watch_globs: Sequence[str] = (),
    visual_checks: Sequence[VisualCheck] = (), min_width: int = 16, min_height: int = 16,
    timeout_s: Optional[float] = None, deps: Tuple[str, ...] = (),
) -> SubgraphNode:
    """Run build and regression commands, then verify evidence in a suite-specific work directory."""
    suite_id = require_id(suite_id)
    build_cmd = _command(build_cmd, "build_cmd")
    test_cmd = _command(test_cmd, "test_cmd")
    if test_cmd is None:
        raise ValueError("test_cmd is required")

    repo_dir = Path(repo_dir).resolve()
    directory = Path(work_dir).resolve()
    watch_globs = tuple(watch_globs)
    checks = _visual_checks(visual_checks, directory, _dimension(min_width, "min_width"),
                            _dimension(min_height, "min_height"))
    preflight_path = directory / "preflight.json"
    build_log, regression_log = directory / "build.log", directory / "regression.log"
    packet_path = directory / "visual_packet.json"
    json_report, markdown_report = directory / "regression_report.json", directory / "regression_report.md"
    qa_checklist = directory / "manual_qa.json"
    qa_paths = (qa_checklist, directory / "qa_scorecard.json", directory / "qa_scorecard.md",
                directory / "manual_qa.json.lock")
    visual_paths = tuple(Path(check["path"]) for check in checks)
    generated_paths = (preflight_path, build_log, regression_log, packet_path, json_report, markdown_report, *qa_paths)
    if set(visual_paths).intersection(generated_paths):
        raise ValueError("Visual check paths must be separate from generated workflow files")
    excluded = (*generated_paths, *visual_paths)

    settings = {
        "suite_id": suite_id, "repo_dir": str(repo_dir), "build_cmd": build_cmd, "test_cmd": test_cmd,
        "watch_globs": watch_globs, "visual_checks": checks, "timeout_s": timeout_s,
    }
    config_hash = fingerprint(settings)

    def source_hash():
        if not repo_dir.is_dir():
            raise ValueError(f"Repository directory does not exist: {repo_dir}")
        if watch_globs:
            return hash_globs(watch_globs, root=repo_dir, exclude_paths=excluded)
        return hash_paths([repo_dir], root=repo_dir, exclude_paths=excluded)

    def clear_reports():
        json_report.unlink(missing_ok=True)
        markdown_report.unlink(missing_ok=True)

    def preflight(_ctx, _attempt):
        clear_reports()
        write_json(preflight_path, {**settings, "source_hash": source_hash(), "config_hash": config_hash})

    def command_stage(stage, command, log_path, parent, inputs):
        task = shell_task(
            stage, command, cwd=repo_dir, timeout_s=timeout_s, deps=(parent,), log_path=log_path,
            input_hash_fn=lambda _ctx: files_hash(inputs), outputs_fn=lambda _ctx: (log_path,),
        )
        run_command = task.run_fn

        def run(ctx, attempt):
            clear_reports()
            return run_command(ctx, attempt)

        task.run_fn = run
        return task

    nodes = [TaskNode(
        id="preflight", kind="code", run_fn=preflight,
        input_hash_fn=lambda _ctx: fingerprint(config_hash, source_hash()),
        outputs_fn=lambda _ctx: (preflight_path,),
    )]
    command_inputs = (preflight_path,)
    if build_cmd is not None:
        nodes.append(command_stage("compile_or_build", build_cmd, build_log, nodes[-1].id, command_inputs))
        command_inputs += (build_log,)
    nodes.append(command_stage("run_regression", test_cmd, regression_log, nodes[-1].id, command_inputs))
    report_inputs = (*command_inputs, regression_log)

    def visual_results():
        for check, path in zip(checks, visual_paths):
            ok, signature, detail = verify_png_file(
                path, min_width=check["min_width"], min_height=check["min_height"],
            )
            yield {**check, "ok": ok, "error_signature": signature, "detail": detail}

    def visual_packet(_ctx):
        clear_reports()
        return {
            "stage": "capture_visual_evidence", "suite_id": suite_id, "repo_dir": str(repo_dir),
            "outputs": {check["id"]: check["path"] for check in checks}, "visual_checks": checks,
            "instructions": ["Capture each screenshot at the specified path and minimum dimensions.",
                             "Use an 8-bit non-interlaced PNG with visible content for each check."],
        }

    def verify_visuals(_ctx, _paths):
        failures = [check for check in visual_results() if not check["ok"]]
        if failures:
            return False, failures[0]["error_signature"], "\n".join(check["detail"] for check in failures)
        return True, "", f"Verified {len(checks)} visual check(s)"

    if checks:
        nodes.append(subagent_gate(
            "capture_visual_evidence", packet_path=packet_path, artifact_paths=visual_paths,
            packet_fn=visual_packet, verify_fn=verify_visuals, deps=("run_regression",),
            input_hash_fn=lambda _ctx: files_hash(report_inputs),
        ))

    def report(_ctx, _attempt):
        evidence = list(visual_results())
        if any(not check["ok"] for check in evidence):
            clear_reports()
            raise ValueError("A screenshot changed after verification. Rerun to verify the current evidence.")
        logs = {"build": _log_summary(build_log)} if build_cmd is not None else {}
        logs["regression"] = _log_summary(regression_log)
        preflight_data = read_object(preflight_path)
        automated = {"automated_status": "pass", "ai_attempts": 1}
        qa_cases = [{"id": suite_id, "title": f"Re-test {suite_id}", "kind": "regression-suite", **automated}]
        qa_cases.extend({
            "id": check["id"] if check["id"] != suite_id else f"visual/{check['id']}",
            "title": f"Visual check: {check['id']}", "kind": "visual", **automated,
        } for check in evidence)
        init_qa_checklist(qa_checklist, batch_id=suite_id, workflow_type="game-regression",
                          title=f"Game regression for {suite_id}", cases=qa_cases,
                          build_ref=fingerprint(preflight_data["source_hash"], preflight_data["config_hash"]))
        write_qa_scorecard(qa_checklist)
        write_json(json_report, {"suite_id": suite_id, "status": "passed", "preflight": preflight_data,
                                 "logs": logs, "visual_checks": evidence})
        lines = [f"# Game regression report for {suite_id}", "", "Passed.", "",
                 f"Source fingerprint `{preflight_data['source_hash']}`",
                 f"\nSuite configuration fingerprint `{preflight_data['config_hash']}`"]
        for label, log in logs.items():
            filename = Path(log["path"]).name
            lines.extend(["", f"## {label.capitalize()} log", "", f"[{filename}]({filename})"])
            if log["tail"]:
                lines.extend(["", *(f"    {line}" for line in log["tail"].splitlines())])
        if evidence:
            lines.extend(["", "## Visual evidence", "",
                          *(f"- `{check['id']}` — {check['detail']} (`{check['path']}`)" for check in evidence)])
        markdown_report.write_text("\n".join(lines) + "\n", encoding="utf-8")

    nodes.append(TaskNode(
        id="regression_report", kind="code", run_fn=report, deps=(nodes[-1].id,),
        input_hash_fn=lambda _ctx: fingerprint("qa-v1", files_hash((*report_inputs, *visual_paths))),
        outputs_fn=lambda _ctx: (json_report, markdown_report),
    ))
    suite = SubgraphNode(id=suite_id, nodes=nodes, deps=deps)
    suite.qa_checklist_path = qa_checklist
    return suite


def create_game_regression_graph(
    suite_id: str, *, work_dir: Path, repo_dir: Union[str, Path] = ".",
    build_cmd: Optional[Command] = None, test_cmd: Command, watch_globs: Sequence[str] = (),
    visual_checks: Sequence[VisualCheck] = (), min_width: int = 16, min_height: int = 16,
    timeout_s: Optional[float] = None, deps: Tuple[str, ...] = (),
) -> List[SubgraphNode]:
    """Build a standalone game regression workflow."""
    return [create_game_regression_subgraph(
        suite_id, work_dir=work_dir, repo_dir=repo_dir, build_cmd=build_cmd, test_cmd=test_cmd,
        watch_globs=watch_globs, visual_checks=visual_checks, min_width=min_width, min_height=min_height,
        timeout_s=timeout_s, deps=deps,
    )]
