#!/usr/bin/env python3
"""
Layer 0 — Portable, zero-dependency Python stdlib Loops-and-Graphs engine.

Zero SmithShroom or Unity imports.
Primitives:
  - TaskNode          : atomic code/worker/verifier node with SHA-256 caching, timeout_s,
                        bounded make->verify->repair loop, and 2-same-cause circuit breaker
  - SubgraphNode      : nested child DAG treated as a single composable node (arbitrary depth)
  - LoopSubgraphNode  : wraps an ENTIRE multi-node subgraph in a bounded run->verify->repair loop
                        with targeted dirty-node invalidation and 2-same-cause circuit breaker
  - SplitterNode      : parallel fan-out over items (each item builds a node)
  - GraphRunner       : dependency-ordered execution, persisted graph_state.json,
                        max_worker_calls budget guard, --until & --retry-node
"""
from __future__ import annotations

import concurrent.futures
import datetime
import hashlib
import json
import threading
import time
from collections import deque
from contextlib import suppress
from dataclasses import dataclass
from graphlib import TopologicalSorter
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Tuple

from ._files import file_lock, write_text


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(Path(path).read_bytes())


def sha256_json(obj: Any) -> str:
    return sha256_bytes(json.dumps(obj, sort_keys=True, default=str).encode("utf-8"))


def _packet_paths(packet_path: Optional[str], packet_paths: Optional[Sequence[str]]) -> List[str]:
    paths = [packet_path] if packet_path is not None else []
    paths.extend(packet_paths or ())
    return list(dict.fromkeys(str(path) for path in paths))


class GraphFailure(RuntimeError):
    """Raised when a node/subgraph fails verification, hits timeout/budget, pauses for subagents, or trips the circuit breaker."""

    def __init__(
        self,
        node_path: str,
        reason: str,
        circuit_broken: bool = False,
        budget_exceeded: bool = False,
        awaiting_subagents: bool = False,
        packet_path: Optional[str] = None,
        efficiency: Optional[Dict[str, int]] = None,
        packet_paths: Optional[Sequence[str]] = None,
    ):
        super().__init__(f"[{node_path}] {reason}")
        self.node_path = node_path
        self.reason = reason
        self.circuit_broken = circuit_broken
        self.budget_exceeded = budget_exceeded
        self.awaiting_subagents = awaiting_subagents
        self.packet_paths: List[str] = _packet_paths(packet_path, packet_paths)
        self.packet_path = self.packet_paths[0] if self.packet_paths else None
        self.efficiency = efficiency or {}


@dataclass
class TaskNode:
    id: str
    kind: str  # "code" | "worker" | "verifier" | "gate"
    run_fn: Callable[[Dict[str, Any], int], Any]
    input_hash_fn: Callable[[Dict[str, Any]], str]
    outputs_fn: Callable[[Dict[str, Any]], Sequence[Path]] = lambda _ctx: ()
    verify_fn: Optional[Callable[[Dict[str, Any], Any], Tuple[bool, str, str]]] = None
    repair_fn: Optional[Callable[[Dict[str, Any], int, str, str], None]] = None
    max_retries: int = 0
    timeout_s: Optional[float] = None
    deps: Tuple[str, ...] = ()
    cacheable: bool = True
    resource: Optional[str] = None
    efficiency_fn: Optional[Callable[[Any], Dict[str, int]]] = None


@dataclass
class SubgraphNode:
    id: str
    nodes: Sequence[Any]
    deps: Tuple[str, ...] = ()
    resource: Optional[str] = None


@dataclass
class LoopSubgraphNode:
    """Wraps an entire multi-node subgraph in a bounded make->verify->repair loop."""
    id: str
    nodes: Sequence[Any]
    verify_fn: Callable[[Dict[str, Any]], Tuple[bool, str, str]]
    repair_fn: Optional[Callable[[Dict[str, Any], int, str, str], Optional[Sequence[str]]]] = None
    max_retries: int = 2
    deps: Tuple[str, ...] = ()
    resource: Optional[str] = None


@dataclass
class SplitterNode:
    id: str
    items_fn: Callable[[Dict[str, Any]], Sequence[Any]]
    child_builder_fn: Callable[[Any, Dict[str, Any]], Any]
    deps: Tuple[str, ...] = ()
    max_workers: int = 4
    resource: Optional[str] = None


def _raise_parallel_errors(path: str, errors: Sequence[Exception], group: str) -> None:
    if not errors:
        return
    if len(errors) == 1:
        raise errors[0]
    summary = "; ".join(str(error) for error in errors)
    awaiting = [error for error in errors if isinstance(error, GraphFailure) and error.awaiting_subagents]
    if len(awaiting) == len(errors):
        raise GraphFailure(
            path,
            summary,
            awaiting_subagents=True,
            packet_paths=[packet for error in awaiting for packet in error.packet_paths],
        )
    raise GraphFailure(path, f"{len(errors)} {group} failed: {summary}")


def _node_kind(node: Any) -> str:
    if isinstance(node, TaskNode):
        return node.kind
    if isinstance(node, SubgraphNode):
        return "subgraph"
    if isinstance(node, LoopSubgraphNode):
        return "loop_subgraph"
    if isinstance(node, SplitterNode):
        return "splitter"
    raise TypeError(f"Unsupported node type: {type(node)}")


def _failure_status(error: BaseException) -> str:
    if isinstance(error, GraphFailure):
        if error.awaiting_subagents:
            return "awaiting_subagents"
        if error.circuit_broken:
            return "circuit_broken"
    return "failed"


def _static_resources(node: Any) -> set[str]:
    name = getattr(node, "resource", None)
    resources = {str(name)} if name else set()
    if isinstance(node, (SubgraphNode, LoopSubgraphNode)):
        for child in node.nodes:
            resources.update(_static_resources(child))
    return resources


class GraphRunner:
    def __init__(
        self,
        state_path: Path,
        retry_nodes: Iterable[str] = (),
        force: bool = False,
        until: Optional[str] = None,
        max_worker_calls: Optional[int] = None,
        max_parallel_nodes: int = 1,
        logger: Callable[[str], None] = print,
        on_event: Optional[Callable[[Dict[str, Any]], None]] = None,
    ):
        self.state_path = Path(state_path)
        self.retry_nodes = set(retry_nodes)
        self.force = force
        self.until = until
        self.max_worker_calls = max_worker_calls
        self.max_parallel_nodes = max(1, int(max_parallel_nodes))
        self.worker_calls_used = 0
        self.efficiency = dict.fromkeys(("script_verifiers_passed", "agent_dispatches", "precheck_short_circuits"), 0)
        self._raw_logger = logger
        self.on_event = on_event
        self._lock = threading.Lock()
        self._resource_locks: Dict[str, threading.RLock] = {}
        self.logger = self._log_safe
        if self.state_path.exists():
            self.state = json.loads(self.state_path.read_text(encoding="utf-8"))
        else:
            self.state = {"version": 3, "nodes": {}}
        self.trace: List[Dict[str, Any]] = []
        self._until_path: Optional[str] = None
        self.reached_path: Optional[str] = None
        self.last_completed_stage: Optional[str] = None

    def _log_safe(self, msg: str) -> None:
        with self._lock:
            self._raw_logger(msg)

    def _now(self) -> str:
        return datetime.datetime.now(datetime.timezone.utc).isoformat()

    def _emit_event(self, name: str, **fields: Any) -> None:
        """Notify observers outside self._lock. Task attempt numbers start at one."""
        if self.on_event is not None:
            with suppress(Exception):
                self.on_event({"event": name, "timestamp": self._now(), **fields})

    def _save_state(self, full_path: str) -> None:
        """Persist a node update while the caller holds self._lock."""
        state_path = self.state_path.resolve()
        with file_lock(state_path.with_name(state_path.name + ".lock")):
            if state_path.exists():
                disk_state = json.loads(state_path.read_text(encoding="utf-8"))
            else:
                disk_state = self.state
            state = {**self.state, **disk_state}
            state["nodes"] = {**disk_state.get("nodes", {}), full_path: self.state["nodes"][full_path]}
            write_text(state_path, json.dumps(state, indent=2) + "\n")

    def _should_retry(self, full_path: str, local_id: str) -> bool:
        if self.force:
            return True
        with self._lock:
            if full_path in self.retry_nodes or local_id in self.retry_nodes:
                return True
            return any(full_path.startswith(pat + "/") for pat in self.retry_nodes)

    def _is_fresh(self, full_path: str, local_id: str, in_hash: str, outputs: Sequence[Path]) -> bool:
        return self._freshness_reason(full_path, local_id, in_hash, outputs)[0]

    def _freshness_reason(
        self, full_path: str, local_id: str, in_hash: str, outputs: Sequence[Path], cacheable: bool = True,
    ) -> Tuple[bool, str]:
        if not cacheable:
            return False, "not_cacheable"
        if self.force:
            return False, "forced"
        if self._should_retry(full_path, local_id):
            return False, "retry_requested"
        with self._lock:
            rec = self.state.get("nodes", {}).get(full_path)
        if not rec:
            return False, "no_prior_state"
        if rec.get("status") != "passed":
            return False, f"prior_status:{rec.get('status')}"
        if rec.get("input_hash") != in_hash:
            return False, "input_hash_changed"
        for path in outputs:
            if not Path(path).exists():
                return False, f"missing_output:{path}"
        # Verify recorded output SHA-256 hashes when present so tampered/overwritten outputs invalidate cache
        saved_out_hashes = rec.get("output_hashes") or {}
        for path in outputs:
            expected_sha = saved_out_hashes.get(str(path))
            if expected_sha and Path(path).is_file() and sha256_file(Path(path)) != expected_sha:
                return False, f"output_hash_changed:{path}"
        return True, "fresh"

    def _consume_worker_budget(self, full_path: str) -> None:
        with self._lock:
            if self.max_worker_calls is not None and self.worker_calls_used >= self.max_worker_calls:
                raise GraphFailure(
                    full_path,
                    f"Worker call budget exceeded ({self.worker_calls_used}/{self.max_worker_calls})",
                    budget_exceeded=True,
                )
            self.worker_calls_used += 1

    def _count_efficiency(self, counts: Dict[str, int]) -> None:
        with self._lock:
            for key in self.efficiency.keys() & counts.keys():
                self.efficiency[key] += counts[key]

    def _record(self, full_path: str, entry: Dict[str, Any]) -> None:
        with self._lock:
            self.state.setdefault("nodes", {})[full_path] = entry
            self.trace.append({"node": full_path, **entry})
            self._save_state(full_path)

    @staticmethod
    def _nodes_by_id(nodes: Sequence[Any]) -> Dict[str, Any]:
        by_id = {}
        for node in nodes:
            if node.id in by_id:
                raise ValueError(f"Duplicate node ID '{node.id}'")
            by_id[node.id] = node
        return by_id

    def topo_sort(self, nodes: Sequence[Any]) -> List[Any]:
        by_id = self._nodes_by_id(nodes)
        visited: Dict[str, int] = {}
        ordered: List[Any] = []

        def dfs(nid: str):
            st = visited.get(nid, 0)
            if st == 1:
                raise ValueError(f"Cycle detected in graph at node '{nid}'")
            if st == 2:
                return
            visited[nid] = 1
            node = by_id[nid]
            for d in getattr(node, "deps", ()):
                if d not in by_id:
                    raise ValueError(f"Node '{nid}' depends on unknown sibling '{d}'")
                dfs(d)
            visited[nid] = 2
            ordered.append(node)

        for n in nodes:
            dfs(n.id)
        return ordered

    def _resolve_until_path(
        self, root_nodes: Sequence[Any], prefix: str, *, validate_topology: bool = False,
    ) -> Optional[str]:
        requested = self.until
        if requested is None:
            return None
        if any(part in ("", ".", "..") for part in requested.split("/")):
            raise GraphFailure("--until", f"Invalid stage path {requested!r}")

        stages: Dict[str, Any] = {}

        def visit(nodes, parent_path):
            if validate_topology:
                self.topo_sort(nodes)
            for node in nodes:
                path = f"{parent_path}/{node.id}" if parent_path else node.id
                if path in stages:
                    raise GraphFailure("--until", f"Ambiguous stage path {path!r}")
                stages[path] = node
                # Splitters have runtime children and are addressable only as a whole stage.
                if isinstance(node, (SubgraphNode, LoopSubgraphNode)):
                    visit(node.nodes, path)

        visit(root_nodes, prefix)
        if "/" in requested:
            matches = [requested] if requested in stages else []
        else:
            matches = [path for path, node in stages.items() if node.id == requested]
        if len(matches) > 1:
            raise GraphFailure("--until", f"Ambiguous stage {requested!r}. Choose from {', '.join(matches)}.")
        path = matches[0] if matches else requested
        for stage_path, node in stages.items():
            if isinstance(node, SplitterNode) and path.startswith(stage_path + "/"):
                raise GraphFailure("--until", f"Cannot stop inside runtime splitter {stage_path!r}. Stop at the splitter itself.")
        if not matches:
            raise GraphFailure("--until", f"Unknown stage {requested!r}. Use a unique stage ID or full path.")
        return path

    def run(self, root_nodes: Sequence[Any], ctx: Dict[str, Any], prefix: str = "") -> Dict[str, Any]:
        self.reached_path = None
        self.last_completed_stage = None
        self._until_path = None
        self._emit_event("run_start", status="running", error="")
        try:
            self._until_path = self._resolve_until_path(root_nodes, prefix)
            self._run_nodes(root_nodes, ctx, prefix)
        except BaseException as exc:
            status = _failure_status(exc)
            if status == "failed" and isinstance(exc, GraphFailure) and exc.budget_exceeded:
                status = "budget_exceeded"
            self._emit_event("run_end", status=status, error=str(exc) or type(exc).__name__)
            raise
        self._emit_event("run_end", status="stopped" if self.reached_path is not None else "passed", error="")
        return ctx

    def plan(self, root_nodes: Sequence[Any], ctx: Dict[str, Any], prefix: str = "") -> List[Dict[str, Any]]:
        """Preview one topological pass using the supplied inputs and existing artifacts.

        Metadata and splitter callbacks run against a shallow context copy. They
        should be read-only. Missing inputs can leave task freshness or the
        contents of a dynamic splitter unknown until execution.
        """
        until_path = self._resolve_until_path(root_nodes, prefix, validate_topology=True)
        plan_ctx = ctx.copy()
        entries: List[Dict[str, Any]] = []

        def visit(nodes: Sequence[Any], parent_path: str) -> bool:
            for node in nodes:
                path = f"{parent_path}/{node.id}" if parent_path else node.id
                entry = {
                    "path": path,
                    "id": node.id,
                    "node_type": type(node).__name__,
                    "kind": _node_kind(node),
                    "deps": list(getattr(node, "deps", ())),
                    "resource": getattr(node, "resource", None),
                    "status": "container",
                    "reason": None,
                }
                entries.append(entry)
                if isinstance(node, TaskNode):
                    try:
                        fresh, reason = self._freshness_reason(
                            path, node.id, node.input_hash_fn(plan_ctx), list(node.outputs_fn(plan_ctx)), node.cacheable,
                        )
                    except (FileNotFoundError, KeyError) as exc:
                        fresh, reason = False, "inputs_unavailable"
                        entry["detail"] = str(exc)
                    entry.update(status="cached" if fresh else "will_run", reason=reason)
                elif isinstance(node, (SubgraphNode, LoopSubgraphNode)):
                    if visit(self.topo_sort(node.nodes), path):
                        return True
                elif isinstance(node, SplitterNode):
                    try:
                        children = self._build_splitter_children(node, plan_ctx)
                    except (FileNotFoundError, KeyError) as exc:
                        entry["detail"] = str(exc)
                    else:
                        if visit(children, path):
                            return True
                if path == until_path:
                    return True
            return False

        visit(self.topo_sort(root_nodes), prefix)
        return entries

    def _get_resource_lock(self, name: str) -> threading.RLock:
        with self._lock:
            lock = self._resource_locks.get(name)
            if lock is None:
                lock = threading.RLock()
                self._resource_locks[name] = lock
            return lock

    def _execute_single_node(
        self,
        node: Any,
        full_path: str,
        ctx: Dict[str, Any],
        *,
        track_stages: bool,
        held_resources: Tuple[str, ...] = (),
    ) -> None:
        kind = _node_kind(node)
        # Resource-free containers can still run their children in parallel.
        # Exclusive containers reserve their static subtree in one order before it runs.
        resources = _static_resources(node) if getattr(node, "resource", None) else set()
        child_resources = tuple(sorted(resources.union(held_resources)))
        acquired = []
        try:
            for name in sorted(resources.difference(held_resources)):
                lock = self._get_resource_lock(name)
                # Runtime splitter children can introduce a resource absent from
                # the static subtree. Never wait for it in reverse order.
                ordered = not held_resources or name > held_resources[-1]
                if not lock.acquire(blocking=ordered):
                    raise GraphFailure(full_path, f"Cannot wait for resource {name!r} while holding {held_resources[-1]!r}")
                acquired.append(lock)
            simple_container = isinstance(node, (SubgraphNode, SplitterNode))
            if simple_container:
                started = time.monotonic()
                self._emit_event("node_start", node=full_path, kind=kind, attempt=1)
            if isinstance(node, TaskNode):
                self._run_task(node, full_path, ctx)
            elif isinstance(node, SubgraphNode):
                self.logger(f"[subgraph:enter] {full_path}")
                self._run_nodes(
                    node.nodes,
                    ctx,
                    full_path,
                    track_stages=track_stages,
                    held_resources=child_resources,
                )
                if self.reached_path is not None:
                    return
                self.logger(f"[subgraph:done]  {full_path}")
            elif isinstance(node, LoopSubgraphNode):
                self._run_loop_subgraph(
                    node,
                    full_path,
                    ctx,
                    track_stages=track_stages,
                    held_resources=child_resources,
                )
                if self.reached_path is not None:
                    return
            elif isinstance(node, SplitterNode):
                self._run_splitter(node, full_path, ctx, held_resources=child_resources)

            if simple_container:
                self._emit_event(
                    "node_passed", node=full_path, kind=kind, attempts=1,
                    duration_s=round(time.monotonic() - started, 3), cache_miss_reason=None,
                )

            if track_stages:
                with self._lock:
                    self.last_completed_stage = full_path
                    if full_path == self._until_path:
                        self.reached_path = full_path
        except Exception as exc:
            failure = exc if isinstance(exc, GraphFailure) else None
            self._emit_event(
                f"node_{_failure_status(exc)}", node=full_path, kind=kind,
                detail=failure.reason if failure else str(exc),
                packet_path=failure.packet_path if failure else None,
                packet_paths=list(failure.packet_paths) if failure else [],
            )
            raise
        finally:
            for lock in reversed(acquired):
                lock.release()

    def _run_nodes(
        self,
        nodes: Sequence[Any],
        ctx: Dict[str, Any],
        prefix: str,
        *,
        track_stages: bool = True,
        held_resources: Tuple[str, ...] = (),
    ) -> None:
        ordered = self.topo_sort(nodes)
        # Keep deterministic sequential order whenever --until is active, max_parallel_nodes == 1, or inside an exclusive resource.
        if held_resources or self.max_parallel_nodes <= 1 or self._until_path is not None:
            for node in ordered:
                full_path = f"{prefix}/{node.id}" if prefix else node.id
                self._execute_single_node(
                    node,
                    full_path,
                    ctx,
                    track_stages=track_stages,
                    held_resources=held_resources,
                )
                if self.reached_path is not None:
                    return
            return

        if not ordered:
            return
        by_id = {node.id: node for node in ordered}
        sorter = TopologicalSorter({node.id: getattr(node, "deps", ()) for node in ordered})
        sorter.prepare()
        ready = deque(sorter.get_ready())
        active: Dict[concurrent.futures.Future, str] = {}
        errors: Dict[str, Exception] = {}
        max_workers = min(self.max_parallel_nodes, len(ordered))
        with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as executor:
            # A failure blocks new submissions while the in-flight siblings finish.
            while active or (ready and not errors):
                while ready and not errors and len(active) < max_workers:
                    node_id = ready.popleft()
                    future = executor.submit(
                        self._execute_single_node,
                        by_id[node_id],
                        f"{prefix}/{node_id}" if prefix else node_id,
                        ctx,
                        track_stages=track_stages,
                    )
                    active[future] = node_id
                completed, _ = concurrent.futures.wait(active, return_when=concurrent.futures.FIRST_COMPLETED)
                for future in completed:
                    node_id = active.pop(future)
                    try:
                        future.result()
                    except Exception as exc:
                        errors[node_id] = exc
                    else:
                        sorter.done(node_id)
                ready.extend(sorter.get_ready())
        _raise_parallel_errors(
            prefix or "root", [errors[node_id] for node_id in by_id if node_id in errors], "parallel sibling(s)",
        )

    def _invoke_with_timeout(self, fn: Callable, args: tuple, timeout_s: Optional[float]):
        if timeout_s is None:
            return fn(*args)
        result: Any = None
        error: Optional[BaseException] = None

        def invoke() -> None:
            nonlocal result, error
            try:
                result = fn(*args)
            except BaseException as exc:
                error = exc

        thread = threading.Thread(target=invoke, daemon=True)
        thread.start()
        thread.join(timeout=timeout_s)
        if thread.is_alive():
            raise TimeoutError(f"exceeded timeout_s={timeout_s}s")
        if error is not None:
            raise error
        return result

    def _run_task(self, node: TaskNode, full_path: str, ctx: Dict[str, Any]) -> None:
        t0 = time.monotonic()
        in_hash = node.input_hash_fn(ctx)
        outputs = list(node.outputs_fn(ctx))
        fresh, miss_reason = self._freshness_reason(full_path, node.id, in_hash, outputs, node.cacheable)
        if fresh:
            self.logger(f"[graph:cached] {full_path}")
            with self._lock:
                self.trace.append({"node": full_path, "status": "cached", "input_hash": in_hash})
            self._emit_event("node_cached", node=full_path, kind=node.kind, input_hash=in_hash)
            return

        task_fields = {
            "kind": node.kind, "input_hash": in_hash, "cache_miss_reason": miss_reason,
            "outputs": [str(p) for p in outputs],
        }

        def record_callback_failure(attempt, exc):
            self._record(full_path, {
                **task_fields,
                "status": "failed",
                "attempts": attempt + 1,
                "duration_s": round(time.monotonic() - t0, 3),
                "error_signature": f"exception:{type(exc).__name__}",
                "detail": str(exc) or type(exc).__name__,
                "updated_at": self._now(),
            })

        prev_err_sig: Optional[str] = None
        last_detail = ""
        for attempt in range(node.max_retries + 1):
            if node.kind == "worker":
                self._consume_worker_budget(full_path)
            started_at = self._now()
            with self._lock:
                self.state.setdefault("nodes", {})[full_path] = {
                    **task_fields,
                    "status": "running",
                    "attempts": attempt,
                    "started_at": started_at,
                    "updated_at": started_at,
                }
                self._save_state(full_path)
            self._emit_event("node_start", node=full_path, kind=node.kind, attempt=attempt + 1)
            try:
                result = self._invoke_with_timeout(node.run_fn, (ctx, attempt), node.timeout_s)
                if node.verify_fn is not None:
                    ok, err_sig, detail = node.verify_fn(ctx, result)
                else:
                    ok, err_sig, detail = True, "", "ok"
                if node.efficiency_fn is not None:
                    self._count_efficiency(node.efficiency_fn(result))
            except TimeoutError as exc:
                ok, err_sig, detail = False, "timeout", str(exc)
            except GraphFailure as exc:
                self._count_efficiency(exc.efficiency)
                self._record(full_path, {
                    **task_fields,
                    "status": _failure_status(exc),
                    "attempts": attempt + 1,
                    "detail": exc.reason,
                    "packet_path": exc.packet_path,
                    "packet_paths": list(exc.packet_paths),
                    "updated_at": self._now(),
                })
                raise
            except Exception as exc:
                ok, err_sig, detail = False, f"exception:{type(exc).__name__}", str(exc)

            if ok:
                try:
                    out_list = list(node.outputs_fn(ctx))
                    out_paths = [str(p) for p in out_list]
                    out_hashes = {str(p): sha256_file(Path(p)) for p in out_list if Path(p).is_file()}
                    final_input_hash = node.input_hash_fn(ctx)
                except BaseException as exc:
                    record_callback_failure(attempt, exc)
                    raise
                if node.kind == "verifier":
                    self._count_efficiency({"script_verifiers_passed": 1})
                dur = round(time.monotonic() - t0, 3)
                self._record(full_path, {
                    **task_fields,
                    "status": "passed",
                    "input_hash": final_input_hash,
                    "attempts": attempt + 1,
                    "duration_s": dur,
                    "detail": detail,
                    "outputs": out_paths,
                    "output_hashes": out_hashes,
                    "updated_at": self._now(),
                })
                # Clear targeted retry once succeeded so nested loops don't infinitely re-run it
                with self._lock:
                    self.retry_nodes.discard(full_path)
                    self.retry_nodes.discard(node.id)
                self.logger(f"[graph:ran]    {full_path} (attempts={attempt + 1}, {dur:.2f}s) — {detail}")
                self._emit_event(
                    "node_passed", node=full_path, kind=node.kind, attempts=attempt + 1,
                    duration_s=dur, cache_miss_reason=miss_reason,
                )
                return

            last_detail = detail
            if prev_err_sig is not None and err_sig == prev_err_sig:
                dur = round(time.monotonic() - t0, 3)
                self._record(full_path, {
                    **task_fields,
                    "status": "circuit_broken",
                    "attempts": attempt + 1,
                    "duration_s": dur,
                    "error_signature": err_sig,
                    "detail": detail,
                    "updated_at": self._now(),
                })
                self.logger(f"[graph:BREAK]  {full_path} circuit breaker tripped on 2x '{err_sig}': {detail}")
                raise GraphFailure(full_path, f"Circuit breaker (2x '{err_sig}'): {detail}", circuit_broken=True)

            prev_err_sig = err_sig
            if attempt < node.max_retries:
                self.logger(f"[graph:retry]  {full_path} attempt {attempt + 1} failed ({err_sig}): {detail}")
                self._emit_event(
                    "node_retry", node=full_path, kind=node.kind, attempt=attempt + 1,
                    error_signature=err_sig, detail=detail,
                )
                if node.repair_fn is not None:
                    try:
                        node.repair_fn(ctx, attempt + 1, err_sig, detail)
                    except BaseException as exc:
                        record_callback_failure(attempt, exc)
                        raise

        dur = round(time.monotonic() - t0, 3)
        self._record(full_path, {
            **task_fields,
            "status": "failed",
            "attempts": node.max_retries + 1,
            "duration_s": dur,
            "error_signature": prev_err_sig,
            "detail": last_detail,
            "updated_at": self._now(),
        })
        self.logger(f"[graph:FAIL]   {full_path} exhausted {node.max_retries + 1} attempt(s): {last_detail}")
        raise GraphFailure(full_path, last_detail, circuit_broken=False)

    def _run_loop_subgraph(
        self,
        node: LoopSubgraphNode,
        full_path: str,
        ctx: Dict[str, Any],
        *,
        track_stages: bool,
        held_resources: Tuple[str, ...] = (),
    ) -> None:
        t0 = time.monotonic()
        prev_err_sig: Optional[str] = None
        last_detail = ""
        for attempt in range(node.max_retries + 1):
            self.logger(f"[loop_sg:iter] {full_path} (attempt {attempt + 1}/{node.max_retries + 1})")
            self._emit_event("node_start", node=full_path, kind="loop_subgraph", attempt=attempt + 1)
            self._run_nodes(node.nodes, ctx, full_path, track_stages=track_stages, held_resources=held_resources)
            if self.reached_path is not None:
                return
            ok, err_sig, detail = node.verify_fn(ctx)
            if ok:
                dur = round(time.monotonic() - t0, 3)
                self._record(full_path, {
                    "kind": "loop_subgraph",
                    "status": "passed",
                    "attempts": attempt + 1,
                    "duration_s": dur,
                    "detail": detail,
                    "updated_at": self._now(),
                })
                self.logger(f"[loop_sg:PASS] {full_path} (attempts={attempt + 1}, {dur:.2f}s) — {detail}")
                self._emit_event(
                    "node_passed", node=full_path, kind="loop_subgraph", attempts=attempt + 1,
                    duration_s=dur, cache_miss_reason=None,
                )
                return

            last_detail = detail
            if prev_err_sig is not None and err_sig == prev_err_sig:
                dur = round(time.monotonic() - t0, 3)
                self._record(full_path, {
                    "kind": "loop_subgraph",
                    "status": "circuit_broken",
                    "attempts": attempt + 1,
                    "duration_s": dur,
                    "error_signature": err_sig,
                    "detail": detail,
                    "updated_at": self._now(),
                })
                self.logger(f"[loop_sg:BRK]  {full_path} circuit breaker tripped on 2x '{err_sig}': {detail}")
                raise GraphFailure(full_path, f"Subgraph circuit breaker (2x '{err_sig}'): {detail}", circuit_broken=True)

            prev_err_sig = err_sig
            if attempt < node.max_retries:
                self.logger(f"[loop_sg:rtry] {full_path} attempt {attempt + 1} failed ({err_sig}): {detail}")
                self._emit_event(
                    "node_retry", node=full_path, kind="loop_subgraph", attempt=attempt + 1,
                    error_signature=err_sig, detail=detail,
                )
                if node.repair_fn is not None:
                    dirty_paths = node.repair_fn(ctx, attempt + 1, err_sig, detail) or ()
                    with self._lock:
                        for dp in dirty_paths:
                            self.retry_nodes.add(dp if dp.startswith(full_path + "/") else f"{full_path}/{dp}")

        dur = round(time.monotonic() - t0, 3)
        self._record(full_path, {
            "kind": "loop_subgraph",
            "status": "failed",
            "attempts": node.max_retries + 1,
            "duration_s": dur,
            "error_signature": prev_err_sig,
            "detail": last_detail,
            "updated_at": self._now(),
        })
        raise GraphFailure(full_path, last_detail, circuit_broken=False)

    def write_run_report(
        self,
        report_path: Optional[Path] = None,
        overall_status: str = "passed",
        error: str = "",
        packet_path: Optional[str] = None,
        packet_paths: Optional[Sequence[str]] = None,
    ) -> Path:
        """Emit structured run_report.json telemetry for objective Failure Classification & Graph Evolution."""
        out_p = Path(report_path) if report_path else self.state_path.with_name(self.state_path.stem.replace("graph_state", "run_report") + ".json")
        with self._lock:
            trace_copy = list(self.trace)
            efficiency = dict(self.efficiency)
            worker_calls_used = self.worker_calls_used
            reached_path = self.reached_path
            last_completed_stage = self.last_completed_stage
        latest = {event["node"]: event["status"] for event in trace_copy}
        packets = _packet_paths(packet_path, packet_paths)
        cached = sum(1 for t in trace_copy if t.get("status") == "cached")
        passed = sum(1 for t in trace_copy if t.get("status") == "passed")
        failed = sum(1 for t in trace_copy if t.get("status") == "failed")
        broken = sum(1 for t in trace_copy if t.get("status") == "circuit_broken")
        retried = [t["node"] for t in trace_copy if t.get("attempts", 1) > 1]
        failure_sigs = [
            {"node": t["node"], "status": t.get("status"), "error_signature": t.get("error_signature"), "detail": t.get("detail")}
            for t in trace_copy if t.get("status") in ("failed", "circuit_broken")
        ]
        timed = sorted(
            [{"node": t["node"], "duration_s": t.get("duration_s", 0.0), "attempts": t.get("attempts", 1)} for t in trace_copy if "duration_s" in t],
            key=lambda x: x["duration_s"], reverse=True,
        )[:5]
        hints = []
        if broken > 0:
            hints.append("Circuit breaker tripped on repeated identical error — classify as Graph/Verifier Flaw vs Task Bug before retrying.")
        if any(f.get("error_signature") == "timeout" for f in failure_sigs):
            hints.append("Timeout hit — inspect hung subprocess or adjust node timeout_s / split workload.")
        if len(retried) >= 2:
            hints.append(f"Multiple nodes required retries ({', '.join(retried)}) — consider tightening upstream prompts or adding a pre-verifier.")

        report = {
            "version": 1,
            "status": "stopped" if overall_status == "passed" and reached_path is not None else overall_status,
            "error": error,
            "awaiting_subagents": overall_status == "awaiting_subagents",
            "packet_path": packets[0] if packets else None,
            "packet_paths": packets,
            "cached_nodes": [node for node, status in latest.items() if status == "cached"],
            "requested_until": self.until,
            "requested_path": self._until_path,
            "reached_path": reached_path,
            "last_completed_stage": last_completed_stage,
            "generated_at": self._now(),
            "state_file": str(self.state_path),
            "scope": {
                "requested_until": self.until,
                "stop_reached": reached_path is not None,
                "reached_nodes": list(dict.fromkeys(event["node"] for event in trace_copy)),
            },
            "summary": {
                "total_events": len(trace_copy),
                "cached": cached,
                "ran_passed": passed,
                "failed": failed,
                "circuit_broken": broken,
                "retried_nodes": retried,
                "worker_calls_used": worker_calls_used,
                "max_worker_calls": self.max_worker_calls,
                **efficiency,
            },
            "failure_signatures": failure_sigs,
            "slowest_nodes": timed,
            "evolution_hints": hints,
        }
        write_text(out_p, json.dumps(report, indent=2) + "\n")
        return out_p

    def _build_splitter_children(self, node: SplitterNode, ctx: Dict[str, Any]) -> List[Any]:
        children = [node.child_builder_fn(item, ctx) for item in list(node.items_fn(ctx))]
        self._nodes_by_id(children)
        return children

    def _run_splitter(
        self, node: SplitterNode, full_path: str, ctx: Dict[str, Any], *, held_resources: Tuple[str, ...] = (),
    ) -> None:
        children = self._build_splitter_children(node, ctx)
        self.logger(f"[splitter]     {full_path} ({len(children)} branch(es), workers={node.max_workers})")

        def exec_child(child):
            self._execute_single_node(
                child, f"{full_path}/{child.id}", ctx, track_stages=False, held_resources=held_resources,
            )

        # Descendants must stay on the thread that owns the reserved resources.
        if held_resources or node.max_workers <= 1 or len(children) <= 1:
            for child in children:
                exec_child(child)
            return

        errors = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=node.max_workers) as executor:
            futures = [executor.submit(exec_child, child) for child in children]
            for future in futures:
                try:
                    future.result()
                except Exception as exc:
                    errors.append(exc)
        _raise_parallel_errors(full_path, errors, "splitter branch(es)")
