from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

from kgraph_runner import (
    create_game_regression_graph, create_game_regression_subgraph, record_qa_result, verify_png_file,
)

from _support import read_json, run_workflow, write_png


BUILD = (sys.executable, "-c", "print('build passed')")
TEST = (sys.executable, "-c", "print('regression passed')")
FAIL = (sys.executable, "-c", "import sys; print('command failed', file=sys.stderr); sys.exit(7)")
SUITE = "game-smoke"


def screenshot(path, width=24, height=18):
    write_png(path, width, height, 2, lambda x, y: (x, y, 32), filters=(0, 1, 2, 3, 4))


class RegressionSubgraphTest(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(prefix="kgraph regression ")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.repo = self.root / "game repo"
        self.repo.mkdir()
        self.work = self.repo / "regression outputs"
        self.source = self.repo / "game.py"
        self.source.write_text("lives = 3\n", encoding="utf-8")
        self.json_report = self.work / "regression_report.json"
        self.markdown_report = self.work / "regression_report.md"

    def workflow(self, **overrides):
        options = dict(work_dir=str(self.work), repo_dir=str(self.repo), test_cmd=TEST)
        options.update(overrides)
        (self.root / "workflow.py").write_text(
            "from kgraph_runner import create_game_regression_graph\n\n"
            f"def build_graph():\n    return create_game_regression_graph({SUITE!r}, **{options!r})\n",
            encoding="utf-8",
        )

    def run_workflow(self, **options):
        return run_workflow(self, self.root, max_worker_calls=0, **options)

    def test_graph_topology_and_validation(self):
        node = create_game_regression_subgraph(SUITE, work_dir=self.work, test_cmd=TEST, deps=("setup",))
        self.assertEqual((node.id, node.deps), (SUITE, ("setup",)))
        self.assertEqual([(child.id, child.deps) for child in node.nodes], [
            ("preflight", ()), ("run_regression", ("preflight",)), ("regression_report", ("run_regression",)),
        ])
        [suite] = create_game_regression_graph(
            SUITE, work_dir=self.work, test_cmd=TEST, build_cmd=BUILD, visual_checks=("menu",),
        )
        self.assertEqual([(child.id, child.deps) for child in suite.nodes], [
            ("preflight", ()), ("compile_or_build", ("preflight",)), ("run_regression", ("compile_or_build",)),
            ("capture_visual_evidence", ("run_regression",)), ("regression_report", ("capture_visual_evidence",)),
        ])
        with self.assertRaisesRegex(ValueError, "Invalid ID"):
            create_game_regression_subgraph("../smoke", work_dir=self.work, test_cmd=TEST)
        with self.assertRaisesRegex(ValueError, "test_cmd"):
            create_game_regression_subgraph(SUITE, work_dir=self.work, test_cmd=" ")
        with self.assertRaisesRegex(ValueError, "positive integer"):
            create_game_regression_subgraph(SUITE, work_dir=self.work, test_cmd=TEST, min_height=0)

    def test_pass_cache_and_source_changes_without_watch_globs(self):
        self.workflow(build_cmd=BUILD)
        report = self.run_workflow()
        preflight = read_json(self.work / "preflight.json")
        summary = read_json(self.json_report)
        self.assertEqual(report["status"], "passed")
        self.assertEqual(summary["status"], "passed")
        self.assertEqual(summary["preflight"], preflight)
        self.assertEqual(summary["logs"]["build"]["tail"], "build passed")
        self.assertEqual(summary["logs"]["regression"]["tail"], "regression passed")
        self.assertEqual(summary["visual_checks"], [])
        self.assertIn("regression passed", self.markdown_report.read_text(encoding="utf-8"))
        qa_path = self.work / "manual_qa.json"
        qa = read_json(qa_path)
        self.assertTrue(qa["build_ref"])
        self.assertEqual(qa["workflow_type"], "game-regression")
        record_qa_result(qa_path, case_id=SUITE, manual_qa="pass")

        warm = self.run_workflow()
        self.assertEqual(warm["cached_nodes"], [f"{SUITE}/{stage}" for stage in (
            "preflight", "compile_or_build", "run_regression", "regression_report",
        )])
        self.source.write_text("lives = 5\n", encoding="utf-8")
        changed = self.run_workflow()
        self.assertEqual(changed["cached_nodes"], [])
        refreshed = read_json(self.work / "preflight.json")
        self.assertNotEqual(refreshed["source_hash"], preflight["source_hash"])
        self.assertEqual(refreshed["config_hash"], preflight["config_hash"])
        self.assertEqual(read_json(qa_path)["cases"][0]["manual_qa"], "not_tested")

    def test_watch_globs_config_changes_and_missing_report(self):
        self.workflow(watch_globs=("*.py",))
        self.run_workflow()
        preflight = read_json(self.work / "preflight.json")
        (self.repo / "notes.txt").write_text("ignored", encoding="utf-8")
        self.markdown_report.unlink()
        report = self.run_workflow()
        self.assertEqual(report["cached_nodes"], [f"{SUITE}/preflight", f"{SUITE}/run_regression"])
        self.assertTrue(self.markdown_report.is_file())
        self.assertFalse((self.work / "build.log").exists())
        self.assertFalse((self.work / "visual_packet.json").exists())
        self.workflow(watch_globs=("*.py",), timeout_s=10)
        self.assertEqual(self.run_workflow()["cached_nodes"], [])
        updated = read_json(self.work / "preflight.json")
        self.assertEqual(updated["source_hash"], preflight["source_hash"])
        self.assertNotEqual(updated["config_hash"], preflight["config_hash"])

    def test_build_failure_stops_regression(self):
        self.workflow(build_cmd=FAIL, visual_checks=("menu",))
        report = self.run_workflow(status="failed")
        self.assertEqual(report["status"], "failed")
        self.assertIn(f"{SUITE}/compile_or_build", report["error"])
        self.assertIn("command failed", (self.work / "build.log").read_text(encoding="utf-8"))
        for name in ("regression.log", "visual_packet.json", "regression_report.json", "regression_report.md"):
            self.assertFalse((self.work / name).exists(), name)

    def test_regression_failure_clears_prior_success_and_skips_visual_gate(self):
        code = "from pathlib import Path; assert Path('game.py').read_text().strip() == 'lives = 3'"
        self.workflow(build_cmd=BUILD, test_cmd=(sys.executable, "-c", code), visual_checks=("menu",))
        screenshot(self.work / "visual" / "menu.png")
        self.run_workflow()
        self.assertTrue(self.json_report.is_file())
        self.source.write_text("lives = 0\n", encoding="utf-8")
        report = self.run_workflow(status="failed")
        self.assertIn(f"{SUITE}/run_regression", report["error"])
        self.assertIn("AssertionError", (self.work / "regression.log").read_text(encoding="utf-8"))
        self.assertFalse((self.work / "visual_packet.json").exists())
        self.assertFalse(self.json_report.exists())
        self.assertFalse(self.markdown_report.exists())

    def test_visual_gate_pauses_for_missing_invalid_blank_and_undersized_pngs(self):
        self.workflow(visual_checks=("menu", {"id": "gameplay", "path": "captures/level.png", "min_width": 24}))
        report = self.run_workflow(status="awaiting_subagents")
        self.assertEqual(report["status"], "awaiting_subagents")
        packet_path = self.work / "visual_packet.json"
        self.assertEqual(report["packet_path"], str(packet_path))
        packet = read_json(packet_path)
        self.assertEqual(packet["stage"], "capture_visual_evidence")
        menu, gameplay = (Path(packet["outputs"][name]) for name in ("menu", "gameplay"))
        self.assertEqual(menu, self.work / "visual" / "menu.png")
        self.assertEqual(gameplay, self.work / "captures" / "level.png")
        self.assertEqual(packet["visual_checks"][1]["min_width"], 24)

        screenshot(menu)
        screenshot(gameplay)
        damaged = bytearray(gameplay.read_bytes())
        damaged[-1] ^= 1
        gameplay.write_bytes(damaged)
        self.assertIn("Bad CRC", self.run_workflow(status="awaiting_subagents")["error"])
        write_png(gameplay, 24, 18, 2, lambda _x, _y: (3, 5, 7), filters=(0, 1, 2, 3, 4))
        self.assertIn("solid color", self.run_workflow(status="awaiting_subagents")["error"])
        screenshot(gameplay, width=23)
        self.assertIn("minimum 24×16px", self.run_workflow(status="awaiting_subagents")["error"])
        self.assertFalse(self.json_report.exists())

        screenshot(gameplay)
        report = self.run_workflow()
        self.assertEqual(report["cached_nodes"], [f"{SUITE}/preflight", f"{SUITE}/run_regression"])
        evidence = read_json(self.json_report)["visual_checks"]
        self.assertEqual([(check["id"], check["ok"]) for check in evidence], [("menu", True), ("gameplay", True)])
        score = read_json(self.work / "qa_scorecard.json")
        self.assertEqual([(case["id"], case["kind"], case["automated_status"]) for case in score["cases"]],
                         [(SUITE, "regression-suite", "pass"), ("menu", "visual", "pass"), ("gameplay", "visual", "pass")])
        self.assertIn("Visual check: menu", (self.work / "qa_scorecard.md").read_text(encoding="utf-8"))
        self.assertIn("24×18px", self.markdown_report.read_text(encoding="utf-8"))
        self.assertIn(f"{SUITE}/capture_visual_evidence", self.run_workflow()["cached_nodes"])

        menu.unlink()
        self.assertEqual(self.run_workflow(status="awaiting_subagents")["cached_nodes"], report["cached_nodes"])
        self.assertFalse(self.json_report.exists())
        self.assertFalse(self.markdown_report.exists())

    def test_transparent_rgba_noise_and_duplicate_palette_colors_are_blank(self):
        path = self.work / "screenshot.png"
        write_png(path, 16, 16, 6, lambda x, y: (x, y, 20, 0))
        self.assertEqual(verify_png_file(path)[1], "blank_png")
        write_png(path, 16, 16, 6, lambda x, y: (x, y, 20, 0 if x == 0 else 255))
        self.assertTrue(verify_png_file(path)[0])
        write_png(path, 16, 16, 3, lambda x, _y: (x % 2,), palette=bytes([3, 5, 7] * 2))
        self.assertEqual(verify_png_file(path)[1], "blank_png")
        write_png(path, 16, 16, 3, lambda x, _y: (x % 2,), palette=bytes([3, 5, 7, 9, 5, 7]))
        self.assertTrue(verify_png_file(path)[0])

    def test_multiple_visual_checks_with_shell_commands_and_watch_globs(self):
        self.workflow(build_cmd="echo build passed", test_cmd="echo regression passed",
                      watch_globs=("*.py", "assets/**/*"), visual_checks=("menu", "gameplay"))
        report = self.run_workflow(status="awaiting_subagents")
        packet_path = Path(report["packet_path"])
        self.assertEqual(packet_path, self.work / "visual_packet.json")
        packet = read_json(packet_path)
        self.assertEqual(set(packet["outputs"]), {"menu", "gameplay"})
        for path in packet["outputs"].values():
            screenshot(Path(path))
        self.run_workflow()
        summary = read_json(self.json_report)
        self.assertEqual(summary["preflight"]["watch_globs"], ["*.py", "assets/**/*"])
        self.assertEqual(summary["preflight"]["repo_dir"], str(self.repo))


if __name__ == "__main__":
    unittest.main()
