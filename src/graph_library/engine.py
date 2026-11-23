#!/usr/bin/env python3
"""
Layer 0 — Portable, zero-dependency Python stdlib Loops-and-Graphs engine.

Zero SmithShroom or Unity imports: copy this single file into any project.
Primitives:
  - TaskNode          : atomic code/worker/verifier node with SHA-256 caching, timeout_s,
                        bounded make->verify->repair loop, and 2-same-cause circuit breaker
  - SubgraphNode      : nested child DAG treated as a single composable node (arbitrary depth)
  - LoopSubgraphNode  : wraps an ENTIRE multi-node subgraph in a bounded run->verify->repair loop
                        with targeted dirty-node invalidation and 2-same-cause circuit breaker
  - SplitterNode      : parallel fan-out over items (each item builds a TaskNode or SubgraphNode)
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
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Tuple


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256_bytes(Path(path).read_bytes())


def sha256_json(obj: Any) -> str:
    return sha256_bytes(json.dumps(obj, sort_keys=True, default=str).encode("utf-8"))


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
    ):
        super().__init__(f"[{node_path}] {reason}")
        self.node_path = node_path
        self.reason = reason
        self.circuit_broken = circuit_broken
        self.budget_exceeded = budget_exceeded
        self.awaiting_subagents = awaiting_subagents
        self.packet_path = packet_path
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
        self._lock = threading.Lock()
        self._resource_locks: Dict[str, threading.RLock] = {}
        self.logger = self._log_safe
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        if self.state_path.exists():
            self.state = json.loads(self.state_path.read_text())
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

    def _save_state(self) -> None:
        """Persist the state while the caller holds self._lock."""
        tmp = self.state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.state, indent=2) + "\n")
        tmp.replace(self.state_path)

    def _should_retry(self, full_path: str, local_id: str) -> bool:
        if self.force:
            return True
        if full_path in self.retry_nodes or local_id in self.retry_nodes:
            return True
        return any(full_path.startswith(pat + "/") for pat in self.retry_nodes)

    def _is_fresh(self, full_path: str, local_id: str, in_hash: str, outputs: Sequence[Path]) -> bool:
        if self._should_retry(full_path, local_id):
            return False
        with self._lock:
            rec = self.state.get("nodes", {}).get(full_path)
        if not rec or rec.get("status") != "passed" or rec.get("input_hash") != in_hash:
            return False
        if not all(Path(p).exists() for p in outputs):
            return False
        # Verify recorded output SHA-256 hashes when present so tampered/overwritten outputs invalidate cache
        saved_out_hashes = rec.get("output_hashes") or {}
        for p in outputs:
            sp = str(p)
            expected_sha = saved_out_hashes.get(sp)
            if expected_sha and Path(p).is_file() and sha256_file(Path(p)) != expected_sha:
                return False
        return True

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
            self._save_state()

    def topo_sort(self, nodes: Sequence[Any]) -> List[Any]:
        by_id = {n.id: n for n in nodes}
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

    def _resolve_until_path(self, root_nodes: Sequence[Any], prefix: str) -> Optional[str]:
        requested = self.until
        if requested is None:
            return None
        if any(part in ("", ".", "..") for part in requested.split("/")):
            raise GraphFailure("--until", f"Invalid stage path {requested!r}")

        stages: Dict[str, Any] = {}

        def visit(nodes, parent_path):
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
        self._until_path = self._resolve_until_path(root_nodes, prefix)
        self._run_nodes(root_nodes, ctx, prefix)
        return ctx

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
        in_exclusive_resource: bool = False,
    ) -> None:
        res_name = getattr(node, "resource", None)
        lock = self._get_resource_lock(str(res_name)) if res_name else None
        child_exclusive = in_exclusive_resource or (lock is not None)
        if lock is not None:
            lock.acquire()
        try:
            if isinstance(node, TaskNode):
                self._run_task(node, full_path, ctx)
            elif isinstance(node, SubgraphNode):
                self.logger(f"[subgraph:enter] {full_path}")
                self._run_nodes(
                    node.nodes,
                    ctx,
                    full_path,
                    track_stages=track_stages,
                    allow_parallel=not child_exclusive,
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
                    allow_parallel=not child_exclusive,
                )
                if self.reached_path is not None:
                    return
            elif isinstance(node, SplitterNode):
                self._run_splitter(node, full_path, ctx)
            else:
                raise TypeError(f"Unsupported node type: {type(node)}")

            if track_stages:
                with self._lock:
                    self.last_completed_stage = full_path
                    if full_path == self._until_path:
                        self.reached_path = full_path
        finally:
            if lock is not None:
                lock.release()

    def _topo_waves(self, nodes: Sequence[Any]) -> List[List[Any]]:
        ordered = self.topo_sort(nodes)
        by_id = {n.id: n for n in ordered}
        order_idx = {n.id: i for i, n in enumerate(ordered)}
        remaining = set(by_id)
        done = set()
        waves: List[List[Any]] = []
        while remaining:
            ready = [
                by_id[nid]
                for nid in remaining
                if all(d in done for d in getattr(by_id[nid], "deps", ()))
            ]
            ready.sort(key=lambda n: order_idx[n.id])
            waves.append(ready)
            for n in ready:
                remaining.remove(n.id)
                done.add(n.id)
        return waves

    def _run_nodes(
        self,
        nodes: Sequence[Any],
        ctx: Dict[str, Any],
        prefix: str,
        *,
        track_stages: bool = True,
        allow_parallel: bool = True,
    ) -> None:
        # Keep deterministic sequential order whenever --until is active, max_parallel_nodes == 1, or inside an exclusive resource.
        if not allow_parallel or self.max_parallel_nodes <= 1 or self._until_path is not None:
            for node in self.topo_sort(nodes):
                full_path = f"{prefix}/{node.id}" if prefix else node.id
                self._execute_single_node(
                    node,
                    full_path,
                    ctx,
                    track_stages=track_stages,
                    in_exclusive_resource=not allow_parallel,
                )
                if self.reached_path is not None:
                    return
            return

        for wave in self._topo_waves(nodes):
            if len(wave) == 1:
                node = wave[0]
                full_path = f"{prefix}/{node.id}" if prefix else node.id
                self._execute_single_node(node, full_path, ctx, track_stages=track_stages)
                continue

            self.logger(
                f"[wave:parallel] {prefix or 'root'} ({len(wave)} sibling(s): "
                f"{', '.join(n.id for n in wave)}, max_parallel={self.max_parallel_nodes})"
            )
            errors: List[Exception] = []
            with concurrent.futures.ThreadPoolExecutor(max_workers=min(self.max_parallel_nodes, len(wave))) as ex:
                futs = [
                    ex.submit(
                        self._execute_single_node,
                        node,
                        f"{prefix}/{node.id}" if prefix else node.id,
                        ctx,
                        track_stages=track_stages,
                    )
                    for node in wave
                ]
                for fut in concurrent.futures.as_completed(futs):
                    try:
                        fut.result()
                    except Exception as exc:
                        errors.append(exc)
            if errors:
                if len(errors) == 1:
                    raise errors[0]
                awaiting = [e for e in errors if isinstance(e, GraphFailure) and e.awaiting_subagents]
                if len(awaiting) == len(errors):
                    raise GraphFailure(
                        prefix or "root",
                        "; ".join(str(e) for e in awaiting),
                        awaiting_subagents=True,
                        packet_path=awaiting[0].packet_path,
                    )
                raise GraphFailure(prefix or "root", f"{len(errors)} parallel sibling(s) failed: " + "; ".join(str(e) for e in errors))

    def _invoke_with_timeout(self, fn: Callable, args: tuple, timeout_s: Optional[float]):
        if timeout_s is None:
            return fn(*args)
        ex = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        fut = ex.submit(fn, *args)
        try:
            return fut.result(timeout=timeout_s)
        except concurrent.futures.TimeoutError:
            ex.shutdown(wait=False, cancel_futures=True)
            raise TimeoutError(f"exceeded timeout_s={timeout_s}s")
        else:
            ex.shutdown(wait=False)

    def _run_task(self, node: TaskNode, full_path: str, ctx: Dict[str, Any]) -> None:
        t0 = time.monotonic()
        in_hash = node.input_hash_fn(ctx)
        outputs = list(node.outputs_fn(ctx))
        if node.cacheable and self._is_fresh(full_path, node.id, in_hash, outputs):
            self.logger(f"[graph:cached] {full_path}")
            with self._lock:
                self.trace.append({"node": full_path, "status": "cached", "input_hash": in_hash})
            return

        prev_err_sig: Optional[str] = None
        last_detail = ""
        for attempt in range(node.max_retries + 1):
            if node.kind == "worker":
                self._consume_worker_budget(full_path)
            started_at = self._now()
            with self._lock:
                self.state.setdefault("nodes", {})[full_path] = {
                    "kind": node.kind,
                    "status": "running",
                    "input_hash": in_hash,
                    "attempts": attempt,
                    "started_at": started_at,
                    "updated_at": started_at,
                }
                self._save_state()
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
                status = "awaiting_subagents" if exc.awaiting_subagents else (
                    "circuit_broken" if exc.circuit_broken else "failed"
                )
                self._record(full_path, {
                    "kind": node.kind,
                    "status": status,
                    "input_hash": in_hash,
                    "attempts": attempt + 1,
                    "detail": exc.reason,
                    "updated_at": self._now(),
                })
                raise
            except Exception as exc:
                ok, err_sig, detail = False, f"exception:{type(exc).__name__}", str(exc)

            if ok:
                if node.kind == "verifier":
                    self._count_efficiency({"script_verifiers_passed": 1})
                dur = round(time.monotonic() - t0, 3)
                out_list = list(node.outputs_fn(ctx))
                out_paths = [str(p) for p in out_list]
                out_hashes = {str(p): sha256_file(Path(p)) for p in out_list if Path(p).is_file()}
                self._record(full_path, {
                    "kind": node.kind,
                    "status": "passed",
                    "input_hash": node.input_hash_fn(ctx),
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
                return

            last_detail = detail
            if prev_err_sig is not None and err_sig == prev_err_sig:
                dur = round(time.monotonic() - t0, 3)
                self._record(full_path, {
                    "kind": node.kind,
                    "status": "circuit_broken",
                    "input_hash": in_hash,
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
                if node.repair_fn is not None:
                    node.repair_fn(ctx, attempt + 1, err_sig, detail)

        dur = round(time.monotonic() - t0, 3)
        self._record(full_path, {
            "kind": node.kind,
            "status": "failed",
            "input_hash": in_hash,
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
        allow_parallel: bool = True,
    ) -> None:
        t0 = time.monotonic()
        prev_err_sig: Optional[str] = None
        last_detail = ""
        for attempt in range(node.max_retries + 1):
            self.logger(f"[loop_sg:iter] {full_path} (attempt {attempt + 1}/{node.max_retries + 1})")
            self._run_nodes(node.nodes, ctx, full_path, track_stages=track_stages, allow_parallel=allow_parallel)
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
    ) -> Path:
        """Emit structured run_report.json telemetry for objective Failure Classification & Graph Evolution."""
        out_p = Path(report_path) if report_path else self.state_path.with_name(self.state_path.stem.replace("graph_state", "run_report") + ".json")
        with self._lock:
            trace_copy = list(self.trace)
            efficiency = dict(self.efficiency)
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
            "status": "stopped" if overall_status == "passed" and self.reached_path is not None else overall_status,
            "error": error,
            "awaiting_subagents": overall_status == "awaiting_subagents",
            "packet_path": packet_path,
            "requested_until": self.until,
            "requested_path": self._until_path,
            "reached_path": self.reached_path,
            "last_completed_stage": self.last_completed_stage,
            "generated_at": self._now(),
            "state_file": str(self.state_path),
            "scope": {
                "requested_until": self.until,
                "stop_reached": self.reached_path is not None,
                "reached_nodes": list(dict.fromkeys(event["node"] for event in trace_copy)),
            },
            "summary": {
                "total_events": len(trace_copy),
                "cached": cached,
                "ran_passed": passed,
                "failed": failed,
                "circuit_broken": broken,
                "retried_nodes": retried,
                "worker_calls_used": self.worker_calls_used,
                "max_worker_calls": self.max_worker_calls,
                **efficiency,
            },
            "failure_signatures": failure_sigs,
            "slowest_nodes": timed,
            "evolution_hints": hints,
        }
        out_p.parent.mkdir(parents=True, exist_ok=True)
        out_p.write_text(json.dumps(report, indent=2) + "\n")
        return out_p

    def _run_splitter(self, node: SplitterNode, full_path: str, ctx: Dict[str, Any]) -> None:
        items = list(node.items_fn(ctx))
        children = [node.child_builder_fn(item, ctx) for item in items]
        self.logger(f"[splitter]     {full_path} ({len(children)} branch(es), workers={node.max_workers})")

        def exec_child(child):
            child_path = f"{full_path}/{child.id}"
            if isinstance(child, TaskNode):
                self._run_task(child, child_path, ctx)
            elif isinstance(child, SubgraphNode):
                self._run_nodes(child.nodes, ctx, child_path, track_stages=False)
            elif isinstance(child, LoopSubgraphNode):
                self._run_loop_subgraph(child, child_path, ctx, track_stages=False)
            else:
                raise TypeError(f"Splitter child must be TaskNode/SubgraphNode/LoopSubgraphNode, got {type(child)}")

        if node.max_workers <= 1 or len(children) <= 1:
            for ch in children:
                exec_child(ch)
            return

        errors = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=node.max_workers) as ex:
            futs = [ex.submit(exec_child, ch) for ch in children]
            for fut in concurrent.futures.as_completed(futs):
                try:
                    fut.result()
                except Exception as exc:
                    errors.append(exc)
        if errors:
            if len(errors) == 1:
                raise errors[0]
            summary = "; ".join(str(e) for e in errors)
            raise GraphFailure(full_path, f"{len(errors)} splitter branch(es) failed: {summary}")
