"""Regression tests for the portable graph engine."""
import json
import tempfile
import time
import unittest
from pathlib import Path

from kgraph_runner import (
    GraphFailure,
    GraphRunner,
    LoopSubgraphNode,
    SplitterNode,
    SubgraphNode,
    TaskNode,
    sha256_file,
    sha256_json,
)

class GraphLibraryTests(unittest.TestCase):
    def test_live_task_state_and_interrupted_cache_recovery(self):
        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "graph_state_live.json"
            calls = []

            def execute(_ctx, attempt):
                record = json.loads(state.read_text())["nodes"]["outer/leaf"]
                self.assertEqual((record["status"], record["attempts"]), ("running", attempt))
                self.assertEqual(record["input_hash"], "same-input")
                self.assertEqual(record["kind"], "code")
                self.assertEqual(record["started_at"], record["updated_at"])
                calls.append(attempt)
                if len(calls) == 1:
                    raise KeyboardInterrupt()

                return attempt

            leaf = TaskNode(id="leaf", kind="code", input_hash_fn=lambda _: "same-input", run_fn=execute,
                            max_retries=1, verify_fn=lambda _ctx, attempt: (attempt == 1, "retry", "verification"))
            graph = [SubgraphNode(id="outer", nodes=[leaf])]
            with self.assertRaises(KeyboardInterrupt):
                GraphRunner(state, logger=lambda _: None).run(graph, {})
            self.assertEqual(json.loads(state.read_text())["nodes"]["outer/leaf"]["status"], "running")
            GraphRunner(state, logger=lambda _: None).run(graph, {})
            self.assertEqual(json.loads(state.read_text())["nodes"]["outer/leaf"]["status"], "passed")
            GraphRunner(state, logger=lambda _: None).run(graph, {})
            self.assertEqual(calls, [0, 0, 1])

    def test_report_identifies_a_prefix_even_if_its_source_check_is_cached(self):
        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "graph_state_trell.json"
            source = TaskNode(id="source", kind="code", input_hash_fn=lambda _: "source-hash", run_fn=lambda _c, _a: True)
            review = TaskNode(id="review", kind="gate", deps=("source",), input_hash_fn=lambda _: "review-hash",
                              run_fn=lambda _c, _a: True)
            station = SubgraphNode(id="trell", nodes=[source, review])
            full = GraphRunner(state, logger=lambda _m: None)
            full.run([station], {})
            full_report = json.loads(full.write_run_report().read_text())
            self.assertEqual(full_report["scope"], {
                "requested_until": None, "stop_reached": False, "reached_nodes": ["trell/source", "trell/review"],
            })

            prefix = GraphRunner(state, until="source", logger=lambda _m: None)
            prefix.run([station], {})
            prefix_report = json.loads(prefix.write_run_report().read_text())
            self.assertEqual(prefix_report["summary"]["cached"], 1)
            self.assertEqual(prefix_report["scope"], {
                "requested_until": "source", "stop_reached": True, "reached_nodes": ["trell/source"],
            })

    def test_nested_subgraphs_cache_and_single_leaf_retry(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            state_p = root / "graph_state.json"
            calls = []

            def build_leaf(item_id: str, _ctx):
                out_f = root / f"{item_id}.txt"
                return SubgraphNode(
                    id=item_id,
                    nodes=[
                        TaskNode(
                            id="make",
                            kind="worker",
                            input_hash_fn=lambda _c: sha256_json({"id": item_id}),
                            outputs_fn=lambda _c: [out_f],
                            run_fn=lambda _c, _a: (calls.append(f"make:{item_id}"), out_f.write_text(item_id)),
                        ),
                        TaskNode(
                            id="check",
                            kind="verifier",
                            deps=("make",),
                            input_hash_fn=lambda _c: sha256_json({"id": item_id, "v": out_f.read_text()}),
                            run_fn=lambda _c, _a: calls.append(f"check:{item_id}"),
                        ),
                    ],
                )

            parent = SubgraphNode(
                id="parent",
                nodes=[
                    SplitterNode(id="items", items_fn=lambda _c: ["a", "b", "c"], child_builder_fn=build_leaf, max_workers=2),
                    TaskNode(id="final", kind="verifier", deps=("items",), input_hash_fn=lambda _c: "f1", run_fn=lambda _c, _a: calls.append("final")),
                ],
            )
            GraphRunner(state_p, logger=lambda _m: None).run([parent], {})
            self.assertEqual(len(calls), 7)

            calls.clear()
            GraphRunner(state_p, logger=lambda _m: None).run([parent], {})
            self.assertEqual(calls, [])

            GraphRunner(state_p, retry_nodes=["parent/items/b/make"], logger=lambda _m: None).run([parent], {})
            self.assertEqual(calls, ["make:b"])

    def test_self_healing_repair_loop_and_circuit_breaker(self):
        with tempfile.TemporaryDirectory() as td:
            state_p = Path(td) / "state.json"
            ctx = {"fixed": False}
            n_heal = TaskNode(
                id="heal",
                kind="worker",
                input_hash_fn=lambda _c: "h1",
                run_fn=lambda c, _a: "ok" if c["fixed"] else "bad",
                verify_fn=lambda _c, r: (True, "", "ok") if r == "ok" else (False, "aspect_mismatch", "too tall"),
                repair_fn=lambda c, _a, _s, _d: c.__setitem__("fixed", True),
                max_retries=2,
            )
            r1 = GraphRunner(state_p, logger=lambda _m: None)
            r1.run([n_heal], ctx)
            self.assertEqual(r1.state["nodes"]["heal"]["attempts"], 2)

            attempts = []
            n_break = TaskNode(
                id="stuck",
                kind="worker",
                input_hash_fn=lambda _c: "h2",
                run_fn=lambda _c, a: attempts.append(a),
                verify_fn=lambda _c, _r: (False, "same_err", "stuck"),
                max_retries=5,
            )
            with self.assertRaises(GraphFailure) as cm:
                GraphRunner(state_p, logger=lambda _m: None).run([n_break], {})
            self.assertTrue(cm.exception.circuit_broken)
            self.assertEqual(len(attempts), 2)

    def test_loop_subgraph_targeted_repair_and_circuit_breaker(self):
        with tempfile.TemporaryDirectory() as td:
            state_p = Path(td) / "state.json"
            calls = []
            ctx = {"gap": -4}

            n_static = TaskNode(id="static", kind="code", input_hash_fn=lambda _c: "s1", run_fn=lambda _c, _a: calls.append("static"))
            n_place = TaskNode(id="place", kind="code", deps=("static",), input_hash_fn=lambda c: f"gap:{c['gap']}", run_fn=lambda c, _a: calls.append(f"place:{c['gap']}"))

            def repair_gap(c, _attempt, _sig, _detail):
                c["gap"] = 4
                return ["place"]  # Only re-run 'place'; keep 'static' cached!

            loop_sg = LoopSubgraphNode(
                id="scene_loop",
                nodes=[n_static, n_place],
                verify_fn=lambda c: (True, "", "gap ok") if c["gap"] >= 1 else (False, "gap_fail", f"gap={c['gap']}"),
                repair_fn=repair_gap,
                max_retries=2,
            )
            runner = GraphRunner(state_p, logger=lambda _m: None)
            runner.run([loop_sg], ctx)
            self.assertEqual(calls, ["static", "place:-4", "place:4"])  # 'static' ran ONCE!
            self.assertEqual(runner.state["nodes"]["scene_loop"]["attempts"], 2)

            # Subgraph circuit breaker trips on 2 identical errors
            bad_sg = LoopSubgraphNode(
                id="bad_loop",
                nodes=[TaskNode(id="noop", kind="code", input_hash_fn=lambda _c: "x", run_fn=lambda _c, _a: None)],
                verify_fn=lambda _c: (False, "stuck_constraint", "never passes"),
                max_retries=5,
            )
            with self.assertRaises(GraphFailure) as cm:
                GraphRunner(state_p, logger=lambda _m: None).run([bad_sg], {})
            self.assertTrue(cm.exception.circuit_broken)
            self.assertEqual(runner.state["nodes"].get("bad_loop"), None)

    def test_timeout_and_worker_budget_guardrails(self):
        with tempfile.TemporaryDirectory() as td:
            state_p = Path(td) / "state.json"
            # Timeout catches slow node
            slow = TaskNode(
                id="hung",
                kind="code",
                input_hash_fn=lambda _c: "t1",
                run_fn=lambda _c, _a: time.sleep(0.25),
                timeout_s=0.05,
            )
            with self.assertRaises(GraphFailure) as cm_t:
                GraphRunner(state_p, logger=lambda _m: None).run([slow], {})
            self.assertIn("timeout", str(cm_t.exception).lower())

            # Budget cap stops after max_worker_calls=1 (while cache hits cost 0)
            w1 = TaskNode(id="w1", kind="worker", input_hash_fn=lambda _c: "a", run_fn=lambda _c, _a: "ok")
            w2 = TaskNode(id="w2", kind="worker", deps=("w1",), input_hash_fn=lambda _c: "b", run_fn=lambda _c, _a: "ok")
            with self.assertRaises(GraphFailure) as cm_b:
                GraphRunner(state_p, max_worker_calls=1, logger=lambda _m: None).run([w1, w2], {})
            self.assertTrue(cm_b.exception.budget_exceeded)

            # Re-running with max_worker_calls=1 now succeeds because w1 is CACHED (0 cost)!
            r2 = GraphRunner(state_p, max_worker_calls=1, logger=lambda _m: None)
            r2.run([w1, w2], {})
            self.assertEqual(r2.worker_calls_used, 1)

    def test_parallel_waves_respect_dependencies_and_resource_mutex(self):
        import threading

        with tempfile.TemporaryDirectory() as td:
            state_p = Path(td) / "state.json"
            barrier = threading.Barrier(2, timeout=2.0)
            events = []
            ev_lock = threading.Lock()
            unity_active = 0
            unity_max_active = 0

            def run_parallel_sibling(label):
                def _fn(_c, _a):
                    barrier.wait()
                    with ev_lock:
                        events.append(label)
                return _fn

            def run_unity_sibling(label):
                def _fn(_c, _a):
                    nonlocal unity_active, unity_max_active
                    with ev_lock:
                        unity_active += 1
                        unity_max_active = max(unity_max_active, unity_active)
                    time.sleep(0.05)
                    with ev_lock:
                        events.append(label)
                        unity_active -= 1
                return _fn

            nodes = [
                TaskNode("p1", "code", run_parallel_sibling("p1"), lambda _c: "p1"),
                TaskNode("p2", "code", run_parallel_sibling("p2"), lambda _c: "p2"),
                TaskNode("u1", "code", run_unity_sibling("u1"), lambda _c: "u1", deps=("p1", "p2"), resource="unity"),
                TaskNode("u2", "code", run_unity_sibling("u2"), lambda _c: "u2", deps=("p1", "p2"), resource="unity"),
                TaskNode("join", "code", lambda _c, _a: events.append("join"), lambda _c: "j", deps=("u1", "u2")),
            ]
            GraphRunner(state_p, max_parallel_nodes=4, logger=lambda _m: None).run(nodes, {})
            self.assertEqual(set(events[:2]), {"p1", "p2"})
            self.assertEqual(set(events[2:4]), {"u1", "u2"})
            self.assertEqual(events[4], "join")
            self.assertEqual(unity_max_active, 1)
