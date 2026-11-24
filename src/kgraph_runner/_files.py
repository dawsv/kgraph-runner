"""File and identifier conventions shared by the workflow builders."""

from contextlib import contextmanager
import json
import os
import re
from pathlib import Path
from tempfile import NamedTemporaryFile


def valid_id(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_-]*", value) is not None


def require_id(value):
    if not valid_id(value):
        raise ValueError(f"Invalid ID {value!r}: use letters, digits, underscores, or hyphens")
    return value


def read_object(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return data


def write_text(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    draft = None
    try:
        with NamedTemporaryFile(mode="w", encoding="utf-8", newline="\n", dir=path.parent, delete=False) as file:
            draft = Path(file.name)
            file.write(content)
        draft.replace(path)
    finally:
        if draft is not None:
            draft.unlink(missing_ok=True)


def write_json(path, data, *, ensure_ascii=True):
    write_text(path, json.dumps(data, indent=2, ensure_ascii=ensure_ascii) + "\n")


@contextmanager
def file_lock(path):
    """Lock a stable sibling while an atomic replacement changes the data file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as lock:
        if os.name == "nt":
            import msvcrt

            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_LOCK, 1)
        else:
            import fcntl

            fcntl.flock(lock, fcntl.LOCK_EX)
        yield
