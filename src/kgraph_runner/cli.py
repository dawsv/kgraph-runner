"""Command-line entry points for standalone KGraph and Hero Town."""

import argparse
from contextlib import suppress
from functools import partial
import json
from math import isfinite
import os
from pathlib import Path
import socket
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep

from ._files import read_object
from .interactions import (
    _PROGRESS_STATUSES, ask_node_question, progress_path_for_state, read_subtask_answer, update_node_progress,
)
from .registry import register_run
from .workflow import _load_workflow, _read_json, _require_object, _resolve_context, _resolve_graph


def _run_files(workflow, state, report):
    directory = Path(".kgraph")
    return state or directory / f"{workflow.stem}_state.json", report or directory / f"{workflow.stem}_report.json"


def _write_run_report(runner, path, workflow, **kwargs):
    # The engine report summarizes cache hits but does not identify them by
    # path. Add the latest trace status before publishing the report atomically.
    path.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=path.parent) as directory:
        draft = runner.write_run_report(Path(directory) / "report.json", **kwargs)
        report = _read_json(draft)
        latest = {event["node"]: event["status"] for event in runner.trace}
        report["cached_nodes"] = [node for node, status in latest.items() if status == "cached"]
        report["workflow_ref"] = str(workflow)
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


def _push_completed_run(args, workflow, state, report, definition, status, run_id):
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
        _publish_snapshot(snapshot, args.hub_url, args.hub_token, run_id=run_id)
    except Exception as exc:
        print(f"kgraph-town: could not report to Hero Town Hub: {exc}", file=sys.stderr)


def _run(args):
    from .engine import GraphFailure, GraphRunner

    workflow = args.workflow.resolve()
    state_path, report_path = _run_files(args.workflow, args.state, args.report)
    if report_path.resolve() in (state_path.resolve(), workflow):
        raise ValueError("--report must be separate from the state and workflow files")

    def register(status, expected_updated_at=None):
        with suppress(OSError, ValueError):
            return register_run(workflow, state_path, report_path, factory=args.factory, status=status,
                                expected_updated_at=expected_updated_at)

    started = register("running")
    run_id = started["id"] if started else None
    runner = None
    definition = None
    status, exit_code, error, packet_path = "passed", 0, "", None
    try:
        if args.max_parallel < 1:
            raise ValueError("--max-parallel must be at least 1")
        if args.max_workers is not None and args.max_workers < 0:
            raise ValueError("--max-workers must be nonnegative")
        runner = GraphRunner(
            state_path,
            retry_nodes=args.retry,
            force=args.force,
            until=args.until,
            max_worker_calls=args.max_workers,
            max_parallel_nodes=args.max_parallel,
            logger=(lambda message: None) if args.quiet else print,
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
        error, packet_path = str(exc), exc.packet_path
    except (Exception, KeyboardInterrupt, SystemExit) as exc:
        status, exit_code = "failed", 1
        error = f"{type(exc).__name__}: {exc}" if str(exc) else type(exc).__name__

    if error:
        print(f"kgraph-town: {status}: {error}", file=sys.stderr)
    if runner is None:
        # A malformed or inaccessible state file can fail initialization.
        # Emit an empty trace with the intended state path via the engine's writer.
        with TemporaryDirectory() as temp:
            runner = GraphRunner(Path(temp) / "state.json", until=args.until, max_worker_calls=args.max_workers)
            runner.state_path = state_path
    try:
        _write_run_report(runner, report_path.resolve(), workflow,
                          overall_status=status, error=error, packet_path=packet_path)
    except (OSError, ValueError):
        register("failed", started and started["updated_at"])
        if args.hub_url:
            _push_completed_run(args, workflow, state_path, report_path, definition, "failed", run_id)
        raise
    if status == "passed" and runner.reached_path is not None:
        status = "stopped"
    register(status, started and started["updated_at"])
    if args.hub_url:
        _push_completed_run(args, workflow, state_path, report_path, definition, status, run_id)
    if not args.quiet:
        print(f"{status} | report: {report_path}")
    return exit_code


def _town_snapshot(args):
    from .snapshot import build_town_snapshot

    if args.workflow is None and args.state is None:
        raise ValueError(f"kgraph-town {args.command} requires a workflow or --state")
    state, report = args.state, args.report
    if args.workflow is not None:
        state, report = _run_files(args.workflow, state, report)
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


def _answer(args):
    if args.poll_ms < 1:
        raise ValueError("--poll-ms must be at least 1")
    if args.timeout is not None and (not isfinite(args.timeout) or args.timeout < 0):
        raise ValueError("--timeout must be a finite, nonnegative number")
    deadline = monotonic() + args.timeout if args.timeout is not None else None
    while True:
        decision = read_subtask_answer(args.state, args.node, args.item)
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
        filename = "report.json" if state.stem == "state" else f"{state.stem.removesuffix('_state')}_report.json"
        report = args.report or state.with_name(filename)
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

    snapshot = _town_snapshot(args)
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


def _report(args):
    data = _read_json(args.report)
    if args.json:
        print(json.dumps(data, indent=2))
        return 0

    print(f"Status: {data.get('status', 'unknown')}")
    for field, label in (("error", "Error"), ("packet_path", "Packet")):
        if data.get(field):
            print(f"{label}: {data[field]}")
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


def _parser():
    from .ui import LAYOUT_MODES

    parser = _ArgumentParser(prog="kgraph-town", description="Run and inspect resumable Python workflows.")
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("run", help="run a Python workflow")
    run.set_defaults(handler=_run)
    run.add_argument("workflow", type=Path)
    run.add_argument("--state", type=Path, help="state file (default: .kgraph/<stem>_state.json)")
    run.add_argument("--report", type=Path, help="report file (default: .kgraph/<stem>_report.json)")
    run.add_argument("--until", metavar="STAGE", help="stop after a stage ID or path")
    run.add_argument("--retry", action="append", default=[], metavar="NODE", help="bypass a node's cache (repeatable)")
    run.add_argument("--force", action="store_true", help="bypass all caches")
    run.add_argument("--max-parallel", type=int, default=1, metavar="N", help="parallel sibling limit (default: 1)")
    run.add_argument("--max-workers", type=int, metavar="N", help="total worker attempt budget (default: unlimited)")
    run.add_argument("--factory", default="build_graph", metavar="NAME", help="graph factory (default: build_graph)")
    run.add_argument("--ctx", metavar="JSON", help="inline JSON object to merge into the context")
    run.add_argument("--ctx-file", type=Path, metavar="PATH", help="JSON object file to merge before --ctx")
    run.add_argument("--hub-url", default=os.environ.get("KGRAPH_HUB_URL"),
                     help="report the run outcome to this hub (default: KGRAPH_HUB_URL)")
    run.add_argument("--hub-token", default=os.environ.get("KGRAPH_HUB_TOKEN"),
                     help="hub access token (default: KGRAPH_HUB_TOKEN)")
    run.add_argument("--quiet", action="store_true", help="suppress runner logs and the CLI summary")

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
    answer.add_argument("--wait", action="store_true")
    answer.add_argument("--poll-ms", type=int, default=500)
    answer.add_argument("--timeout", type=float, metavar="SECONDS")
    answer.add_argument("--json", action="store_true")

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
