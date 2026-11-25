"""Regression tests for the portable workflow helpers."""

import json
import sys
import tempfile
import unittest
from pathlib import Path

from kgraph_runner import (
    DEFAULT_IGNORE_DIRS,
    GraphFailure,
    GraphRunner,
    hash_globs,
    hash_paths,
    shell_task,
    subagent_gate,
)


class HelperTests(unittest.TestCase):
    def setUp(self):
        workspace = tempfile.TemporaryDirectory()
        self.addCleanup(workspace.cleanup)
        self.root = Path(workspace.name)

    def write_file(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
        return target

    def runner(self, name="state.json"):
        return GraphRunner(self.root / name, logger=lambda _message: None)

    def test_hash_paths_skips_default_ignored_directories(self):
        source = self.write_file("project/src/main.py", "source")
        expected = hash_paths([source], root=self.root)
        ignored = []
        for directory in DEFAULT_IGNORE_DIRS:
            ignored.append(self.write_file(f"project/{directory}/hidden.py", "ignored"))
        self.assertEqual(hash_paths(["project", *ignored], root=self.root), expected)

    def test_hash_paths_accepts_custom_ignores(self):
        source = self.write_file("project/main.py", "source")
        self.write_file("project/generated/copy.py", "generated")
        self.assertEqual(
            hash_paths(["project"], root=self.root, ignore_dirs={"generated"}),
            hash_paths([source], root=self.root),
        )

    def test_hash_paths_is_independent_of_order_and_root_location(self):
        first = self.write_file("one/src/a.py", "first")
        second = self.write_file("one/src/b.py", "second")
        root = self.root / "one"
        expected = hash_paths(["src"], root=root)
        self.assertEqual(hash_paths([second, first, "src/a.py"], root=root), expected)
        self.write_file("two/src/b.py", "second")
        self.write_file("two/src/a.py", "first")
        self.assertEqual(hash_paths(["src"], root=self.root / "two"), expected)

    def test_hash_paths_detects_changes_to_contents_and_names(self):
        source = self.write_file("src/one.py", "before")
        before = hash_paths(["src"], root=self.root)
        source.write_text("after", encoding="utf-8")
        after = hash_paths(["src"], root=self.root)
        self.assertNotEqual(after, before)
        source.rename(source.with_name("two.py"))
        self.assertNotEqual(hash_paths(["src"], root=self.root), after)

    def test_hash_globs_matches_overlapping_patterns_and_ignores_directories(self):
        source = self.write_file("pkg/source.py", "source")
        config = self.write_file("settings.json", "{}")
        self.write_file("pkg/.venv/site.py", "ignored")
        self.write_file("README.txt", "excluded by pattern")
        self.assertEqual(
            hash_globs(["**/*.py", "pkg/source.py", "*.json"], root=self.root),
            hash_paths([source, config], root=self.root),
        )

    def test_shell_task_runs_in_requested_directory_and_writes_log(self):
        self.write_file("work/input.txt", "local file")
        log = self.root / "logs/command.log"
        task = shell_task(
            "check",
            [sys.executable, "-c",
             "import os, sys; from pathlib import Path; "
             "print(Path('input.txt').read_text()); print(os.environ['EXAMPLE_MODE'], file=sys.stderr)"],
            input_hash_fn=lambda _ctx: "input",
            cwd=self.root / "work",
            env={"EXAMPLE_MODE": "test"},
            log_path=log,
        )
        runner = self.runner()
        runner.run([task], {})
        self.assertEqual(runner.state["nodes"]["check"]["status"], "passed")
        self.assertEqual(log.read_text(encoding="utf-8"), "local file\ntest\n")

    def test_shell_task_uses_exit_code_as_failure_signature(self):
        task = shell_task(
            "fail",
            [sys.executable, "-c", "import sys; sys.stderr.write('check failed'); sys.exit(7)"],
            input_hash_fn=lambda _ctx: "input",
        )
        runner = self.runner()
        with self.assertRaisesRegex(GraphFailure, "Command exited with code 7: check failed"):
            runner.run([task], {})
        self.assertEqual(runner.state["nodes"]["fail"]["error_signature"], "exit_7")

    def test_shell_task_checks_custom_verifier(self):
        def verify(ctx, result):
            if result.stdout.strip() == ctx["expected"]:
                return True, "", "Output verified"
            return False, "unexpected_output", "Output did not match"

        task = shell_task(
            "verify",
            [sys.executable, "-c", "print('ready')"],
            input_hash_fn=lambda ctx: ctx["expected"],
            verify_fn=verify,
        )
        runner = self.runner()
        with self.assertRaisesRegex(GraphFailure, "Output did not match"):
            runner.run([task], {"expected": "waiting"})
        self.assertEqual(runner.state["nodes"]["verify"]["error_signature"], "unexpected_output")
        runner.run([task], {"expected": "ready"})
        self.assertEqual(runner.state["nodes"]["verify"]["detail"], "Output verified")

    def test_shell_task_times_out_and_captures_partial_log(self):
        log = self.root / "timeout.log"
        task = shell_task(
            "slow",
            [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(30)"],
            input_hash_fn=lambda _ctx: "input",
            timeout_s=1,
            log_path=log,
        )
        runner = self.runner()
        with self.assertRaisesRegex(GraphFailure, "Command timed out"):
            runner.run([task], {})
        self.assertEqual(runner.state["nodes"]["slow"]["error_signature"], "timeout")
        self.assertEqual(log.read_text(encoding="utf-8"), "started\n")

    def test_subagent_gate_pauses_and_writes_packet_for_missing_artifact(self):
        packet = self.root / "packets/request.json"
        artifact = self.root / "approval.json"
        request = {"action": "review", "outputs": [str(artifact)]}
        task = subagent_gate(
            "approval",
            packet_path=packet,
            artifact_paths=[artifact],
            packet_fn=lambda _ctx: request,
            input_hash_fn=lambda _ctx: "revision",
        )
        runner = self.runner()
        with self.assertRaises(GraphFailure) as paused:
            runner.run([task], {})
        self.assertTrue(paused.exception.awaiting_subagents)
        self.assertEqual(paused.exception.packet_path, str(packet))
        self.assertEqual(runner.state["nodes"]["approval"]["status"], "awaiting_subagents")
        self.assertEqual(json.loads(packet.read_text(encoding="utf-8")), request)
        self.assertEqual(list(packet.parent.iterdir()), [packet])

    def test_subagent_gate_leaves_previous_packet_intact_on_serialization_failure(self):
        packet = self.write_file("packets/request.json", '{"revision": 1}\n')
        task = subagent_gate(
            "approval",
            packet_path=packet,
            artifact_paths=[self.root / "approval.json"],
            packet_fn=lambda _ctx: {"revision": 2, "invalid": object()},
            input_hash_fn=lambda _ctx: "revision",
        )
        with self.assertRaisesRegex(GraphFailure, "not JSON serializable"):
            self.runner().run([task], {})
        self.assertEqual(json.loads(packet.read_text(encoding="utf-8")), {"revision": 1})
        self.assertEqual(list(packet.parent.iterdir()), [packet])

    def test_subagent_gate_verifies_artifacts_and_caches_only_valid_outputs(self):
        artifact = self.write_file("approval.txt", "pending")
        packet = self.root / "request.json"
        checks = []

        def verify(_ctx, paths):
            checks.append(paths)
            if paths[0].read_text(encoding="utf-8") == "approved":
                return True, "", "Approved"
            return False, "approval_pending", "Approval required"

        task = subagent_gate(
            "approval",
            packet_path=packet,
            artifact_paths=lambda ctx: [ctx["artifact"]],
            packet_fn=lambda _ctx: {"action": "approve"},
            input_hash_fn=lambda _ctx: "revision",
            verify_fn=verify,
        )
        ctx = {"artifact": artifact}
        with self.assertRaises(GraphFailure) as paused:
            self.runner().run([task], ctx)
        self.assertTrue(paused.exception.awaiting_subagents)
        self.assertEqual(paused.exception.reason, "Approval required")
        self.assertEqual(json.loads(packet.read_text(encoding="utf-8")), {"action": "approve"})

        artifact.write_text("approved", encoding="utf-8")
        resumed = self.runner()
        resumed.run([task], ctx)
        self.assertEqual(resumed.state["nodes"]["approval"]["detail"], "Approved")

        cached = self.runner()
        cached.run([task], ctx)
        self.assertEqual(cached.trace[0]["status"], "cached")
        self.assertEqual(checks, [(artifact,), (artifact,)])

        artifact.write_text("pending again", encoding="utf-8")
        with self.assertRaises(GraphFailure) as invalidated:
            self.runner().run([task], ctx)
        self.assertTrue(invalidated.exception.awaiting_subagents)
        self.assertEqual(checks, [(artifact,), (artifact,), (artifact,)])


if __name__ == "__main__":
    unittest.main()
