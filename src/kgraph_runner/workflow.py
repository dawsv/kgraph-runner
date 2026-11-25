"""Load a local Python workflow with the context used by the CLI."""

from contextlib import contextmanager, suppress
import importlib.util
import inspect
import json
from pathlib import Path
import sys
from threading import RLock


_workflow_lock = RLock()


@contextmanager
def _load_workflow(path):
    with _workflow_lock:
        with _import_workflow(path) as module:
            yield module


@contextmanager
def _import_workflow(path):
    spec = importlib.util.spec_from_file_location("_kgraph_user_workflow", path)
    if spec is None or spec.loader is None:
        raise ValueError(f"Cannot load Python workflow {path}")
    module = importlib.util.module_from_spec(spec)
    parent = str(path.parent)
    added_parent = parent not in sys.path
    if added_parent:
        sys.path.insert(0, parent)
    previous_module = sys.modules.get(spec.name)
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
        yield module
    finally:
        if previous_module is None:
            sys.modules.pop(spec.name, None)
        else:
            sys.modules[spec.name] = previous_module
        if added_parent:
            with suppress(ValueError):
                sys.path.remove(parent)


def _require_object(value, source):
    if not isinstance(value, dict):
        raise ValueError(f"{source} must be a JSON object (dict)")
    return value


def _parse_json(text, source):
    try:
        value = json.loads(text)
    except ValueError as exc:
        raise ValueError(f"Invalid JSON in {source}: {exc}") from exc
    return _require_object(value, source)


def _read_json(path):
    return _parse_json(path.read_text(encoding="utf-8"), str(path))


def _resolve_context(module, ctx_inline=None, ctx_file=None):
    default_factory = getattr(module, "default_context", None)
    if callable(default_factory):
        ctx = _require_object(default_factory(), "default_context()").copy()
    else:
        ctx = _require_object(getattr(module, "DEFAULT_CTX", {}), "DEFAULT_CTX").copy()
    if ctx_file is not None:
        ctx.update(_read_json(Path(ctx_file)))
    if ctx_inline is not None:
        overrides = _parse_json(ctx_inline, "--ctx") if isinstance(ctx_inline, str) else ctx_inline
        ctx.update(_require_object(overrides, "--ctx"))
    return ctx


def _resolve_graph(module, factory_name, ctx):
    factory = getattr(module, factory_name, None)
    if callable(factory):
        signature = inspect.signature(factory)
        try:
            signature.bind(ctx)
        except TypeError:
            return factory()
        return factory(ctx)
    for name in ("GRAPH", "NODES"):
        try:
            return getattr(module, name)
        except AttributeError:
            continue
    raise ValueError(f"Workflow must define a callable {factory_name} or export GRAPH or NODES")
