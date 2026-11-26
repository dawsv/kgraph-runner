"""Reusable helpers for common workflow tasks."""

import codecs
import hashlib
import io
import json
import os
import signal
import subprocess
import time
from pathlib import Path
from tempfile import NamedTemporaryFile
from threading import Lock, Thread

from .engine import GraphFailure, TaskNode


DEFAULT_IGNORE_DIRS = frozenset({
    ".git", ".kgraph", ".venv", "venv", "node_modules", "__pycache__",
    "Library", "Temp", "Obj", "Build", "Logs",
})
_HASH_CACHE_MAX_ENTRIES = 16_384
_HASH_CACHE_MIN_AGE_NS = 2_000_000_000
_hash_cache = {}
_hash_cache_lock = Lock()


def clear_hash_cache():
    """Discard all cached file digests."""
    with _hash_cache_lock:
        _hash_cache.clear()


def _relative_path(path, root):
    try:
        return Path(os.path.relpath(path, root))
    except ValueError:
        # Windows cannot make paths relative across drives.
        return path


def _ignored(path, root, ignore_dirs):
    return any(part in ignore_dirs for part in _relative_path(path, root).parts)


def _stat_identity(stat):
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def _file_digest(path, use_cache):
    key = None
    if use_cache:
        stat = path.stat()
        # Recent files can change without a distinguishable timestamp update.
        if time.time_ns() - stat.st_mtime_ns >= _HASH_CACHE_MIN_AGE_NS:
            key = (str(path.resolve()), *_stat_identity(stat))
            with _hash_cache_lock:
                cached = _hash_cache.get(key)
            if cached is not None:
                return cached

    content = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            content.update(chunk)
        stable = key is not None and _stat_identity(os.fstat(file.fileno())) == key[1:]
    digest = content.digest()
    if stable:
        with _hash_cache_lock:
            if len(_hash_cache) >= _HASH_CACHE_MAX_ENTRIES:
                _hash_cache.clear()
            _hash_cache[key] = digest
    return digest


def hash_paths(paths, *, root=None, ignore_dirs=DEFAULT_IGNORE_DIRS, exclude_paths=(), use_cache: bool = True) -> str:
    """Hash sorted file names and contents, skipping ignored directories and excluded files."""
    root = Path(root or Path.cwd()).absolute()
    ignore_dirs = frozenset(ignore_dirs)
    excluded = {Path(os.path.abspath(root / value)) for value in exclude_paths}
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
    for relative, path in sorted((_relative_path(path, root).as_posix(), path) for path in files - excluded):
        digest.update(os.fsencode(relative) + b"\0" + _file_digest(path, use_cache))
    return digest.hexdigest()


def hash_globs(patterns, *, root=None, ignore_dirs=DEFAULT_IGNORE_DIRS, exclude_paths=(), use_cache: bool = True) -> str:
    """Hash files matched by glob patterns relative to root."""
    root = Path(root or Path.cwd()).absolute()
    return hash_paths(
        (path for pattern in patterns for path in root.glob(pattern)),
        root=root,
        ignore_dirs=ignore_dirs,
        exclude_paths=exclude_paths,
        use_cache=use_cache,
    )


def _no_outputs(_ctx):
    return ()


def _stream_command(command, cwd, env, timeout_s, log_path):
    log_path = Path(log_path)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    process_group = os.name == "posix"
    with subprocess.Popen(
        command, shell=isinstance(command, str), cwd=cwd, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
        start_new_session=process_group, bufsize=0,
    ) as proc:
        lock = Lock()
        stdout, stderr = io.StringIO(), io.StringIO()
        stream_error = None
        accepting_output = True

        def drain(pipe, output):
            nonlocal stream_error
            decode = codecs.getincrementaldecoder(pipe.encoding)(errors="replace")
            decoder = io.IncrementalNewlineDecoder(decode, translate=True)
            while True:
                try:
                    chunk = pipe.buffer.read(64 * 1024)
                except (OSError, ValueError) as exc:
                    with lock:
                        if accepting_output and stream_error is None:
                            stream_error = exc
                    return
                value = decoder.decode(chunk, final=not chunk)
                with lock:
                    if not accepting_output:
                        return
                    if value:
                        output.write(value)
                        if stream_error is None:
                            try:
                                log.write(value)
                                log.flush()
                            except OSError as exc:
                                stream_error = exc
                if not chunk:
                    return

        def kill():
            if process_group:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                proc.kill()

        readers = []
        try:
            with log_path.open("w", encoding="utf-8") as log:
                timed_out = None
                try:
                    deadline = None if timeout_s is None else time.monotonic() + timeout_s
                    for pipe, output in ((proc.stdout, stdout), (proc.stderr, stderr)):
                        reader = Thread(target=drain, args=(pipe, output), daemon=True)
                        reader.start()
                        readers.append(reader)
                    proc.wait(timeout=timeout_s)
                    for reader in readers:
                        reader.join(None if deadline is None else max(0, deadline - time.monotonic()))
                        if reader.is_alive():
                            raise subprocess.TimeoutExpired(command, timeout_s)
                except subprocess.TimeoutExpired as exc:
                    timed_out = exc
                    kill()
                except BaseException:
                    kill()
                    raise
                finally:
                    # A descendant outside the process group can still hold a pipe open.
                    drain_deadline = time.monotonic() + 1
                    for reader in readers:
                        reader.join(max(0, drain_deadline - time.monotonic()))
                    with lock:
                        accepting_output = False
                proc.wait()
                out, err = stdout.getvalue(), stderr.getvalue()
                if stream_error is not None:
                    raise stream_error
                # Retain the completed log's stdout-first layout for existing readers.
                streams = [value.rstrip("\n") for value in (out, err) if value]
                log.seek(0)
                log.write("\n".join(streams) + ("\n" if streams else ""))
                log.truncate()
                if timed_out is not None:
                    raise timed_out
        except BaseException:
            if proc.poll() is None:
                kill()
            raise

    return subprocess.CompletedProcess(command, proc.returncode, out, err)


def shell_task(
    id, command, *, input_hash_fn, outputs_fn=_no_outputs, cwd=None, env=None,
    kind="code", deps=(), timeout_s=None, max_retries=0, resource=None,
    cacheable=True, log_path=None, verify_fn=None, repair_fn=None,
) -> TaskNode:
    """Create a task that checks a static or context-derived command and streams its log."""

    def run(ctx, _attempt):
        cmd = command(ctx) if callable(command) else command
        command_env = None if env is None else {**os.environ, **env}
        try:
            if log_path is not None:
                return _stream_command(cmd, cwd, command_env, timeout_s, log_path)
            return subprocess.run(
                cmd, shell=isinstance(cmd, str), cwd=cwd, env=command_env,
                timeout=timeout_s, capture_output=True, text=True, errors="replace",
            )
        except subprocess.TimeoutExpired as exc:
            raise TimeoutError(f"Command timed out after {timeout_s}s") from exc

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
