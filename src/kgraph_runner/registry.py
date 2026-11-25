"""Machine-wide inventory of runs and their on-disk telemetry."""

from contextlib import contextmanager
from datetime import datetime, timezone
from hashlib import sha256
import json
import ntpath
import os
from pathlib import Path
import posixpath
import re
import socket

from ._files import file_lock, require_id, valid_id, write_json
from .interactions import progress_path_for_state
from .snapshot import _object, _timestamp


def kgraph_home():
    return Path(os.environ.get("KGRAPH_HOME") or Path.home() / ".kgraph").expanduser().resolve()


def _read(path):
    try:
        return _object(json.loads(path.read_text(encoding="utf-8")))
    except (OSError, ValueError, RecursionError):
        return {}


@contextmanager
def _locked_registry():
    path = kgraph_home() / "registry.json"
    with file_lock(path.with_suffix(".lock")):
        data = _read(path)
        yield path, _object(data.get("runs")), _object(data.get("dismissed"))


def _write_registry(path, runs, dismissed):
    data = {"version": 1, "runs": runs}
    if dismissed:
        data["dismissed"] = dismissed
    write_json(path, data)


def _slug(value):
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")[:48] or "run"


def _entry(workflow, state, report, project, factory, status, updated_at, name=None, project_name=None):
    name = name or (workflow.stem if workflow else state.stem.removesuffix("_state"))
    project_name = project_name or project.name or str(project)
    digest = sha256(os.fsencode(state)).hexdigest()[:8]
    return {
        "id": f"{_slug(project_name)}-{_slug(name)}-{digest}",
        "name": name,
        "project_name": project_name,
        "project_dir": str(project),
        "workflow_path": str(workflow) if workflow is not None else None,
        "state_path": str(state),
        "report_path": str(report),
        "factory": factory,
        "status": status,
        "updated_at": updated_at,
    }


def register_run(workflow_path, state_path, report_path, *, project_dir=None,
                 factory="build_graph", status="running", name=None, project_name=None,
                 expected_updated_at=None):
    """Upsert a run and return its entry. An optional timestamp guards stale completions."""
    workflow, state, report = (Path(path).resolve() for path in (workflow_path, state_path, report_path))
    project = Path(project_dir or Path.cwd()).resolve()
    with _locked_registry() as (path, runs, dismissed):
        entry = _entry(workflow, state, report, project, factory, status,
                       datetime.now(timezone.utc).isoformat(), name, project_name)
        previous = _object(runs.get(entry["id"]))
        if expected_updated_at is None or previous.get("updated_at") == expected_updated_at:
            runs[entry["id"]] = entry
            dismissed.pop(os.path.normcase(entry["state_path"]), None)
            _write_registry(path, runs, dismissed)
            return entry
        return previous or None


def _snapshot_state_path(snapshot, *, local):
    state = snapshot.get("state_file")
    if not isinstance(state, str) or not state.strip() or "\0" in state:
        return None

    # Preserve the source machine's path semantics when the hub is on a different OS.
    windows = bool(ntpath.splitdrive(state)[0] or "\\" in state) or (local and os.name == "nt")
    paths = ntpath if windows else posixpath
    if not paths.isabs(state):
        base = snapshot.get("project_dir")
        if not isinstance(base, str) or not base:
            workflow = snapshot.get("workflow_ref")
            if isinstance(workflow, str) and (ntpath.splitdrive(workflow)[0] or "\\" in workflow):
                paths = ntpath
            base = paths.dirname(workflow) if isinstance(workflow, str) else ""
        if not base and local:
            base = str(Path.cwd())
        if base and (ntpath.splitdrive(base)[0] or "\\" in base):
            paths = ntpath
        if not paths.isabs(base):
            return None
        state = paths.join(base, state)
    if local and paths is (ntpath if os.name == "nt" else posixpath):
        try:
            state = str(Path(state).resolve())
        except (OSError, ValueError, RuntimeError):
            pass
    return paths.normcase(paths.normpath(state))


def _run_key(entry, local_host):
    host = entry.get("source_host") if entry.get("source") == "remote" else local_host
    if not isinstance(host, str):
        return None
    host = host.casefold()
    state = entry.get("state_path")
    if not state and entry.get("source") == "remote":
        snapshot_path = entry.get("remote_snapshot_path")
        if isinstance(snapshot_path, str):
            # Snapshots from older hubs kept the source path only in the payload.
            state = _snapshot_state_path(_read(Path(snapshot_path)), local=host == local_host)
    else:
        state = _snapshot_state_path({"state_file": state}, local=host == local_host)
    return (host, state) if state else None


def ingest_remote_run(run_meta: dict, snapshot: dict):
    """Persist a pushed snapshot and return its hub registry entry."""
    if not isinstance(run_meta, dict) or not isinstance(snapshot, dict):
        raise ValueError("run and snapshot must be JSON objects")

    def field(key, default):
        value = run_meta.get(key, default)
        if not isinstance(value, str) or not value.strip():
            raise ValueError(f"run.{key} must be a nonempty string")
        return value.strip()

    source_host = field("source_host", "Unknown host")
    local_host = socket.gethostname().casefold()
    host = source_host.casefold()
    state_path = _snapshot_state_path(snapshot, local=host == local_host)
    project_name = field("project_name", "Remote project")
    name = field("name", snapshot.get("title") or "workflow")
    status = field("status", snapshot.get("status") or "pending")
    run_id = run_meta.get("run_id", run_meta.get("id"))
    if "run_id" in run_meta or "id" in run_meta:
        # Windows reserves these filenames even with a .json suffix.
        if (not isinstance(run_id, str) or len(run_id) > 128 or
                re.fullmatch(r"con|prn|aux|nul|(?:com|lpt)[0-9]", run_id, re.IGNORECASE)):
            raise ValueError("run ID must be a safe slug of at most 128 characters")
        require_id(run_id)
    else:
        identity = json.dumps([host, state_path] if state_path else [host, project_name, name]).encode("ascii")
        digest = sha256(identity).hexdigest()[:16]
        run_id = f"remote-{_slug(project_name)}-{_slug(name)}-{digest}"

    with _locked_registry() as (path, runs, dismissed):
        if state_path:
            matches = [(key, existing) for key, existing in runs.items()
                       if _run_key(_object(existing), local_host) == (host, state_path)]
            if matches:
                run_id, _ = max(matches, key=lambda item: (
                    "report_path" in item[1], item[0] == run_id, _timestamp(item[1].get("updated_at")),
                ))
        for existing_id, existing in runs.items():
            if existing_id.casefold() != run_id.casefold():
                continue
            existing = _object(existing)
            existing_key = _run_key(existing, local_host)
            if existing_key and existing_key == (host, state_path):
                continue
            existing_host = existing.get("source_host")
            if (existing_id != run_id or existing.get("source") != "remote"
                    or not isinstance(existing_host, str) or existing_host.casefold() != host
                    or (state_path and existing_key)):
                raise ValueError("run ID conflicts with an existing run")
        snapshot_path = path.parent / "remote" / f"{run_id}.json"
        previous = _object(runs.get(run_id))
        entry = {
            **previous,
            "id": run_id,
            "name": name,
            "project_name": project_name,
            "source": "remote",
            "source_host": source_host,
            "remote_snapshot_path": str(snapshot_path),
            "status": status,
            "updated_at": datetime.now(timezone.utc).isoformat(),
        }
        if state_path:
            entry["state_path"] = state_path
        write_json(snapshot_path, snapshot, indent=None)
        runs[run_id] = entry
        if host == local_host:
            dismissed.pop(state_path, None)
        _write_registry(path, runs, dismissed)
        return entry


def remove_run(run_id, watch_dirs=()):
    """Remove a registry entry, returning whether it existed."""
    if not valid_id(run_id):
        return False
    local_host = socket.gethostname().casefold()
    with _locked_registry() as (path, runs, dismissed):
        entry = runs.get(run_id)
        watched = {run["id"]: run for directory in watch_dirs for run in _discover(directory)}
        if entry is None:
            entry = watched.get(run_id)
            if entry is None or os.path.normcase(entry["state_path"]) in dismissed:
                return False
        key = _run_key(_object(entry), local_host)
        ids = {run_id}
        if key:
            ids.update(other_id for other_id, other in runs.items() if _run_key(_object(other), local_host) == key)
            for run in watched.values():
                if _run_key(run, local_host) == key:
                    dismissed[os.path.normcase(run["state_path"])] = True
        for other_id in ids:
            if valid_id(other_id):
                (path.parent / "remote" / f"{other_id}.json").unlink(missing_ok=True)
            runs.pop(other_id, None)
        _write_registry(path, runs, dismissed)
        return True


def _modified(path):
    try:
        return datetime.fromtimestamp(path.stat().st_mtime, timezone.utc)
    except OSError:
        return None


_STATUS_ORDER = (
    "running", "circuit_broken", "budget_exceeded", "failed", "awaiting_subagents",
    "pending", "passed", "cached",
)


def _live_entry(entry):
    state_path, report_path = Path(entry["state_path"]), Path(entry["report_path"])
    state_time, report_time = _modified(state_path), _modified(report_path)
    if state_time is None and report_time is None:
        return {**entry, "status": entry["status"] if entry["status"] == "running" else "missing"}

    report, state = _read(report_path), _read(state_path)
    stamped_report = _timestamp(report.get("generated_at"))
    if stamped_report.year > 1:
        report_time = stamped_report

    updated = _timestamp(entry["updated_at"])
    status = entry["status"]
    if report_time and report_time >= updated and isinstance(report.get("status"), str):
        updated, status = report_time, report["status"]

    records = (_object(node) for node in _object(state.get("nodes")).values())
    fresh = []
    for node in records:
        stamp = _timestamp(node.get("updated_at") or node.get("started_at"))
        if stamp.year == 1:
            stamp = state_time
        if stamp and stamp > updated and isinstance(node.get("status"), str):
            fresh.append((stamp, node["status"]))
    if fresh:
        updated = max(stamp for stamp, _ in fresh)
        statuses = {status for _, status in fresh}
        # Consecutive node completions belong to an active CLI run until its
        # final report is published. Failure and pause states remain visible.
        if status == "running" and statuses <= {"passed", "cached", "pending"}:
            statuses.add("running")
        status = next((value for value in _STATUS_ORDER if value in statuses), status)
    if updated.year == 1:
        updated = max(stamp for stamp in (state_time, report_time) if stamp is not None)
    progress_time = _modified(progress_path_for_state(state_path))
    if progress_time and progress_time > updated:
        updated = progress_time
    return {**entry, "status": status, "updated_at": updated.isoformat()}

def _discover(directory):
    project = Path(directory).expanduser().resolve()
    for state in (project / ".kgraph").glob("*_state.json"):
        stem = state.stem.removesuffix("_state")
        report = state.with_name(f"{stem}_report.json")
        source = _read(report).get("workflow_ref")
        workflow = project / source if isinstance(source, str) and source else project / f"{stem}.py"
        yield _entry(workflow.resolve(), state.resolve(), report.resolve(), project, "build_graph", "pending", None)


def list_runs(watch_dirs=()):
    """List registered and watched runs with current telemetry, newest first."""
    data = _read(kgraph_home() / "registry.json")
    registry = _object(data.get("runs"))
    dismissed = _object(data.get("dismissed"))
    common = ("id", "name", "project_name", "status", "updated_at")
    local = ("project_dir", "workflow_path", "state_path", "report_path", "factory")
    remote = ("source_host", "remote_snapshot_path")
    entries = {}
    for run_id, entry in registry.items():
        if isinstance(entry, dict):
            fields = remote if entry.get("source") == "remote" else local
            if all(isinstance(entry.get(field), str) for field in common + fields):
                entries[run_id] = entry
    state_paths = {entry["state_path"] for entry in entries.values() if entry.get("source") != "remote"}
    for directory in watch_dirs:
        for entry in _discover(directory):
            state = entry["state_path"]
            if state not in state_paths and entry["id"] not in entries and os.path.normcase(state) not in dismissed:
                entries[entry["id"]] = entry
                state_paths.add(state)

    def with_telemetry(entry):
        if entry.get("source") != "remote":
            return _live_entry(entry)
        if all(isinstance(entry.get(field), str) for field in local):
            live = _live_entry(entry)
            if _timestamp(live["updated_at"]) > _timestamp(entry["updated_at"]):
                return {**live, "source": "local"}
        return entry

    def priority(run):
        return _timestamp(run["updated_at"]), run.get("source") == "remote", run["id"]

    local_host = socket.gethostname().casefold()
    seen, runs = set(), []
    for entry in sorted(map(with_telemetry, entries.values()), key=priority, reverse=True):
        key = _run_key(entry, local_host)
        if key is None or key not in seen:
            runs.append(entry)
            seen.add(key)
    return runs


def get_run(run_id, watch_dirs=()):
    """Return one enriched run, or None."""
    return next((run for run in list_runs(watch_dirs) if run["id"] == run_id), None)
