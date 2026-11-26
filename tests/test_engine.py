"""Regression tests for the portable graph engine."""
import concurrent.futures
import datetime
import json
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

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


class GraphRunnerImprovementsTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.state = self.root / "graph_state.json"

    def runner(self, **options):
        return GraphRunner(self.state, logger=lambda _: None, **options)

    @staticmethod
    def task(node_id, run_fn=lambda _ctx, _attempt: None, **options):
        return TaskNode(node_id, "code", run_fn, lambda _ctx: node_id, **options)

    def test_plan_does_not_create_state_directory_before_a_write(self):
        state = self.root / ".kgraph" / "graph_state.json"
        runner = GraphRunner(state, logger=lambda _: None)
        self.assertEqual(runner.plan([self.task("check")], {})[0]["status"], "will_run")
        self.assertFalse(state.parent.exists())
        runner.run([self.task("check")], {})
        self.assertTrue(state.is_file())
        report_only = GraphRunner(self.root / "report-only" / "graph_state.json")
        self.assertTrue(report_only.write_run_report().is_file())

    def test_running_and_failed_records_expose_task_outputs(self):
        log = self.root / "build.log"
        for retries, status in ((0, "failed"), (2, "circuit_broken")):
            with self.subTest(status=status):
                state = self.root / f"{status}.json"

                def run(_ctx, _attempt):
                    record = json.loads(state.read_text())["nodes"]["build"]
                    self.assertEqual((record["status"], record["outputs"]), ("running", [str(log)]))
                    log.write_text("build output\n")

                task = self.task("build", run, outputs_fn=lambda _: [log], max_retries=retries,
                                 verify_fn=lambda *_: (False, "compiler", "build failed"))
                runner = GraphRunner(state, logger=lambda _: None)
                with self.assertRaisesRegex(GraphFailure, "build failed"):
                    runner.run([task], {})
                record = json.loads(state.read_text())["nodes"]["build"]
                self.assertEqual((record["status"], record["outputs"]), (status, [str(log)]))

    def test_metadata_and_repair_exceptions_persist_failed_task_status(self):
        for callback in ("outputs_fn", "sha256_file", "input_hash_fn", "repair_fn"):
            with self.subTest(callback=callback):
                state = self.root / f"{callback}.json"
                log = self.root / f"{callback}.log"
                error = OSError(f"{callback} failed")

                def run(ctx, _attempt):
                    log.write_text("task log\n")
                    ctx["ran"] = True

                def metadata(ctx, value, name):
                    if name == callback and ctx.get("ran"):
                        raise error
                    return value

                def repair(*_):
                    raise error

                task = self.task("build", run, outputs_fn=lambda ctx: metadata(ctx, [log], "outputs_fn"))
                task.input_hash_fn = lambda ctx: metadata(ctx, "input", "input_hash_fn")
                if callback == "repair_fn":
                    task.verify_fn = lambda *_: (False, "compiler", "build failed")
                    task.repair_fn = repair
                    task.max_retries = 1
                runner = GraphRunner(state, logger=lambda _: None)
                with patch("kgraph_runner.engine.sha256_file", side_effect=error if callback == "sha256_file"
                           else sha256_file), self.assertRaises(OSError) as raised:
                    runner.run([task], {})
                self.assertIs(raised.exception, error)
                record = json.loads(state.read_text())["nodes"]["build"]
                self.assertEqual((record["status"], record["attempts"], record["outputs"]), ("failed", 1, [str(log)]))
                self.assertEqual(record["detail"], str(error))

    def test_parallel_branches_acquire_opposite_nested_resources_without_deadlock(self):
        script = textwrap.dedent("""
            import sys
            import threading
            from pathlib import Path
            from kgraph_runner import GraphRunner, LoopSubgraphNode, SubgraphNode, TaskNode

            entered = threading.Barrier(2)

            def on_event(event):
                if event['event'] == 'node_start' and event['node'] in ('left', 'right'):
                    # The old lock order lets both branches enter before either asks for its second lock.
                    try:
                        entered.wait(1)
                    except threading.BrokenBarrierError:
                        pass

            def leaf(resource):
                return TaskNode('leaf', 'code', lambda *_: None, lambda _: 'input', resource=resource)

            left = SubgraphNode('left', [SubgraphNode('nested', [leaf('B')])], resource='A')
            right = LoopSubgraphNode('right', [SubgraphNode('nested', [leaf('A')])],
                                     lambda _: (True, '', ''), resource='B')
            runner = GraphRunner(Path(sys.argv[1]), max_parallel_nodes=2, on_event=on_event)
            runner.run([left, right], {})
            assert {entry['node'] for entry in runner.trace if entry['status'] == 'passed'} == {
                'left/nested/leaf', 'right/nested/leaf', 'right',
            }
        """)
        result = subprocess.run([sys.executable, "-c", script, str(self.state)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_dynamic_scheduler_releases_dependents_before_an_unrelated_node_finishes(self):
        slow_started = threading.Event()
        dependent_started = threading.Event()

        def run_slow(_ctx, _attempt):
            slow_started.set()
            self.assertTrue(dependent_started.wait(3), "fast dependency remained behind the slow sibling")

        def run_fast(_ctx, _attempt):
            self.assertTrue(slow_started.wait(3))

        runner = self.runner(max_parallel_nodes=2)
        runner.run([
            self.task("slow", run_slow),
            self.task("fast", run_fast),
            self.task("dependent", lambda *_: dependent_started.set(), deps=("fast",)),
        ], {})
        self.assertTrue(dependent_started.is_set())

    def test_dynamic_scheduler_stops_submitting_and_drains_active_siblings_on_failure(self):
        started = threading.Barrier(2, timeout=3)

        def fail(_ctx, _attempt):
            started.wait()
            raise GraphFailure("failed", "hard failure")

        def pause(_ctx, _attempt):
            started.wait()
            raise GraphFailure("active", "needs review", awaiting_subagents=True, packet_path="active.json")

        unexpected = []
        nodes = [
            self.task("failed", fail),
            self.task("active", pause),
            self.task("independent", lambda *_: unexpected.append("independent")),
            self.task("dependent", lambda *_: unexpected.append("dependent"), deps=("failed",)),
        ]
        runner = self.runner(max_parallel_nodes=2)
        with self.assertRaisesRegex(GraphFailure, r"2 parallel sibling\(s\) failed.*hard failure.*needs review") as raised:
            runner.run(nodes, {})
        self.assertFalse(raised.exception.awaiting_subagents)
        self.assertEqual(unexpected, [])

    def test_splitter_serializes_shared_resources_across_child_types(self):
        active = 0
        max_active = 0
        guard = threading.Lock()
        overlapping = threading.Event()

        def use_resource(_ctx, _attempt):
            nonlocal active, max_active
            with guard:
                active += 1
                max_active = max(max_active, active)
                if active > 1:
                    overlapping.set()
            overlapping.wait(0.05)
            with guard:
                active -= 1

        leaf = self.task("leaf", use_resource)
        children = [
            self.task("task", use_resource, resource="shared"),
            SubgraphNode("subgraph", [leaf], resource="shared"),
            LoopSubgraphNode("loop", [leaf], lambda _: (True, "", ""), resource="shared"),
            SplitterNode("splitter", lambda _: [leaf], lambda item, _: item, resource="shared"),
        ]
        fanout = SplitterNode("fanout", lambda _: children, lambda item, _: item, max_workers=4)
        runner = self.runner(max_parallel_nodes=4)
        runner.run([fanout], {})
        self.assertEqual(max_active, 1)
        self.assertEqual({event["node"] for event in runner.trace}, {
            "fanout/task", "fanout/subgraph/leaf", "fanout/loop/leaf", "fanout/loop", "fanout/splitter/leaf",
        })

    def test_duplicate_sibling_ids_raise_before_execution(self):
        calls = []
        duplicate = self.task("same", lambda *_: calls.append("ran"))
        runner = self.runner(max_parallel_nodes=2)
        with self.assertRaisesRegex(ValueError, "^Duplicate node ID 'same'$"):
            runner.topo_sort([duplicate, duplicate])
        with self.assertRaisesRegex(ValueError, "^Duplicate node ID 'same'$"):
            runner.run([duplicate, duplicate], {})
        with self.assertRaisesRegex(ValueError, "^Duplicate node ID 'same'$"):
            runner.plan([SubgraphNode("nested", [duplicate, duplicate])], {})
        splitter = SplitterNode("split", lambda _: [duplicate, duplicate], lambda item, _: item)
        with self.assertRaisesRegex(ValueError, "^Duplicate node ID 'same'$"):
            runner.run([splitter], {})
        self.assertEqual(calls, [])

    def test_state_writes_merge_from_independent_processes(self):
        self.state.write_text('{"version": 3, "label": "Đà Nẵng", "nodes": {}}', encoding="utf-8")
        script = textwrap.dedent("""
            import sys
            import time
            from pathlib import Path
            from kgraph_runner.engine import GraphRunner, TaskNode

            state, node_id, sibling = sys.argv[1:]
            runner = GraphRunner(Path(state), logger=lambda _: None)
            assert runner.state["label"] == "Đà Nẵng"

            def run(_ctx, _attempt):
                Path(state).with_name(node_id + ".started").touch()
                peer = Path(state).with_name(sibling + ".started")
                deadline = time.monotonic() + 4
                while not peer.exists():
                    if time.monotonic() >= deadline:
                        raise RuntimeError("sibling did not start")
                    time.sleep(0.01)

            runner.run([TaskNode(node_id, "code", run, lambda _: "héritage")], {})
            runner.write_run_report(error=node_id)
        """)

        def execute(node_id, sibling):
            return subprocess.run(
                [sys.executable, "-c", script, str(self.state), node_id, sibling],
                capture_output=True, text=True, timeout=8,
            )

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            one = executor.submit(execute, "một", "hai")
            two = executor.submit(execute, "hai", "một")
            for future in (one, two):
                result = future.result()
                self.assertEqual(result.returncode, 0, result.stderr)
        state = json.loads(self.state.read_text(encoding="utf-8"))
        self.assertEqual(state["label"], "Đà Nẵng")
        self.assertEqual({path: (record["status"], record["input_hash"]) for path, record in state["nodes"].items()},
                         {"một": ("passed", "héritage"), "hai": ("passed", "héritage")})
        report = json.loads((self.root / "run_report.json").read_text(encoding="utf-8"))
        self.assertIn(report["error"], ("một", "hai"))

    def test_atomic_write_failure_preserves_existing_state_and_report(self):
        runner = self.runner()
        runner.run([self.task("first")], {})
        report = runner.write_run_report()
        original_state = self.state.read_bytes()
        original_report = report.read_bytes()
        original_files = set(self.root.iterdir())

        with patch("kgraph_runner._files.os.fsync", side_effect=OSError("fsync failed")) as fsync:
            with self.assertRaisesRegex(OSError, "fsync failed"):
                runner.run([self.task("second")], {})
            with self.assertRaisesRegex(OSError, "fsync failed"):
                runner.write_run_report(error="new report")
        self.assertEqual(fsync.call_count, 2)
        self.assertEqual(self.state.read_bytes(), original_state)
        self.assertEqual(report.read_bytes(), original_report)
        self.assertEqual(set(self.root.iterdir()), original_files)

    def test_report_cached_nodes_use_the_latest_event_per_node(self):
        runner = self.runner()
        one, two = self.task("one"), self.task("two")
        runner.run([one, two], {})
        runner.run([one, two], {})
        runner.retry_nodes.add("two")
        runner.run([two], {})
        report = json.loads(runner.write_run_report().read_text(encoding="utf-8"))
        self.assertEqual(report["summary"]["cached"], 2)
        self.assertEqual(report["cached_nodes"], ["one"])

    def test_parallel_awaiting_failures_surface_all_packets_in_dag_and_splitter(self):
        for group in ("dag", "splitter"):
            with self.subTest(group=group):
                barrier = threading.Barrier(2, timeout=3)

                def pause(error):
                    def run(_ctx, _attempt):
                        barrier.wait()
                        raise error
                    return run

                children = [
                    self.task("first", pause(GraphFailure(
                        "first", "review first", awaiting_subagents=True,
                        packet_path="primary.json", packet_paths=["shared.json", "primary.json"],
                    ))),
                    self.task("second", pause(GraphFailure(
                        "second", "review second", awaiting_subagents=True, packet_paths=["shared.json", "other.json"],
                    ))),
                ]
                nodes = children if group == "dag" else [
                    SplitterNode("split", lambda _: children, lambda item, _: item, max_workers=2),
                ]
                runner = self.runner(max_parallel_nodes=2)
                with self.assertRaises(GraphFailure) as raised:
                    runner.run(nodes, {}, prefix=group)
                error = raised.exception
                self.assertTrue(error.awaiting_subagents)
                self.assertEqual(error.packet_path, "primary.json")
                self.assertEqual(error.packet_paths, ["primary.json", "shared.json", "other.json"])
                report_path = runner.write_run_report(
                    overall_status="awaiting_subagents", error=str(error), packet_paths=error.packet_paths,
                )
                report = json.loads(report_path.read_text(encoding="utf-8"))
                self.assertEqual(report["packet_path"], "primary.json")
                self.assertEqual(report["packet_paths"], error.packet_paths)

        report_path = self.runner().write_run_report(None, "awaiting_subagents", "paused", "legacy.json")
        report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertEqual(report["packet_paths"], ["legacy.json"])
        self.assertEqual(GraphFailure("node", "failed").packet_paths, [])

    def test_daemon_timeout_does_not_hold_the_python_process_open(self):
        script = textwrap.dedent("""
            import sys
            import threading
            from pathlib import Path
            from kgraph_runner.engine import GraphRunner

            runner = GraphRunner(Path(sys.argv[1]))
            assert runner._invoke_with_timeout(lambda: threading.current_thread().daemon, (), 1)
            try:
                runner._invoke_with_timeout(threading.Event().wait, (), 0.05)
            except TimeoutError as error:
                assert str(error) == "exceeded timeout_s=0.05s"
            else:
                raise AssertionError("Expected timeout")
        """)
        result = subprocess.run(
            [sys.executable, "-c", script, str(self.state)], capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_timeout_invocation_preserves_the_function_exception(self):
        original = TimeoutError("callback deadline")

        def fail():
            raise original

        with self.assertRaises(TimeoutError) as raised:
            self.runner()._invoke_with_timeout(fail, (), 1)
        self.assertIs(raised.exception, original)

    def test_cache_miss_reasons_and_latest_task_trace(self):
        artifact = self.root / "result.txt"
        node = TaskNode("build", "code", lambda _ctx, _attempt: artifact.write_text("result", encoding="utf-8"),
                        lambda ctx: ctx["hash"], outputs_fn=lambda _: [artifact])
        runner = self.runner()
        inputs = {"hash": "v1"}

        def check(reason, *, with_runner=runner, task=node, context=inputs):
            plan = with_runner.plan([task], context)
            self.assertEqual(plan[0]["reason"], reason)
            self.assertEqual(plan[0]["status"], "cached" if reason == "fresh" else "will_run")

        check("no_prior_state")
        runner.run([node], inputs)
        self.assertEqual(runner.trace[-1]["cache_miss_reason"], "no_prior_state")
        check("fresh")
        self.assertTrue(runner._is_fresh("build", "build", "v1", [artifact]))
        check("forced", with_runner=self.runner(force=True))
        check("retry_requested", with_runner=self.runner(retry_nodes=["build"]))
        check("not_cacheable", task=replace(node, cacheable=False))
        check("input_hash_changed", context={"hash": "v2"})
        artifact.write_text("tampered", encoding="utf-8")
        check(f"output_hash_changed:{artifact}")
        artifact.unlink()
        check(f"missing_output:{artifact}")
        runner.run([node], inputs)
        self.assertEqual(runner.trace[-1]["cache_miss_reason"], f"missing_output:{artifact}")
        runner.state["nodes"]["build"]["status"] = "running"
        check("prior_status:running")

    def test_plan_respects_nested_order_and_until_without_running_callbacks_for_tasks(self):
        def unexpected(*_):
            self.fail("dry-run called an execution, verification, or repair callback")

        def items(ctx):
            ctx["items_called"] = True
            return ["item"]

        def child(item, ctx):
            self.assertTrue(ctx["items_called"])
            return SplitterNode(item, lambda _: ["leaf"], lambda name, _: self.task(name, unexpected))

        graph = [SubgraphNode("parent", [
            self.task("stop", unexpected, deps=("split",), resource="shared"),
            self.task("after", unexpected),
            LoopSubgraphNode("prepare", [self.task("make", unexpected)], unexpected),
            SplitterNode("split", items, child, deps=("prepare",)),
        ])]
        ctx = {}
        runner = self.runner(until="prefix/parent/stop", max_parallel_nodes=8)
        plan = runner.plan(graph, ctx, prefix="prefix")
        self.assertEqual([entry["path"] for entry in plan], [
            "prefix/parent", "prefix/parent/prepare", "prefix/parent/prepare/make",
            "prefix/parent/split", "prefix/parent/split/item",
            "prefix/parent/split/item/leaf", "prefix/parent/stop",
        ])
        self.assertEqual([entry["kind"] for entry in plan], [
            "subgraph", "loop_subgraph", "code", "splitter", "splitter", "code", "code",
        ])
        self.assertEqual(plan[-1], {
            "path": "prefix/parent/stop", "id": "stop", "node_type": "TaskNode", "kind": "code",
            "deps": ["split"], "resource": "shared", "status": "will_run", "reason": "no_prior_state",
        })
        self.assertEqual(plan[0]["status"], "container")
        self.assertEqual(ctx, {})
        self.assertEqual(runner.trace, [])
        self.assertIsNone(runner.reached_path)
        self.assertFalse(self.state.exists())

    def test_plan_validates_dependencies_and_until_and_handles_unavailable_inputs(self):
        task = TaskNode("task", "code", lambda *_: self.fail("plan ran the task"),
                        lambda _: (self.root / "unavailable").read_text(encoding="utf-8"))
        splitter = SplitterNode("split", lambda ctx: ctx["items"], lambda item, _: item)
        with self.assertRaisesRegex(ValueError, "unknown sibling 'missing'"):
            self.runner().plan([replace(task, deps=("missing",))], {})
        with self.assertRaisesRegex(ValueError, "Cycle detected"):
            self.runner().plan([replace(task, deps=("task",))], {})
        with self.assertRaisesRegex(GraphFailure, "Unknown stage"):
            self.runner(until="unknown").plan([task], {})
        with self.assertRaisesRegex(GraphFailure, "Cannot stop inside runtime splitter"):
            self.runner(until="split/task").plan([splitter], {})
        with self.assertRaisesRegex(GraphFailure, "Ambiguous stage"):
            self.runner(until="task").plan([task, SubgraphNode("nested", [task])], {})
        plan = self.runner().plan([task, splitter], {})
        self.assertEqual(plan[0]["reason"], "inputs_unavailable")
        self.assertIn("unavailable", plan[0]["detail"])
        self.assertEqual(plan[1]["status"], "container")
        self.assertIn("items", plan[1]["detail"])

    def test_event_hooks_report_retry_pass_and_cache_outside_the_state_lock(self):
        events = []
        inspected = []

        def observe(event):
            events.append(event)
            unlocked = runner._lock.acquire(blocking=False)
            inspected.append(unlocked)
            if unlocked:
                runner._lock.release()
                runner.write_run_report(self.root / "from_hook.json")
            if event["event"] == "node_retry":
                raise RuntimeError("observer failed")

        runner = self.runner(on_event=observe)
        node = self.task(
            "node", lambda _ctx, attempt: attempt, max_retries=1,
            verify_fn=lambda _ctx, attempt: (True, "", "") if attempt else (False, "retry", "first attempt"),
        )
        runner.run([node], {})
        runner.run([node], {})
        self.assertEqual([event["event"] for event in events], [
            "run_start", "node_start", "node_retry", "node_start", "node_passed", "run_end",
            "run_start", "node_cached", "run_end",
        ])
        self.assertEqual(inspected, [True] * len(events))
        for event in events:
            self.assertIsNotNone(datetime.datetime.fromisoformat(event["timestamp"]).tzinfo)
        self.assertEqual((events[1]["attempt"], events[3]["attempt"]), (1, 2))
        self.assertEqual((events[2]["error_signature"], events[2]["detail"]), ("retry", "first attempt"))
        self.assertEqual((events[4]["attempts"], events[4]["cache_miss_reason"]), (2, "no_prior_state"))
        self.assertGreaterEqual(events[4]["duration_s"], 0)
        self.assertEqual((events[5]["status"], events[5]["error"]), ("passed", ""))
        self.assertEqual((events[7]["node"], events[7]["kind"], events[7]["input_hash"]), ("node", "code", "node"))

    def test_event_hooks_report_failed_circuit_broken_and_awaiting_subagents(self):
        def pause(*_):
            raise GraphFailure("pause", "needs review", awaiting_subagents=True, packet_paths=["a", "b"])

        bad = self.task("bad", verify_fn=lambda *_: (False, "bad", "verification failed"))
        cases = [
            (bad, "failed", []),
            (replace(bad, max_retries=3), "circuit_broken", []),
            (self.task("pause", pause), "awaiting_subagents", ["a", "b"]),
        ]
        for node, status, packets in cases:
            with self.subTest(status=status):
                events = []
                with self.assertRaises(GraphFailure):
                    self.runner(on_event=events.append).run([node], {})
                terminal, run_end = events[-2:]
                self.assertEqual((terminal["event"], terminal["node"], terminal["kind"]),
                                 (f"node_{status}", node.id, "code"))
                self.assertTrue(terminal["detail"])
                self.assertEqual(terminal["packet_paths"], packets)
                self.assertEqual(terminal["packet_path"], packets[0] if packets else None)
                self.assertEqual((run_end["event"], run_end["status"]), ("run_end", status))
                self.assertIn(terminal["detail"], run_end["error"])
