"""Reusable helpers for common workflow tasks."""

import hashlib
import json
import os
import subprocess
from pathlib import Path
from tempfile import NamedTemporaryFile

from .engine import GraphFailure, TaskNode


DEFAULT_IGNORE_DIRS = frozenset({
    ".git", ".kgraph", ".venv", "venv", "node_modules", "__pycache__",
    "Library", "Temp", "Obj", "Build", "Logs",
})


def _relative_path(path, root):
    try:
        return Path(os.path.relpath(path, root))
    except ValueError:
        # Windows cannot make paths relative across drives.
        return path


def _ignored(path, root, ignore_dirs):
    return any(part in ignore_dirs for part in _relative_path(path, root).parts)


def hash_paths(paths, *, root=None, ignore_dirs=DEFAULT_IGNORE_DIRS) -> str:
    """Hash file names relative to root and their contents in sorted order."""
    root = Path(root or Path.cwd()).absolute()
    ignore_dirs = frozenset(ignore_dirs)
    files = set()
    for value in paths:
        path = Path(os.path.abspath(root / value))
        if _ignored(path.parent, root, ignore_dirs):
            continue
        if path.is_file():
            files.add(path)
        elif path.is_dir() and not path.is_symlink() and not _ignored(path, root, ignore_dirs):
            for directory, children, names in os.walk(path):
                children[:] = [name for name in children if name not in ignore_dirs]
                for name in names:
                    file = Path(directory) / name
                    if file.is_file():
                        files.add(file)

    digest = hashlib.sha256()
    for relative, path in sorted((_relative_path(path, root).as_posix(), path) for path in files):
        content = hashlib.sha256()
        with path.open("rb") as file:
            for chunk in iter(lambda: file.read(1024 * 1024), b""):
                content.update(chunk)
        digest.update(os.fsencode(relative) + b"\0" + content.digest())
    return digest.hexdigest()


def hash_globs(patterns, *, root=None, ignore_dirs=DEFAULT_IGNORE_DIRS) -> str:
    """Hash files matched by glob patterns relative to root."""
    root = Path(root or Path.cwd()).absolute()
    return hash_paths(
        (path for pattern in patterns for path in root.glob(pattern)),
        root=root,
        ignore_dirs=ignore_dirs,
    )


def _no_outputs(_ctx):
    return ()


def shell_task(
    id, command, *, input_hash_fn, outputs_fn=_no_outputs, cwd=None, env=None,
    kind="code", deps=(), timeout_s=None, max_retries=0, resource=None,
    cacheable=True, log_path=None, verify_fn=None, repair_fn=None,
) -> TaskNode:
    """Create a task that checks a command's exit code and optional verifier."""

    def write_log(stdout, stderr):
        if log_path is None:
            return
        streams = []
        for value in (stdout, stderr):
            if value:
                if isinstance(value, bytes):
                    value = value.decode(errors="replace").replace("\r\n", "\n").replace("\r", "\n")
                streams.append(value.rstrip("\n"))
        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)
        log_file.write_text("\n".join(streams) + ("\n" if streams else ""), encoding="utf-8")

    def run(_ctx, _attempt):
        try:
            result = subprocess.run(
                command, shell=isinstance(command, str), cwd=cwd,
                env=None if env is None else {**os.environ, **env},
                timeout=timeout_s, capture_output=True, text=True, errors="replace",
            )
        except subprocess.TimeoutExpired as exc:
            write_log(exc.stdout, exc.stderr)
            raise TimeoutError(f"Command timed out after {timeout_s}s") from exc
        write_log(result.stdout, result.stderr)
        return result

    def verify(ctx, result):
        if result.returncode:
            detail = (result.stderr or result.stdout or "").strip()
            message = f"Command exited with code {result.returncode}"
            return False, f"exit_{result.returncode}", f"{message}: {detail}" if detail else message
        if verify_fn is not None:
            return verify_fn(ctx, result)
        return True, "", "Command passed"

    return TaskNode(
        id=id, kind=kind, run_fn=run, input_hash_fn=input_hash_fn,
        outputs_fn=outputs_fn, verify_fn=verify, repair_fn=repair_fn,
        max_retries=max_retries, deps=tuple(deps), resource=resource, cacheable=cacheable,
    )


def subagent_gate(
    id, *, packet_path, artifact_paths, packet_fn, input_hash_fn, deps=(),
    verify_fn=None, resource=None, cacheable=True,
) -> TaskNode:
    """Write a JSON packet and pause until the expected artifacts pass verification."""
    if callable(artifact_paths):
        def resolve_paths(ctx):
            return tuple(Path(path) for path in artifact_paths(ctx))
    else:
        paths = tuple(Path(path) for path in artifact_paths)

        def resolve_paths(_ctx):
            return paths

    packet = Path(packet_path)

    def run(ctx, _attempt):
        artifacts = resolve_paths(ctx)
        missing = [str(path) for path in artifacts if not path.exists()]
        if missing:
            reason = "Waiting for artifacts: " + ", ".join(missing)
        elif verify_fn is not None:
            ok, signature, detail = verify_fn(ctx, artifacts)
            if ok:
                return detail
            reason = detail or signature or "Artifact verification failed"
        else:
            return "Artifacts present"

        packet.parent.mkdir(parents=True, exist_ok=True)
        draft = None
        try:
            with NamedTemporaryFile(mode="w", encoding="utf-8", dir=packet.parent, delete=False) as file:
                draft = Path(file.name)
                json.dump(packet_fn(ctx), file, indent=2)
                file.write("\n")
            draft.replace(packet)
        finally:
            if draft is not None:
                draft.unlink(missing_ok=True)
        raise GraphFailure(
            id, reason, awaiting_subagents=True, packet_path=str(packet),
            efficiency={"agent_dispatches": 1},
        )

    return TaskNode(
        id=id, kind="gate", run_fn=run, input_hash_fn=input_hash_fn,
        outputs_fn=resolve_paths, verify_fn=lambda _ctx, detail: (True, "", detail),
        deps=tuple(deps), resource=resource, cacheable=cacheable,
    )
