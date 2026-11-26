"""Command-line entry points for standalone KGraph and Hero Town."""

import argparse
from contextlib import suppress
from functools import partial
import json
from math import isfinite
import os
from pathlib import Path
import socket
from stat import S_ISREG
import sys
from tempfile import TemporaryDirectory
from threading import Condition, Lock, Thread
from time import monotonic, sleep

from ._files import read_object
from .interactions import (
    _PACKET_LIMIT, _PROGRESS_STATUSES, _active_gate, _allowed_root, _inside, _optional_object,
    _read_json as _read_packet, _reject_path_traversal, _resolve_packet, _subtask_path, _valid_node_path,
    ask_node_question, extract_input_gate, progress_path_for_state, read_subtask_answer,
    submit_gate_response, update_node_progress,
)
from .registry import register_run
from .workflow import _load_workflow, _read_json, _require_object, _resolve_context, _resolve_graph


def _run_files(workflow, state, report):
    directory = Path(".kgraph")
    return state or directory / f"{workflow.stem}_state.json", report or directory / f"{workflow.stem}_report.json"


def _write_run_report(runner, path, workflow, *, definition, previous_worker_calls, **kwargs):
    from .qa_metrics import _graph_qa_checklist

    path.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=path.parent) as directory:
        draft = runner.write_run_report(Path(directory) / "report.json", **kwargs)
        report = _read_json(draft)
        report["workflow_ref"] = str(workflow)
        if definition is not None and (qa_path := _graph_qa_checklist(definition[0])) is not None:
            report["qa_checklist_path"] = str(Path(qa_path).resolve())
        summary = report["summary"]
        summary["worker_calls_used"] += previous_worker_calls
        if summary["max_worker_calls"] is not None:
            summary["max_worker_calls"] += previous_worker_calls
        draft.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        draft.replace(path)


def _publish_snapshot(snapshot, hub_url, token, *, run_id=None, project_name=None, host_label=None):
    from .network import push_snapshot

    snapshot = {**snapshot}
    for key in ("state_file", "report_file"):
        if snapshot.get(key):
            snapshot[key] = str(Path(snapshot[key]).resolve())
    project = Path.cwd()
    run = {
        "name": snapshot["title"],
        "project_name": project_name if project_name is not None else project.name or str(project),
        "source_host": host_label if host_label is not None else socket.gethostname(),
        "status": snapshot["status"],
    }
    if run_id is not None:
        run["run_id"] = run_id
    return push_snapshot(hub_url, token, run, snapshot)


def _build_run_snapshot(args, workflow, state, report, definition, status, *, event=None):
    from .snapshot import _snapshot_from_graph

    project = Path.cwd()
    try:
        try:
            snapshot = _snapshot_from_graph(workflow, state, report, definition=definition,
                                            status_override=status, project_dir=project)
        except Exception:
            # A failed graph definition can still have usable state and report telemetry.
            snapshot = _snapshot_from_graph(workflow, state, report, status_override=status, project_dir=project)
        snapshot.update(project_dir=str(project), factory=args.factory)
        if event is not None:
            fields = ("event", "timestamp", "node", "kind", "attempt", "attempts", "duration_s")
            snapshot["event"] = {key: event[key] for key in fields if key in event}
        return snapshot
    except Exception as exc:
        print(f"kgraph-town: could not report to Hero Town Hub: {exc}", file=sys.stderr)


class _HubPublisher:
    """Serialize Hub requests with one pending snapshot that newer events can replace."""

    def __init__(self, url, token):
        self.url = url
        self.token = token
        self.run_id = None
        self._condition = Condition()
        self._started = self._sending = self._closed = False
        self._pending = None

    def set_initial_run_id(self, run_id):
        with self._condition:
            if self.run_id is None:
                self.run_id = run_id

    def submit(self, snapshot):
        if snapshot is None:
            return
        with self._condition:
            if not self._started:
                self._started = self._sending = True
                Thread(target=self._send, args=(snapshot,), daemon=True).start()
            else:
                self._pending = snapshot
                self._condition.notify_all()

    def _send(self, snapshot):
        while snapshot is not None:
            with self._condition:
                run_id = self.run_id
            try:
                published = _publish_snapshot(snapshot, self.url, self.token, run_id=run_id)
                with self._condition:
                    self.run_id = published["id"]
            except Exception as exc:
                print(f"kgraph-town: could not report to Hero Town Hub: {exc}", file=sys.stderr)
            with self._condition:
                self._sending = False
                self._condition.notify_all()
                self._condition.wait_for(lambda: self._pending is not None or self._closed)
                snapshot, self._pending = self._pending, None
                self._sending = snapshot is not None

    def _flush(self, timeout):
        with self._condition:
            self._condition.wait_for(lambda: not self._sending and self._pending is None, timeout=timeout)
            return self.run_id

    def finish(self, snapshot):
        # Give a healthy Hub time to receive the last live event. If it stalls,
        # replace the pending event with the final report and bound the CLI wait.
        self._flush(0.25)
        self.submit(snapshot)
        return self._flush(0.25)

    def close(self):
        with self._condition:
            self._closed = True
            self._condition.notify_all()


_LIVE_EVENTS = {
    "node_start": "running", "node_passed": "running", "node_retry": "running",
    "node_failed": "failed", "node_circuit_broken": "circuit_broken",
    "node_awaiting_subagents": "awaiting_subagents",
}


def _run_once(args, workflow, state_path, report_path, hub, wakeups, previous_worker_calls):
    from .engine import GraphFailure, GraphRunner

    def register(status, expected_updated_at=None):
        with suppress(OSError, ValueError):
            return register_run(workflow, state_path, report_path, factory=args.factory, status=status,
                                expected_updated_at=expected_updated_at)

    started = register("running")
    if hub is not None and started is not None:
        hub.set_initial_run_id(started["id"])
    runner = None
    definition = None
    status, exit_code, error, packet_path = "passed", 0, "", None
    packet_paths = ()
    outputs = {}
    last_push = float("-inf")
    event_lock = Lock()
    max_worker_calls = args.max_workers

    def on_event(event):
        nonlocal last_push
        live_status = _LIVE_EVENTS.get(event["event"])
        if live_status is None:
            return
        with event_lock:
            if args.watch and live_status == "awaiting_subagents":
                _watch_packet_outputs(outputs, event.get("packet_paths") or [event.get("packet_path")],
                                      state_path, report_path, wakeups)
            if hub is not None and (live_status != "running" or monotonic() - last_push >= 0.4):
                snapshot = _build_run_snapshot(args, workflow, state_path, report_path, definition,
                                               live_status, event=event)
                hub.submit(snapshot)
                last_push = monotonic()

    try:
        if args.max_parallel < 1:
            raise ValueError("--max-parallel must be at least 1")
        if max_worker_calls is not None:
            if max_worker_calls < 0:
                raise ValueError("--max-workers must be nonnegative")
            max_worker_calls = max(0, max_worker_calls - previous_worker_calls)
        runner = GraphRunner(
            state_path,
            retry_nodes=args.retry,
            force=args.force,
            until=args.until,
            max_worker_calls=max_worker_calls,
            max_parallel_nodes=args.max_parallel,
            logger=(lambda message: None) if args.quiet else print,
            on_event=on_event if hub is not None or args.watch else None,
        )
        with _load_workflow(workflow) as module:
            ctx = _resolve_context(module, args.ctx, args.ctx_file)
            graph = _resolve_graph(module, args.factory, ctx)
            definition = graph, ctx
            runner.run(graph, ctx)
    except GraphFailure as exc:
        status, exit_code = "failed", 1
        for flag, code in (("circuit_broken", 2), ("budget_exceeded", 3), ("awaiting_subagents", 4)):
            if getattr(exc, flag):
                status, exit_code = flag, code
                break
        error, packet_path, packet_paths = str(exc), exc.packet_path, exc.packet_paths
        if args.watch and status == "awaiting_subagents":
            _watch_packet_outputs(outputs, packet_paths, state_path, report_path, wakeups)
    except (Exception, KeyboardInterrupt, SystemExit) as exc:
        status, exit_code = "failed", 1
        error = f"{type(exc).__name__}: {exc}" if str(exc) else type(exc).__name__

    if error:
        print(f"kgraph-town: {status}: {error}", file=sys.stderr)
    if runner is None:
        # A malformed or inaccessible state file can fail initialization.
        # Emit an empty trace with the intended state path via the engine's writer.
        with TemporaryDirectory() as temp:
            runner = GraphRunner(Path(temp) / "state.json", until=args.until, max_worker_calls=max_worker_calls)
            runner.state_path = state_path
    try:
        _write_run_report(runner, report_path.resolve(), workflow, overall_status=status, error=error,
                          packet_path=packet_path, packet_paths=packet_paths, definition=definition,
                          previous_worker_calls=previous_worker_calls)
    except (OSError, ValueError):
        register("failed", started and started["updated_at"])
        if hub is not None:
            hub.finish(_build_run_snapshot(args, workflow, state_path, report_path, definition, "failed"))
        raise
    if status == "passed" and runner.reached_path is not None:
        status = "stopped"
    register(status, started and started["updated_at"])
    hub_run_id = None
    if hub is not None:
        hub_run_id = hub.finish(_build_run_snapshot(args, workflow, state_path, report_path, definition, status))
    if not args.quiet:
        print(f"{status} | report: {report_path}")
        if status == "awaiting_subagents" and len(packet_paths) > 1:
            print("Waiting packets:")
            for path in packet_paths:
                print(f"  {path}")
    return exit_code, outputs, hub_run_id, previous_worker_calls + runner.worker_calls_used


def _output_signature(path):
    try:
        info = path.stat()
    except OSError:
        return None
    return (info.st_mtime_ns, info.st_size) if S_ISREG(info.st_mode) else None


def _watch_packet_outputs(outputs, packet_paths, state, report, wakeups):
    state, report = state.resolve(), report.resolve()
    project = Path.cwd()
    # Watching files only needs a .kgraph packet, even with a custom state location.
    try:
        root = _allowed_root(state)
        packet_state = state
    except PermissionError:
        root = (project / ".kgraph").resolve()
        packet_state = root / "state.json"
    protected = {state, report, progress_path_for_state(state).resolve()}
    for raw_packet in packet_paths:
        try:
            packet_path = _resolve_packet(raw_packet, packet_state, report, project)
            packet, packet_mtime, _ = _read_packet(packet_path, limit=_PACKET_LIMIT)
        except (OSError, ValueError, RuntimeError):
            continue
        declared = packet.get("outputs")
        if isinstance(declared, dict):
            declared = list(declared.values())
        paths = list(declared) if isinstance(declared, (list, tuple)) else [declared]
        paths.extend((packet.get("output_path"), packet.get("approval_path")))
        for raw_output in paths:
            if not isinstance(raw_output, str) or not raw_output.strip() or "\0" in raw_output:
                continue
            with suppress(OSError, ValueError, RuntimeError):
                path = Path(raw_output)
                _reject_path_traversal(path)
                base = root.parent if path.parts and path.parts[0] == ".kgraph" else packet_path.parent
                path = _inside(base / path, root)
                if path in protected or path == packet_path.resolve() or path in outputs:
                    continue
                signature = _output_signature(path)
                # A responder may write the artifact before the pause event is emitted.
                if signature and signature[0] > packet_mtime and wakeups.get(path) != signature:
                    signature = None
                outputs[path] = signature


def _wait_for_outputs(args, outputs, state, report, run_id, deadline, wakeups, applied_response):
    from .network import fetch_remote_response

    last_error = None
    while True:
        changed = {path: current for path, previous in outputs.items()
                   if (current := _output_signature(path)) is not None and current != previous}
        if changed:
            wakeups.update(changed)
            return True, applied_response
        if deadline is not None and monotonic() >= deadline:
            break
        if args.hub_url and run_id:
            try:
                response = fetch_remote_response(args.hub_url, args.hub_token, run_id)
                if response is not None and response != applied_response and (deadline is None or monotonic() < deadline):
                    result = _apply_hub_response(response, state, report, skip_stale=True)
                    if result is not None:
                        return True, response
                last_error = None
            except (OSError, ValueError) as exc:
                if str(exc) != last_error:
                    last_error = str(exc)
                    print(f"kgraph-town: could not pull from Hero Town Hub: {exc}", file=sys.stderr)
        remaining = deadline - monotonic() if deadline is not None else float("inf")
        if remaining > 0:
            sleep(min(args.watch_interval, remaining))
    return False, applied_response


def _run(args):
    workflow = args.workflow.resolve()
    state_path, report_path = _run_files(args.workflow, args.state, args.report)
    if report_path.resolve() in (state_path.resolve(), workflow):
        raise ValueError("--report must be separate from the state and workflow files")
    if not isfinite(args.watch_interval) or args.watch_interval <= 0:
        raise ValueError("--watch-interval must be a finite, positive number")
    _validate_timeout(args.watch_timeout, "--watch-timeout")

    deadline = applied_response = None
    wakeups = {}
    worker_calls_used = 0
    hub = _HubPublisher(args.hub_url, args.hub_token) if args.hub_url else None
    try:
        while True:
            exit_code, outputs, hub_run_id, worker_calls_used = _run_once(
                args, workflow, state_path, report_path, hub, wakeups, worker_calls_used,
            )
            if exit_code != 4 or not args.watch:
                return exit_code
            if not outputs and not (args.hub_url and hub_run_id):
                print("kgraph-town: no packet outputs or Hero Town Hub run to watch", file=sys.stderr)
                return 4
            if deadline is None and args.watch_timeout is not None:
                deadline = monotonic() + args.watch_timeout
            try:
                ready, applied_response = _wait_for_outputs(args, outputs, state_path, report_path, hub_run_id,
                                                            deadline, wakeups, applied_response)
                if not ready:
                    print("kgraph-town: watch timed out awaiting subagents", file=sys.stderr)
                    return 4
            except KeyboardInterrupt:
                return 130
    finally:
        if hub is not None:
            hub.close()


def _town_snapshot(args, *, cached=False):
    from .snapshot import build_town_snapshot, cached_town_snapshot

    if args.workflow is None and args.state is None:
        raise ValueError(f"kgraph-town {args.command} requires a workflow or --state")
    state, report = args.state, args.report
    if args.workflow is not None:
        state, report = _run_files(args.workflow, state, report)
    if cached:
        return cached_town_snapshot(
            args.workflow, state, report, factory=args.factory,
            ctx_inline=args.ctx, ctx_file=args.ctx_file, title=args.title,
        )
    return partial(
        build_town_snapshot, args.workflow, state, report, factory=args.factory,
        ctx_inline=args.ctx, ctx_file=args.ctx_file, title=args.title,
    )


def _push(args):
    snapshot = _town_snapshot(args)()
    run = _publish_snapshot(snapshot, args.hub_url, args.token, run_id=args.run_id,
                            project_name=args.project_name, host_label=args.host_label)
    print(f"Pushed {run['id']} | {run['status']}")
    return 0


def _progress(args):
    fields = {key: getattr(args, key) for key in
              ("status", "title", "kind", "detail", "deps", "agent", "progress", "outputs",
               "question", "options", "recommended", "ticket")
              if getattr(args, key) is not None}
    data = update_node_progress(args.state, args.node, item_id=args.item, clear=args.clear, **fields)
    return _finish_progress_update(args, data)


def _ask(args):
    questions = None
    if args.questions_json is not None:
        source = args.questions_json
        if not source.lstrip().startswith(("[", "{")):
            source = Path(source).read_text(encoding="utf-8")
        questions = json.loads(source)
    data = ask_node_question(args.state, args.node, item_id=args.item, question=args.question,
                             questions=questions, title=args.title, ticket=args.ticket, detail=args.detail,
                             options=args.options or (), recommended=args.recommended, multi_select=args.multi_select,
                             allow_note=args.allow_note, agent=args.agent, deps=args.deps or ())
    return _finish_progress_update(args, data)


def _report_for_state(state):
    state = state.resolve()
    filename = "report.json" if state.stem == "state" else f"{state.stem.removesuffix('_state')}_report.json"
    return state.with_name(filename)


def _apply_hub_response(response, state, report, *, skip_stale=False):
    def reject(message):
        if not skip_stale:
            raise ValueError(message)
        return None

    node_path, packet_hash = response.get("node_path"), response.get("packet_hash")
    subtask = _subtask_path(node_path)
    if not (_valid_node_path(node_path) or subtask) or not isinstance(packet_hash, str) or not packet_hash.strip():
        return reject("Queued response must contain a valid node path and packet hash")
    state = state.resolve()
    report = report or _report_for_state(state)
    project = Path.cwd()
    payload = {**response}
    payload.pop("submitted_at", None)
    if not subtask:
        target, raw_packet = _active_gate(_optional_object(state), _optional_object(report), node_path)
        if target != node_path or not raw_packet:
            return reject("The queued response's node is no longer awaiting input")
        packet_path = _resolve_packet(raw_packet, state, report, project)
        packet = _read_packet(packet_path, limit=_PACKET_LIMIT)[0]
        gate = extract_input_gate(packet, packet_path, node_path=target, state_path=state, project_dir=project)
        if not gate or gate["packet_hash"] != packet_hash:
            return reject("The queued response does not match the active gate's packet hash")
        submitted_at = gate.get("submitted", {}).get("answered_at")
        answered_at = response.get("answered_at")
        if submitted_at and (not isinstance(answered_at, str) or answered_at <= submitted_at):
            return reject("The queued response is older than the current local decision")
        payload["submitted_at"] = submitted_at

    status, result = submit_gate_response({"state_path": state, "report_path": report, "project_dir": project}, payload)
    if status == 409:
        return reject(result["error"])
    if status != 200:
        raise ValueError(result["error"])
    return result["decision"]


def _validate_timeout(timeout, option):
    if timeout is not None and (not isfinite(timeout) or timeout < 0):
        raise ValueError(f"{option} must be a finite, nonnegative number")


def _poll_deadline(poll_ms, timeout):
    if poll_ms < 1:
        raise ValueError("--poll-ms must be at least 1")
    _validate_timeout(timeout, "--timeout")
    return monotonic() + timeout if timeout is not None else None


def _poll_again(wait, interval, deadline):
    remaining = deadline - monotonic() if deadline is not None else float("inf")
    if not wait or remaining <= 0:
        return False
    sleep(min(remaining, interval))
    return deadline is None or monotonic() < deadline


def _pull_response(args):
    from .network import fetch_remote_response

    deadline = _poll_deadline(args.poll_ms, args.timeout)
    while True:
        response = fetch_remote_response(args.hub_url, args.token, args.run_id)
        if response is not None or not _poll_again(args.wait, args.poll_ms / 1000, deadline):
            break
    if response is not None and args.apply:
        _apply_hub_response(response, args.state, args.report)
    if args.json:
        print(json.dumps(response, indent=2))
    elif response is None:
        print("No response queued.")
    else:
        print(response.get("summary") or f"Response for {args.run_id}")
    return 0 if response is not None else 4


def _answer(args):
    from .network import fetch_remote_response

    deadline = _poll_deadline(args.poll_ms, args.timeout)
    while True:
        decision = read_subtask_answer(args.state, args.node, args.item)
        if decision is None and args.hub_url and args.run_id:
            response = fetch_remote_response(args.hub_url, args.token, args.run_id)
            if response is not None and response.get("node_path") == f"{args.node}::{args.item}":
                decision = _apply_hub_response(response, args.state, None, skip_stale=True)
        remaining = deadline - monotonic() if deadline is not None else float("inf")
        if decision is not None or not args.wait or remaining <= 0:
            break
        sleep(min(remaining, args.poll_ms / 1000))
    if args.json:
        print(json.dumps(decision, indent=2))
    else:
        print(decision["summary"] if decision is not None else "No answer yet.")
    return 0 if decision is not None else 4


def _finish_progress_update(args, data):
    from .snapshot import build_town_snapshot

    if args.push or args.hub_url is not None:
        state = args.state.resolve()
        report = args.report or _report_for_state(state)
        workflow = args.workflow
        if workflow is None and report.is_file():
            reference = read_object(report).get("workflow_ref")
            if isinstance(reference, str):
                candidate = Path(reference)
                if not candidate.is_absolute():
                    candidate = next((base / candidate for base in (Path.cwd(), report.parent)
                                      if (base / candidate).is_file()), candidate)
                if candidate.is_file():
                    workflow = candidate
        snapshot = build_town_snapshot(workflow, state, report)
        snapshot["project_dir"] = str(next((parent.parent for parent in state.parents if parent.name == ".kgraph"),
                                           Path.cwd()))
        hub_url = args.hub_url or os.environ.get("KGRAPH_HUB_URL") or "http://127.0.0.1:8765"
        run = _publish_snapshot(snapshot, hub_url, args.token, run_id=args.run_id,
                                project_name=args.project_name, host_label=args.host_label)
        if not args.json:
            print(f"Pushed {run['id']} | {run['status']}")
    if args.json:
        print(json.dumps(data, indent=2))
    else:
        print(progress_path_for_state(args.state))
    return 0


def _town_ui(args):
    from .server import serve_town

    snapshot = _town_snapshot(args, cached=True)
    snapshot()  # Validate the workflow and context before binding.
    serve_town(snapshot, layout_mode=args.layout, read_only=args.read_only,
               **_serve_options(args))
    return 0


def _auth_token(args):
    from .network import generate_token

    return generate_token() if args.auth else args.token


def _serve_options(args):
    return {
        "host": "0.0.0.0" if args.lan else args.host,
        "port": args.port,
        "poll_ms": args.poll_ms,
        "open_browser": args.open,
        "token": _auth_token(args),
        "tunnel": args.tunnel,
        "no_auth": args.no_auth,
    }


def _hub(args):
    from .server import serve_hub

    serve_hub(args.watch_dir, **_serve_options(args))
    return 0


def _hub_list(args):
    from .registry import list_runs

    runs = list_runs()
    if args.json:
        print(json.dumps({"runs": runs}, indent=2))
    else:
        for run in runs:
            print(f"{run['project_name']} / {run['name']} | {run['status']} | {run['updated_at'] or '—'} | {run['id']}")
        if not runs:
            print("No registered workflow runs.")
    return 0


def _town_render(args):
    from .ui import render_town_html

    snapshot = _town_snapshot(args)()
    filename = f"{args.workflow.stem}_town.html" if args.workflow is not None else "town.html"
    output = args.output or Path(".kgraph") / filename
    protected = (snapshot["workflow_ref"], snapshot["state_file"], snapshot["report_file"], args.ctx_file)
    if any(path and output.resolve() == Path(path).resolve() for path in protected):
        raise ValueError("--output must be separate from the workflow, state, report, and context files")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(render_town_html(snapshot, embed=args.embed, layout_mode=args.layout), encoding="utf-8")
    print(output)
    return 0


def _status(args):
    data = _read_json(args.state)
    if args.json:
        print(json.dumps(data, indent=2))
        return 0

    nodes = _require_object(data.get("nodes", {}), "State nodes")
    if not nodes:
        print("No recorded nodes.")
        return 0
    rows = [("Path", "Kind", "Status", "Attempts", "Duration (s)", "Detail / error signature")]
    for path, entry in sorted(nodes.items()):
        node = _require_object(entry, f"State entry {path}")
        detail = " | ".join(str(node[key]) for key in ("detail", "error_signature") if node.get(key))
        fields = (node.get(key, "-") for key in ("kind", "status", "attempts", "duration_s"))
        rows.append((path, *fields, " ".join(detail.splitlines())))
    widths = [max(len(str(value)) for value in column) for column in list(zip(*rows))[:-1]]
    for *cells, detail in rows:
        columns = "  ".join(f"{str(cell):<{width}}" for cell, width in zip(cells, widths))
        print(f"{columns}  {detail}".rstrip())
    return 0


def _plan(args):
    from .engine import GraphFailure, GraphRunner

    state, _ = _run_files(args.workflow, args.state, None)
    try:
        with _load_workflow(args.workflow.resolve()) as module:
            ctx = _resolve_context(module, args.ctx, args.ctx_file)
            graph = _resolve_graph(module, args.factory, ctx)
            runner = GraphRunner(state, retry_nodes=args.retry, force=args.force, until=args.until)
            entries = runner.plan(graph, ctx)
    except GraphFailure as exc:
        raise ValueError(str(exc)) from exc
    if args.json:
        print(json.dumps(entries, indent=2))
    else:
        fields = ("path", "kind", "status", "reason", "resource")
        rows = [tuple(field.capitalize() for field in fields)]
        rows.extend(tuple(str(entry.get(field) if entry.get(field) is not None else "-") for field in fields)
                    for entry in entries)
        widths = [max(map(len, column)) for column in zip(*rows)]
        for row in rows:
            print("  ".join(f"{value:<{width}}" for value, width in zip(row, widths)).rstrip())
    return 0


def _qa_record(args):
    from .qa_metrics import record_qa_result, write_qa_scorecard

    checklist = record_qa_result(args.checklist, case_id=args.case, manual_qa=args.result,
                                 resolved_on_attempt=args.attempt, gap_type=args.gap,
                                 notes=args.notes, tester=args.tester)
    scorecard = None
    if args.scorecard or args.record_history:
        scorecard = write_qa_scorecard(args.checklist, record_history=args.record_history, history_path=args.history)
    if args.json:
        if scorecard is not None:
            checklist = {**checklist, "scorecard": scorecard}
        print(json.dumps(checklist, indent=2, ensure_ascii=False))
    else:
        case = next(case for case in checklist["cases"] if case["id"] == args.case.strip())
        attempt = case["resolved_on_attempt"]
        print(f"Recorded {case['manual_qa']} for {case['id']}" + (f" (attempt {attempt})" if attempt else ""))
    return 0


def _qa_scorecard(args):
    from .qa_metrics import render_qa_scorecard_markdown, write_qa_scorecard

    scorecard = write_qa_scorecard(args.checklist, json_path=args.json_out, md_path=args.md_out,
                                   record_history=args.record_history, history_path=args.history)
    if args.json:
        print(json.dumps(scorecard, indent=2, ensure_ascii=False))
    else:
        print(render_qa_scorecard_markdown(scorecard), end="")
    return 0


def _qa_history(args):
    from .qa_metrics import aggregate_qa_history

    summary = aggregate_qa_history(history_path=args.history)
    if args.json:
        print(json.dumps(summary, indent=2, ensure_ascii=False))
    else:
        print(f"QA history | {summary['batches']} batches | {summary['tested']}/{summary['total_cases']} tested")
        for label, key in (("Manual pass", "manual_pass_pct"), ("One-shot pass", "one_shot_pct"),
                           ("Within two attempts", "within_two_attempts_pct")):
            rate = summary[key]
            print(f"{label}  {f'{rate:.1f}%' if rate is not None else 'N/A'}")
        print(f"Passed {summary['passed']} | Failed {summary['failed']} | Blocked {summary['blocked']}")
        print(f"Automated pass, manual fail  {summary['auto_pass_manual_fail_count']}")
    return 0


def _report(args):
    data = _read_json(args.report)
    if args.json:
        print(json.dumps(data, indent=2))
        return 0

    print(f"Status: {data.get('status', 'unknown')}")
    for field, label in (("error", "Error"), ("packet_path", "Packet")):
        if data.get(field):
            print(f"{label}: {data[field]}")
    packet_paths = data.get("packet_paths")
    if isinstance(packet_paths, list):
        for path in packet_paths:
            if path != data.get("packet_path"):
                print(f"Packet: {path}")
    scope = data.get("scope", {})
    print(f"Scope: until={scope.get('requested_until') or '-'}, "
          f"reached={data.get('reached_path') or '-'}, "
          f"last completed={data.get('last_completed_stage') or '-'}, "
          f"nodes={len(scope.get('reached_nodes', []))}")
    summary = data.get("summary", {})
    counts = ", ".join(f"{key}={value}" for key, value in summary.items() if isinstance(value, int))
    print(f"Summary: {counts or '-'}")
    if summary.get("retried_nodes"):
        print(f"Retried nodes: {', '.join(summary['retried_nodes'])}")
    for key, label in (("failure_signatures", "Failure signatures"),
                       ("slowest_nodes", "Slowest nodes"), ("evolution_hints", "Evolution hints")):
        print(f"{label}:")
        entries = data.get(key, [])
        if not entries:
            print("  -")
        for entry in entries:
            if key == "failure_signatures":
                print(f"  {entry['node']} [{entry['error_signature'] or entry['status']}] {entry['detail'] or ''}")
            elif key == "slowest_nodes":
                print(f"  {entry['node']}  {entry['duration_s']}s  ({entry['attempts']} attempts)")
            else:
                print(f"  {entry}")
    return 0


class _ArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(1, f"{self.prog}: error: {message}\n")


def _auth_options(parser, *, allow_no_auth=False):
    auth = parser.add_mutually_exclusive_group()
    auth.add_argument("--token", help="require this access token")
    auth.add_argument("--auth", action="store_true", help="require a generated access token")
    if allow_no_auth:
        auth.add_argument("--no-auth", action="store_true", help="disable access tokens, including for tunnels")


def _bind_options(parser, host):
    parser.add_argument("--host", default=host, help=f"bind address (default: {host})")
    parser.add_argument("--port", type=int, default=8765, help="bind port (default: 8765)")
    parser.add_argument("--poll-ms", type=int, default=1200, help="refresh interval (default: 1200)")


def _serve_arguments(parser, host):
    _bind_options(parser, host)
    _auth_options(parser, allow_no_auth=True)
    parser.add_argument("--open", action="store_true", help="open Hero Town in your browser")
    parser.add_argument("--lan", action="store_true", help="bind to all IPv4 interfaces")
    parser.add_argument("--tunnel", action="store_true", help="open a public HTTPS Cloudflare quick tunnel")


def _watch_arguments(parser):
    parser.add_argument("--watch-dir", type=Path, action="append", default=[],
                        help="also discover .kgraph run files in this project (repeatable)")


def _execution_arguments(parser):
    parser.add_argument("workflow", type=Path)
    parser.add_argument("--state", type=Path, help="state file (default: .kgraph/<stem>_state.json)")
    parser.add_argument("--until", metavar="STAGE", help="stop after a stage ID or path")
    parser.add_argument("--retry", action="append", default=[], metavar="NODE", help="bypass a node's cache (repeatable)")
    parser.add_argument("--force", action="store_true", help="bypass all caches")
    parser.add_argument("--factory", default="build_graph", metavar="NAME", help="graph factory (default: build_graph)")
    parser.add_argument("--ctx", metavar="JSON", help="inline JSON object to merge into the context")
    parser.add_argument("--ctx-file", type=Path, metavar="PATH", help="JSON object file to merge before --ctx")


def _response_arguments(parser):
    parser.add_argument("--wait", action="store_true")
    parser.add_argument("--poll-ms", type=int, default=500)
    parser.add_argument("--timeout", type=float, metavar="SECONDS")
    parser.add_argument("--token", default=os.environ.get("KGRAPH_HUB_TOKEN"))
    parser.add_argument("--json", action="store_true")


def _qa_arguments(commands):
    from .qa_metrics import GAP_TYPES, MANUAL_QA_VALUES

    qa = commands.add_parser("qa", help="record manual QA and review quality metrics")
    subcommands = qa.add_subparsers(dest="qa_command", required=True)
    record = subcommands.add_parser("record", help="record a checklist result")
    record.set_defaults(handler=_qa_record)
    record.add_argument("--case", required=True, metavar="ID")
    record.add_argument("--result", required=True, choices=MANUAL_QA_VALUES)
    record.add_argument("--attempt", type=int, metavar="N")
    record.add_argument("--gap", choices=[gap for gap in GAP_TYPES if gap is not None])
    record.add_argument("--notes")
    record.add_argument("--tester")
    record.add_argument("--scorecard", action="store_true")

    scorecard = subcommands.add_parser("scorecard", help="write and show a QA scorecard")
    scorecard.set_defaults(handler=_qa_scorecard)
    scorecard.add_argument("--json-out", type=Path)
    scorecard.add_argument("--md-out", type=Path)
    for command in (record, scorecard):
        command.add_argument("checklist", type=Path)
        command.add_argument("--record-history", action="store_true")

    history = subcommands.add_parser("history", help="show aggregate QA history")
    history.set_defaults(handler=_qa_history)
    for command in (record, scorecard, history):
        command.add_argument("--history", type=Path)
        command.add_argument("--json", action="store_true")


def _parser():
    from .ui import LAYOUT_MODES

    parser = _ArgumentParser(prog="kgraph-town", description="Run and inspect resumable Python workflows.")
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("run", help="run a Python workflow")
    run.set_defaults(handler=_run)
    _execution_arguments(run)
    run.add_argument("--report", type=Path, help="report file (default: .kgraph/<stem>_report.json)")
    run.add_argument("--max-parallel", type=int, default=1, metavar="N", help="parallel sibling limit (default: 1)")
    run.add_argument("--max-workers", type=int, metavar="N", help="total worker attempt budget (default: unlimited)")
    run.add_argument("--hub-url", default=os.environ.get("KGRAPH_HUB_URL"),
                     help="stream the run to this hub (default: KGRAPH_HUB_URL)")
    run.add_argument("--hub-token", default=os.environ.get("KGRAPH_HUB_TOKEN"),
                     help="hub access token (default: KGRAPH_HUB_TOKEN)")
    run.add_argument("--quiet", action="store_true", help="suppress runner logs and the CLI summary")
    run.add_argument("--watch", action="store_true", help="resume when a waiting packet's output changes")
    run.add_argument("--watch-interval", type=float, default=0.5, metavar="SECONDS")
    run.add_argument("--watch-timeout", type=float, metavar="SECONDS")

    plan = commands.add_parser("plan", help="preview which workflow nodes would run")
    plan.set_defaults(handler=_plan)
    _execution_arguments(plan)
    plan.add_argument("--json", action="store_true")

    _qa_arguments(commands)

    ui = commands.add_parser("ui", help="serve the live Hero Town visualizer")
    ui.set_defaults(handler=_town_ui)
    _serve_arguments(ui, "127.0.0.1")
    ui.add_argument("--read-only", action="store_true", help="disable gate submissions for shared team viewing")

    hub = commands.add_parser("hub", help="serve or manage the multi-project Hero Town hub")
    hub.set_defaults(handler=_hub)
    _serve_arguments(hub, "127.0.0.1")
    _watch_arguments(hub)
    hub_commands = hub.add_subparsers(dest="hub_command")
    hub_list = hub_commands.add_parser("list", help="list registered runs")
    hub_list.set_defaults(handler=_hub_list)
    hub_list.add_argument("--json", action="store_true", help="print the run list as JSON")

    render = commands.add_parser("render", help="export a self-contained Hero Town page")
    render.set_defaults(handler=_town_render)
    render.add_argument("--output", type=Path, help="HTML path (default: .kgraph/<stem>_town.html or .kgraph/town.html)")
    render.add_argument("--embed", action="store_true", help="omit the outer page styling for inline use")

    push = commands.add_parser("push", help="send a workflow snapshot to Hero Town Hub")
    push.set_defaults(handler=_push)
    push.add_argument("--hub-url", default=os.environ.get("KGRAPH_HUB_URL") or "http://127.0.0.1:8765",
                      help="hub address (default: KGRAPH_HUB_URL or http://127.0.0.1:8765)")
    push.add_argument("--token", default=os.environ.get("KGRAPH_HUB_TOKEN"),
                      help="hub access token (default: KGRAPH_HUB_TOKEN)")
    push.add_argument("--run-id", help="stable hub run ID (default: derived from host, project, and workflow name)")
    push.add_argument("--project-name", help="project name in the hub (default: current directory name)")
    push.add_argument("--host-label", help="machine name in the hub (default: system hostname)")

    for town in (ui, render, push):
        town.add_argument("workflow", nargs="?", type=Path, help="Python workflow")
        town.add_argument("--state", type=Path, help="state JSON file")
        town.add_argument("--report", type=Path, help="run report JSON file")
        town.add_argument("--factory", default="build_graph", metavar="NAME", help="graph factory (default: build_graph)")
        town.add_argument("--ctx", metavar="JSON", help="inline JSON object to merge into the context")
        town.add_argument("--ctx-file", type=Path, metavar="PATH", help="JSON object file to merge before --ctx")
        town.add_argument("--title", help="town title (default: workflow or state filename stem)")
    for town in (ui, render):
        town.add_argument("--layout", choices=LAYOUT_MODES, default="grid", help="initial town layout (default: grid)")

    status = commands.add_parser("status", help="show persisted node states")
    status.set_defaults(handler=_status)
    status.add_argument("state", type=Path)
    status.add_argument("--json", action="store_true", help="print the full JSON document")

    progress = commands.add_parser("progress", help="report live sub-task progress to Hero Town")
    progress.set_defaults(handler=_progress)
    progress.add_argument("state", type=Path)
    progress.add_argument("--node", required=True, help="workflow node path")
    progress.add_argument("--item", help="stable sub-task ID")
    progress.add_argument("--status", choices=_PROGRESS_STATUSES)
    progress.add_argument("--kind", choices=("code", "worker", "verifier", "gate"))
    progress.add_argument("--agent")
    progress.add_argument("--progress", type=float, metavar="PERCENT")
    progress.add_argument("--output", dest="outputs", action="append", help="output path (repeatable)")
    progress.add_argument("--clear", action="store_true", help="remove the item or all items for this node")
    progress.add_argument("--question")

    ask = commands.add_parser("ask", help="request a decision during an active Hero Town stage")
    ask.set_defaults(handler=_ask)
    ask.add_argument("state", type=Path)
    ask.add_argument("--node", required=True, help="workflow node path")
    ask.add_argument("--item", required=True, help="stable sub-task ID")
    question = ask.add_mutually_exclusive_group(required=True)
    question.add_argument("--question")
    question.add_argument("--questions-json", metavar="PATH_OR_JSON")
    ask.add_argument("--multi-select", action="store_true")
    ask.add_argument("--no-note", action="store_false", dest="allow_note")
    ask.add_argument("--agent", default="Reviewer")

    for command in (progress, ask):
        command.add_argument("--title")
        command.add_argument("--detail")
        command.add_argument("--dep", dest="deps", action="append", help="dependency item ID (repeatable)")
        command.add_argument("--option", dest="options", action="append", help="answer choice (repeatable)")
        command.add_argument("--recommended")
        command.add_argument("--ticket")
        command.add_argument("--push", action="store_true", help="publish to Hero Town Hub")
        command.add_argument("--workflow", type=Path)
        command.add_argument("--report", type=Path)
        command.add_argument("--hub-url", help="publish to this Hub instead of KGRAPH_HUB_URL")
        command.add_argument("--token", default=os.environ.get("KGRAPH_HUB_TOKEN"))
        command.add_argument("--run-id")
        command.add_argument("--project-name")
        command.add_argument("--host-label")
        command.add_argument("--json", action="store_true", help="print the progress sidecar as JSON")

    answer = commands.add_parser("answer", help="read a submitted sub-task decision")
    answer.set_defaults(handler=_answer)
    answer.add_argument("state", type=Path)
    answer.add_argument("--node", required=True, help="workflow node path")
    answer.add_argument("--item", required=True, help="stable sub-task ID")
    answer.add_argument("--hub-url", default=os.environ.get("KGRAPH_HUB_URL"))
    answer.add_argument("--run-id", help="Hub run ID to check for a queued decision")
    _response_arguments(answer)

    pull = commands.add_parser("pull-response", help="retrieve a queued Hub decision and apply it locally")
    pull.set_defaults(handler=_pull_response, apply=True)
    pull.add_argument("state", type=Path)
    pull.add_argument("--run-id", required=True)
    pull.add_argument("--hub-url", default=os.environ.get("KGRAPH_HUB_URL") or "http://127.0.0.1:8765")
    pull.add_argument("--report", type=Path)
    apply = pull.add_mutually_exclusive_group()
    apply.add_argument("--apply", action="store_true", help="apply the decision locally (default)")
    apply.add_argument("--no-apply", dest="apply", action="store_false", help="show the decision without writing it")
    _response_arguments(pull)

    report = commands.add_parser("report", help="summarize a run report")
    report.set_defaults(handler=_report)
    report.add_argument("report", type=Path)
    report.add_argument("--json", action="store_true", help="print the full JSON document")

    return parser


def main(argv=None):
    args = _parser().parse_args(argv)
    try:
        return args.handler(args)
    except (OSError, ValueError, TypeError, KeyError, AttributeError, ImportError) as exc:
        print(f"kgraph-town: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
