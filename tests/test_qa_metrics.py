from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from kgraph_runner import (
    aggregate_qa_history, compute_qa_scorecard, init_qa_checklist, record_qa_result,
    render_qa_scorecard_markdown, write_qa_scorecard,
)


class QAMetricsTest(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(prefix="kgraph qa ")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.checklist = self.root / "manual_qa.json"
        self.history = self.root / "history.jsonl"

    def read(self, path):
        return json.loads(path.read_text(encoding="utf-8"))

    def init(self, cases, **kwargs):
        return init_qa_checklist(self.checklist, batch_id="desktop-42", cases=cases, **kwargs)

    def test_state_paths_aliases_ambiguity_and_missing_state(self):
        state = self.root / "state.json"
        state.write_text(json.dumps({"nodes": {
            "batch/bug-a": {"status": "passed", "attempts": 2},
            "batch/bug-b": {"status": "circuit_broken", "attempts": 3},
            "batch/capture": {"status": "awaiting_subagents", "attempts": 0},
            "other/duplicate": {"status": "passed", "attempts": 1},
            "batch/duplicate": {"status": "failed", "attempts": 4},
            "duplicate": {"status": "cached", "attempts": 1},
            "other/ambiguous": {"status": "passed"},
            "batch/ambiguous": {"status": "failed"},
        }}), encoding="utf-8")
        cases = [{"id": "bug-a"}, {"id": "b", "node_path": "batch/bug-b"},
                 {"id": "screenshot", "node_id": "capture"}, {"id": "duplicate"},
                 {"id": "ambiguous"}, {"id": "unknown", "automated_status": "blocked", "ai_attempts": 4}]
        checklist = self.init(cases, state_path=state)
        self.assertEqual(checklist["version"], 1)
        self.assertEqual(checklist["tester"], "Daniel")
        self.assertTrue(checklist["updated_at"].endswith("+00:00"))
        self.assertEqual([(item["automated_status"], item["ai_attempts"]) for item in checklist["cases"]], [
            ("pass", 2), ("fail", 3), ("blocked", 0), ("pass", 1), ("pending", 0), ("blocked", 4),
        ])
        missing = self.init([{"id": "plain"}], state_path=self.root / "missing.json")
        self.assertEqual(missing["cases"][0], {
            "id": "plain", "title": "plain", "platform": "unspecified", "kind": "custom",
            "automated_status": "pending", "ai_attempts": 0, "manual_qa": "not_tested",
            "resolved_on_attempt": None, "gap_type": None, "notes": None,
        })

    def test_atomic_record_defaults_and_preserves_only_current_results(self):
        a = {"id": "a", "ai_attempts": 1}
        b = {"id": "b", "ai_attempts": 2}
        self.init([a, b], title="Régression", build_ref="build-a")
        record_qa_result(self.checklist, case_id="a", manual_qa="pass", notes="Đã kiểm tra", tester="Hà")
        first = record_qa_result(self.checklist, case_id="b", manual_qa="pass")
        self.assertEqual([case["resolved_on_attempt"] for case in first["cases"]], [1, 2])
        current = self.init([{**a, "automated_status": "pass"}, {**b, "ai_attempts": 3}])
        one, two = current["cases"]
        self.assertEqual((one["manual_qa"], one["resolved_on_attempt"], one["notes"]), ("pass", 1, "Đã kiểm tra"))
        self.assertEqual((one["automated_status"], two["manual_qa"], two["resolved_on_attempt"]), ("pass", "not_tested", None))
        self.assertEqual((current["tester"], current["title"], current["build_ref"]), ("Hà", "Régression", "build-a"))
        result = record_qa_result(self.checklist, case_id="a", manual_qa="fail", resolved_on_attempt=9,
                                  gap_type="missing_verifier", notes="Still fails")
        self.assertIsNone(result["cases"][0]["resolved_on_attempt"])
        self.assertEqual(self.init([a, b])["cases"][0]["manual_qa"], "fail")
        self.assertEqual(self.init([a, b], preserve_existing=False)["cases"][0]["manual_qa"], "not_tested")
        record_qa_result(self.checklist, case_id="b", manual_qa="pass")
        self.assertEqual(self.init([a, b], build_ref="build-b")["cases"][1]["manual_qa"], "not_tested")
        self.init([{"id": "zero"}])
        self.assertEqual(record_qa_result(self.checklist, case_id="zero", manual_qa="pass")["cases"][0]
                         ["resolved_on_attempt"], 1)

    def test_invalid_inputs_leave_existing_checklist_intact(self):
        self.init([{"id": "stable"}])
        saved = self.checklist.read_bytes()
        for kwargs in (
            {"case_id": "unknown"}, {"manual_qa": "done"}, {"gap_type": "wrong"}, {"resolved_on_attempt": 0},
            {"resolved_on_attempt": True}, {"notes": []},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                record_qa_result(self.checklist, **{"case_id": "stable", "manual_qa": "pass", **kwargs})
        for invalid in (
            [{"id": "duplicate"}, {"id": "duplicate"}], [{"id": "x", "ai_attempts": -1}],
            [{"id": "x", "automated_status": "passed"}], [{"title": "ID required"}], ["not a case"],
        ):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                self.init(invalid)
        self.assertEqual(self.checklist.read_bytes(), saved)

    def test_scorecard_uses_only_tested_cases_and_reports_graph_gaps(self):
        cases = [
            {"id": "a", "platform": "Windows", "manual_qa": "pass", "resolved_on_attempt": 1},
            {"id": "b", "platform": "Windows", "manual_qa": "pass", "resolved_on_attempt": 2},
            {"id": "c", "platform": "macOS", "manual_qa": "pass", "resolved_on_attempt": 3},
            {"id": "d", "platform": "macOS", "automated_status": "pass", "manual_qa": "fail",
             "title": "Góc | view", "gap_type": "missing_verifier", "notes": "<b>Broken</b>\nOpen menu"},
            {"id": "e", "automated_status": "fail", "manual_qa": "fail", "resolved_on_attempt": 1},
            {"id": "f", "manual_qa": "blocked", "gap_type": "env_blocker"},
            {"id": "g", "manual_qa": "not_tested"},
            {"id": "h", "manual_qa": "skipped"},
        ]
        scorecard = compute_qa_scorecard(self.init(cases, title="Build QA"))
        self.assertEqual(scorecard["counts"], {"total": 8, "tested": 5, "passed": 3, "failed": 2,
                                                "blocked": 1, "not_tested": 1, "skipped": 1})
        self.assertEqual(scorecard["rates"], {
            "manual_pass_pct": 60.0, "one_shot_passed": 1, "one_shot_pct": 20.0, "second_attempt_passed": 1,
            "within_two_attempts_passed": 2, "within_two_attempts_pct": 40.0,
        })
        self.assertIsNone(scorecard["cases"][4]["resolved_on_attempt"])
        self.assertEqual(scorecard["auto_pass_manual_fail"], [{
            "id": "d", "platform": "macOS", "title": "Góc | view", "gap_type": "missing_verifier",
            "notes": "<b>Broken</b>\nOpen menu",
        }])
        self.assertEqual(scorecard["gap_breakdown"], {"missing_verifier": 1, "env_blocker": 1, "unspecified": 1})
        self.assertEqual(scorecard["by_platform"]["Windows"], {
            "total": 2, "tested": 2, "passed": 2, "failed": 0, "blocked": 0, "not_tested": 0,
            "one_shot_passed": 1, "within_two_attempts_passed": 2,
        })
        markdown = render_qa_scorecard_markdown(scorecard)
        for text in ("60.0%", "20.0%", "40.0%", "Auto-Pass → Manual-Fail", "Góc &#124; view",
                     "&lt;b&gt;Broken&lt;/b&gt; Open menu", "By Platform", "## Cases"):
            self.assertIn(text, markdown)

    def test_empty_scorecards_and_history_have_no_percentages(self):
        for cases in ([], [{"id": "blocked", "manual_qa": "blocked"}, {"id": "p"}, {"id": "s", "manual_qa": "skipped"}]):
            scorecard = compute_qa_scorecard(self.init(cases))
            self.assertIsNone(scorecard["rates"]["manual_pass_pct"])
            self.assertIsNone(scorecard["rates"]["one_shot_pct"])
            self.assertIsNone(scorecard["rates"]["within_two_attempts_pct"])
        history = aggregate_qa_history(self.history)
        self.assertEqual((history["batches"], history["total_cases"], history["recent_batches"]), (0, 0, []))
        self.assertIsNone(history["manual_pass_pct"])

    def test_history_upserts_and_uses_weighted_rates(self):
        one = {"id": "a", "automated_status": "pass"}
        self.init([one])
        record_qa_result(self.checklist, case_id="a", manual_qa="fail", gap_type="logic_bug")
        write_qa_scorecard(self.checklist, record_history=True, history_path=self.history)
        self.assertEqual(aggregate_qa_history(self.history)["auto_pass_manual_fail_count"], 1)
        record_qa_result(self.checklist, case_id="a", manual_qa="pass", resolved_on_attempt=2)
        output = write_qa_scorecard(self.checklist, record_history=True, history_path=self.history)
        self.assertEqual(self.read(self.root / "qa_scorecard.json"), output)
        self.assertEqual((self.root / "qa_scorecard.md").read_text(encoding="utf-8"),
                         render_qa_scorecard_markdown(output))
        second = self.root / "second.json"
        init_qa_checklist(second, batch_id="other", cases=[
            {"id": "x", "manual_qa": "pass"},
            {"id": "y", "manual_qa": "fail", "automated_status": "pass", "gap_type": "ui_regression"},
            {"id": "z", "manual_qa": "fail"}, {"id": "blocked", "manual_qa": "blocked", "gap_type": "env_blocker"},
        ])
        write_qa_scorecard(second, record_history=True, history_path=self.history)
        self.assertEqual(len(self.history.read_text(encoding="utf-8").splitlines()), 2)
        totals = aggregate_qa_history(self.history)
        self.assertEqual((totals["batches"], totals["total_cases"], totals["tested"], totals["blocked"]), (2, 5, 4, 1))
        self.assertEqual((totals["manual_pass_pct"], totals["one_shot_pct"], totals["within_two_attempts_pct"]),
                         (50.0, 25.0, 50.0))
        self.assertEqual(totals["auto_pass_manual_fail_count"], 1)
        self.assertEqual(totals["gap_breakdown"], {"ui_regression": 1, "unspecified": 1, "env_blocker": 1})
        self.assertEqual([batch["batch_id"] for batch in totals["recent_batches"]], ["other", "desktop-42"])
        with self.assertRaisesRegex(ValueError, "separate"):
            write_qa_scorecard(self.checklist, json_path=self.checklist)

    def test_parallel_record_and_history_writes_keep_all_updates(self):
        case_ids = [f"case-{number}" for number in range(8)]
        self.init([{"id": case_id} for case_id in case_ids])

        def update(case_id):
            record_qa_result(self.checklist, case_id=case_id, manual_qa="pass")
            other = self.root / case_id / "manual_qa.json"
            init_qa_checklist(other, batch_id=case_id, cases=[{"id": case_id}])
            write_qa_scorecard(other, record_history=True, history_path=self.history)

        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(update, case_ids))
        self.assertEqual(compute_qa_scorecard(self.read(self.checklist))["counts"]["passed"], len(case_ids))
        self.assertEqual(aggregate_qa_history(self.history)["batches"], len(case_ids))

    def test_public_qa_workflow_with_custom_and_default_history(self):
        init_qa_checklist(
            self.checklist, batch_id="acceptance", workflow_type="integration", build_ref="build-123",
            cases=[{"id": "menu", "title": "Mở menu", "ai_attempts": 2}, {"id": "gameplay"}],
        )
        record_qa_result(self.checklist, case_id="menu", manual_qa="pass", tester="Qá", notes="Đã kiểm tra")
        with patch.dict(os.environ, {"KGRAPH_HOME": str(self.root / "history")}):
            write_qa_scorecard(self.checklist, record_history=True)
            markdown = (self.root / "qa_scorecard.md").read_text(encoding="utf-8")
            self.assertIn("≤2-Shot %", markdown)
            self.assertIn("Đã kiểm tra", markdown)
            record_qa_result(self.checklist, case_id="gameplay", manual_qa="blocked", gap_type="env_blocker")
            write_qa_scorecard(self.checklist, record_history=True)
            self.assertEqual(self.read(self.checklist)["cases"][0]["resolved_on_attempt"], 2)
            output = self.root / "publish"
            scorecard = write_qa_scorecard(self.checklist, json_path=output / "result.json",
                                           md_path=output / "result.md", record_history=True, history_path=self.history)
            self.assertEqual(scorecard, self.read(output / "result.json"))
            self.assertIn("QA Scorecard", (output / "result.md").read_text(encoding="utf-8"))
            for history in (aggregate_qa_history(), aggregate_qa_history(self.history)):
                self.assertEqual((history["batches"], history["tested"], history["blocked"]), (1, 1, 1))
                self.assertEqual(history["within_two_attempts_pct"], 100.0)
        with self.assertRaisesRegex(ValueError, "Unknown QA case"):
            record_qa_result(self.checklist, case_id="bad", manual_qa="pass")


if __name__ == "__main__":
    unittest.main()
