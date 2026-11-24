from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import textwrap
import unittest

from kgraph_runner import create_bug_batch_graph, create_desktop_bug_fix_subgraph, record_qa_result

from _support import read_json, run_workflow, write_json


BUILD = [sys.executable, "-c", "from pathlib import Path; compile(Path('values.py').read_text(), 'values.py', 'exec')"]
TEST = [sys.executable, "-B", "regression.py"]
REGRESSION = """\
import sys
from pathlib import Path
from values import alpha, beta

count_path = Path("test_runs.txt")
count = int(count_path.read_text()) + 1 if count_path.exists() else 1
count_path.write_text(str(count))
print("Checking the desktop settings", flush=True)
if alpha != 10:
    print(f"ERROR at 2026-01-02 03:04:{count:02}.123, after 0.00{count}s at 0xabc{count}", file=sys.stderr)
    assert alpha == 10, "test A rejected the alpha setting"
assert beta == 20, "test B rejected the beta setting"
print("Both settings passed")
"""


class BugFixTest(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(prefix="kgraph bug fixes ")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.repo = self.root / "app repo"
        self.repo.mkdir()
        self.work = self.root / ".kgraph" / "fixes"
        self.directory = self.work / "bugs" / "app-42"
        self.write_values(0, 0)
        (self.repo / "regression.py").write_text(REGRESSION, encoding="utf-8")

    def run_workflow(self, **options):
        report = run_workflow(self, self.root, **options)
        packet = read_json(Path(report["packet_path"])) if report["packet_path"] else None
        return report, packet

    def write_values(self, alpha, beta):
        (self.repo / "values.py").write_text(f"alpha = {alpha}\nbeta = {beta}\n", encoding="utf-8")

    def write_workflow(self, *, build=BUILD, repro=None, test=TEST, timeout=300, custom=False,
                       spec=None, watch_globs=("*.py",), max_retries=4):
        if spec is None:
            spec = {"id": "app-42", "title": "Restore desktop settings", "expected": "Both settings load"}
        (self.root / "workflow.py").write_text(textwrap.dedent(f"""\
            from pathlib import Path
            from kgraph_runner import create_desktop_bug_fix_subgraph

            def verify(ctx):
                return (True, "", "accepted") if ctx.get("approve", True) else (False, "custom", "rejected")

            def build_graph():
                return [create_desktop_bug_fix_subgraph(
                    {spec!r},
                    work_dir=Path({str(self.work)!r}), repo_dir=Path({str(self.repo)!r}),
                    build_cmd={build!r}, repro_cmd={repro!r}, test_cmd={test!r}, watch_globs={watch_globs!r},
                    max_retries={max_retries!r},
                    timeout_s={timeout!r}, verify_fn={"verify" if custom else "None"},
                )]
        """), encoding="utf-8")

    def deliver(self, packet, **fields):
        path, = map(Path, packet["outputs"])
        report = {
            "revision": packet["revision"], "root_cause": "The desktop defaults were incorrect.",
            "summary": "Corrected the persisted settings.", "changed_files": ["values.py"],
            **fields,
        }
        write_json(path, report)

    def test_fix_loop_across_processes_and_watched_source_changes(self):
        repro = [sys.executable, "-B", "-c", "from values import alpha; print('Reproduction check completed')"]
        self.write_workflow(repro=repro)
        report, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(report["status"], "awaiting_subagents")
        self.assertEqual(Path(report["packet_path"]), self.directory / "fix_packet.json")
        self.assertEqual(packet["revision"], 0)
        self.assertEqual(packet["bug_spec"]["expected"], "Both settings load")
        self.assertEqual(Path(packet["repo_dir"]), self.repo)
        self.assertEqual(packet["commands"]["test"], TEST)
        self.assertEqual(read_json(Path(packet["spec_path"]))["watch_globs"], ["*.py"])
        self.assertFalse((self.directory / "test.log").exists())

        self.write_values(5, 20)
        self.deliver(packet)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["revision"], 1)
        failure_a = packet["repair_feedback"][-1]
        self.assertRegex(failure_a["error_signature"], r"^test_exit_1:[0-9a-f]{8}$")
        self.assertIn("test A rejected", failure_a["detail"])
        self.assertEqual(read_json(self.directory / "test_result.json")["error_signature"],
                         failure_a["error_signature"])
        self.assertEqual(set(packet["log_paths"]), {"build", "repro", "test"})
        log = Path(packet["log_paths"]["test"]).read_text(encoding="utf-8")
        self.assertIn("Checking the desktop settings", log)
        self.assertIn("AssertionError: test A rejected", log)

        self.write_values(10, 15)
        self.deliver(packet)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["revision"], 2)
        self.assertEqual(packet["repair_feedback"][0], failure_a)
        failure_b = packet["repair_feedback"][-1]
        self.assertNotEqual(failure_b["error_signature"], failure_a["error_signature"])
        self.assertIn("test B rejected", failure_b["detail"])
        self.assertFalse((self.directory / "final_summary.json").exists())

        self.write_values(10, 20)
        self.deliver(packet)
        self.run_workflow()
        for filename in ("build_result.json", "test_result.json", "verification.json"):
            self.assertTrue(read_json(self.directory / filename)["ok"])
        summary_path = self.directory / "final_summary.json"
        summary = read_json(summary_path)
        self.assertEqual(summary["bug_id"], "app-42")
        self.assertEqual(summary["title"], "Restore desktop settings")
        self.assertEqual(summary["changed_files"], ["values.py"])
        self.assertEqual((summary["attempts"], summary["revision"]), (3, 2))
        qa_path = self.directory / "manual_qa.json"
        qa = read_json(qa_path)
        self.assertEqual(qa["workflow_type"], "desktop-bug-fix")
        self.assertEqual([(case["id"], case["ai_attempts"], case["automated_status"], case["manual_qa"])
                          for case in qa["cases"]], [("app-42", 3, "pass", "not_tested")])
        record_qa_result(qa_path, case_id="app-42", manual_qa="pass")
        self.assertEqual(read_json(self.directory / "repair_state.json")["consecutive_repairs"], 0)

        report, _ = self.run_workflow(max_worker_calls=0)
        self.assertEqual(report["summary"]["worker_calls_used"], 0)
        self.assertEqual(len(report["cached_nodes"]), 5)
        self.assertEqual((self.repo / "test_runs.txt").read_text(), "3")

        with (self.repo / "values.py").open("a", encoding="utf-8") as source:
            source.write("# Source changed after verification\n")
        report, _ = self.run_workflow(max_worker_calls=0)
        self.assertEqual(report["cached_nodes"], ["app-42/triage_and_plan", "app-42/apply_fix"])
        self.assertEqual((self.repo / "test_runs.txt").read_text(), "4")

        (self.directory / "test.log").unlink()
        summary_path.unlink()
        self.run_workflow(max_worker_calls=0)
        self.assertEqual((self.repo / "test_runs.txt").read_text(), "5")
        self.assertEqual(read_json(summary_path), summary)
        case, = read_json(qa_path)["cases"]
        self.assertEqual((case["manual_qa"], case["resolved_on_attempt"]), ("pass", 3))

    def test_same_failure_breaks_even_with_new_delivery_and_volatile_output(self):
        self.write_workflow(build=None)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.write_values(4, 20)
        self.deliver(packet)
        _, packet = self.run_workflow(status="awaiting_subagents")
        failure = packet["repair_feedback"][-1]
        self.write_values(5, 20)
        self.deliver(packet)
        report, _ = self.run_workflow(status="circuit_broken")
        self.assertEqual(report["status"], "circuit_broken")
        result = read_json(self.directory / "test_result.json")
        self.assertEqual(result["error_signature"], failure["error_signature"])
        self.assertNotEqual(result["detail"], failure["detail"])
        self.assertEqual((self.repo / "test_runs.txt").read_text(), "2")

    def test_build_failure_skips_repro_and_tests_and_requires_new_revision(self):
        repro = [sys.executable, "-c", "from pathlib import Path; Path('repro_ran').touch()"]
        self.write_workflow(repro=repro)
        _, packet = self.run_workflow(status="awaiting_subagents")
        (self.repo / "values.py").write_text("alpha =\n", encoding="utf-8")
        self.deliver(packet)
        _, packet = self.run_workflow(status="awaiting_subagents")
        failure = packet["repair_feedback"][-1]
        self.assertRegex(failure["error_signature"], r"^build_exit_1:[0-9a-f]{8}$")
        self.assertIn("SyntaxError", failure["detail"])
        self.assertEqual(read_json(self.directory / "build_result.json"),
                         read_json(self.directory / "test_result.json"))
        self.assertEqual(set(packet["log_paths"]), {"build"})
        self.assertIn("SyntaxError", Path(packet["log_paths"]["build"]).read_text(encoding="utf-8"))
        self.assertFalse((self.directory / "test.log").exists())
        self.assertFalse((self.repo / "repro_ran").exists())
        self.assertFalse((self.repo / "test_runs.txt").exists())

        self.write_values(10, 20)
        self.deliver(packet, revision=0)
        report, same_packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(same_packet, packet)
        self.assertIn("revision 1", report["error"])
        self.deliver(packet)
        self.run_workflow()
        self.assertTrue((self.repo / "repro_ran").is_file())

    def test_timeout_preserves_output_and_requests_repair(self):
        repro = [sys.executable, "-c", "import time; print('Starting repro', flush=True); time.sleep(30)"]
        self.write_workflow(build=None, repro=repro, test=None, timeout=2)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.deliver(packet)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["repair_feedback"][-1]["error_signature"], "repro_timeout")
        self.assertIn("Starting repro", Path(packet["log_paths"]["repro"]).read_text(encoding="utf-8"))

    def test_custom_verifier_and_report_validation(self):
        self.write_workflow(build=None, test=None, custom=True)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.deliver(packet, root_cause=" ", changed_files="values.py")
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["repair_feedback"][-1]["error_signature"], "invalid_fix_report")
        self.deliver(packet)
        self.run_workflow()
        report, _ = self.run_workflow(max_worker_calls=0)
        self.assertEqual(len(report["cached_nodes"]), 5)
        _, packet = self.run_workflow(status="awaiting_subagents", ctx={"approve": False})
        self.assertEqual(packet["repair_feedback"][-1]["error_signature"], "custom")

    def test_batch_orders_shared_repo_and_writes_consolidated_reports(self):
        bugs = [
            {"id": "alpha-set", "title": "Restore Alpha"},
            {"id": "beta-set", "title": "Restore Beta", "max_retries": 2, "watch_globs": ["values.py"],
             "test_cmd": [sys.executable, "-B", "-c", "from values import beta; assert beta == 20"]},
        ]
        default = [sys.executable, "-B", "-c", "from values import alpha; assert alpha == 10"]
        (self.root / "workflow.py").write_text(textwrap.dedent(f"""\
            from pathlib import Path
            from kgraph_runner import create_bug_batch_graph

            def build_graph():
                return create_bug_batch_graph(
                    {bugs!r}, work_dir=Path({str(self.work)!r}), repo_dir=Path({str(self.repo)!r}),
                    test_cmd={default!r}, watch_globs=("*.py",),
                )
        """), encoding="utf-8")
        json_path = self.work / "batch_report.json"
        md_path = self.work / "batch_report.md"
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["bug_id"], "alpha-set")
        self.assertEqual(packet["commands"]["test"], default)
        self.assertFalse((self.work / "bugs/beta-set/fix_packet.json").exists())
        self.write_values(10, 0)
        self.deliver(packet, summary="Set the Alpha default.")
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["bug_id"], "beta-set")
        self.assertEqual(packet["commands"]["test"], bugs[1]["test_cmd"])
        self.assertEqual(packet["watch_globs"], ["values.py"])
        self.assertFalse(json_path.exists())
        self.assertFalse(md_path.exists())
        self.write_values(10, 20)
        self.deliver(packet, summary="Set the Beta default.")
        self.run_workflow()
        batch = read_json(json_path)
        self.assertEqual(batch["fixed_count"], 2)
        self.assertEqual([bug["bug_id"] for bug in batch["bugs"]], ["alpha-set", "beta-set"])
        qa = read_json(self.work / "manual_qa.json")
        self.assertEqual([(case["id"], case["ai_attempts"], case["automated_status"]) for case in qa["cases"]],
                         [("alpha-set", 1, "pass"), ("beta-set", 1, "pass")])
        self.assertEqual(read_json(self.work / "qa_scorecard.json")["counts"]["not_tested"], 2)
        self.assertIn("QA Scorecard", (self.work / "qa_scorecard.md").read_text(encoding="utf-8"))
        markdown = md_path.read_text(encoding="utf-8")
        for text in ("alpha-set", "beta-set", "Restore Alpha", "Restore Beta",
                     "Set the Alpha default.", "Set the Beta default."):
            self.assertIn(text, markdown)

    def test_multiline_bug_spec_and_watched_files(self):
        spec = {"id": "app-42", "title": 'Restore the "settings"', "description": "First line\nSecond line"}
        self.write_workflow(spec=spec, watch_globs=("values.py", "regression.py"), max_retries=2)
        _, packet = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(packet["bug_spec"]["title"], spec["title"])
        self.assertEqual(packet["bug_spec"]["description"], "First line\nSecond line")
        self.assertEqual(packet["watch_globs"], ["values.py", "regression.py"])
        self.write_values(10, 20)
        self.deliver(packet)
        self.run_workflow()
        self.assertTrue((self.directory / "final_summary.json").is_file())

    def test_builder_validation_and_custom_node_id(self):
        with self.assertRaisesRegex(ValueError, "at least one"):
            create_desktop_bug_fix_subgraph({"id": "app-42"}, work_dir=self.work, repo_dir=self.repo)
        for spec in ({}, {"id": ""}, {"id": 1}, {"id": "path/to/bug"}, {"id": "NUL"}):
            with self.assertRaises(ValueError):
                create_desktop_bug_fix_subgraph(spec, work_dir=self.work, repo_dir=self.repo, test_cmd=TEST)
        with self.assertRaisesRegex(ValueError, "Duplicate or reserved"):
            create_bug_batch_graph([{"id": "App-42"}, {"id": "app-42"}],
                                   work_dir=self.work, repo_dir=self.repo, test_cmd=TEST)
        node = create_desktop_bug_fix_subgraph(
            {"id": "app-42"}, work_dir=self.work, repo_dir=self.repo, test_cmd=TEST,
            node_id="fix_settings", deps=("ready",), resource="desktop",
        )
        self.assertEqual((node.id, node.deps, node.resource), ("fix_settings", ("ready",), "desktop"))
        self.assertEqual([child.id for child in node.nodes],
                         ["triage_and_plan", "apply_fix", "build_check", "run_tests", "verify_fix"])


if __name__ == "__main__":
    unittest.main()
