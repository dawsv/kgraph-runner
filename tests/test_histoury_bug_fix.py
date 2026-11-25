"""Cross-process tests for Histoury release gates and Version History ZIPs."""

import hashlib
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import textwrap
import unittest
from zipfile import ZipFile

from kgraph_runner import (
    build_town_snapshot,
    create_histoury_bug_fix_graph,
    create_version_history_subgraph,
    submit_gate_response,
)
from kgraph_runner.subgraphs import find_version_history_builder

from _support import read_json, run_workflow, write_json, write_png


BATCH_ID = "desktop-42"
BUG_ID = "app-42"
BUG = {
    "id": BUG_ID, "title": "Restore desktop settings", "description": "Settings reopen with the wrong value.",
    "platform": "macOS", "repro_steps": ["Open settings, save, and reopen."], "expected": "The saved value loads.",
}
APPROVAL = {"approved": True, "approved_by": "Daniel", "approve_upload_zip": True, "approve_commit_main": True}
SCRIPT = find_version_history_builder()


def screenshot(path, color):
    write_png(path, 4, 4, 2, lambda _x, _y: (color, 55, 123))


class HistouryWorkflowTest(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(prefix="kgraph histoury workflow ")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir()
        self.repo = self.root / "repo with spaces"
        self.repo.mkdir()
        self.work = self.root / ".kgraph" / "work"
        self.release = self.work / "version_history"
        self.pkg = self.release / "package"
        self.review = self.work / "review"
        self.before = self.work / "media" / f"{BUG_ID}-before.png"
        self.after = self.work / "media" / f"{BUG_ID}-after.png"
        self.before_provenance = self.work / "evidence" / "before_provenance.json"
        self.after_provenance = self.work / "evidence" / "after_provenance.json"
        self.approval = self.review / "approval.json"
        self.upload = self.review / "upload_receipt.json"
        self.commit = self.review / "commit_receipt.json"

    def run_graph(self, *, stage=None, error=None, max_worker_calls=None):
        status = "awaiting_subagents" if stage else "passed"
        report = run_workflow(self, self.root, status=status, max_worker_calls=max_worker_calls,
                              builder_home=self.home)
        if error is not None:
            self.assertIn(error, report["error"])
        if stage:
            self.assertEqual(report["status"], "awaiting_subagents")
            packet = read_json(Path(report["packet_path"]))
            self.assertEqual(packet["stage"], stage)
            return packet
        self.assertEqual(report["status"], "passed")
        return report

    def package_workflow(self, builder=None):
        builder_arg = f"Path({str(builder)!r})" if builder is not None else "None"
        (self.root / "workflow.py").write_text(textwrap.dedent(f"""\
            from pathlib import Path
            from kgraph_runner import create_version_history_subgraph

            def build_graph():
                return [create_version_history_subgraph(
                    work_dir=Path({str(self.work)!r}), release_id={BATCH_ID!r}, builder_script={builder_arg},
                )]
        """), encoding="utf-8")

    def author_release(self, revision, *, duplicate=False, overrides=None):
        manifest = {
            "schema_version": 2, "product": "desktop", "release_id": BATCH_ID,
            "version": "1.0.42", "title": "Desktop maintenance",
            "date": "2026-11-23", "stage": "source", "summary": "Fixed saved settings.",
            "changes": [{
                "id": BUG_ID, "title": "Restore desktop settings", "type": "fix",
                "description": "Reopening settings uses the saved value.",
                "verification": "The local regression test passed on macOS.",
                "evidence": [
                    {"label": stage.capitalize(), "kind": "image", "stage": stage, "url": f"media/{stage}.png"}
                    for stage in ("before", "after")
                ],
            }],
            **(overrides or {}),
        }
        write_json(self.pkg / "release.json", manifest)
        write_json(self.pkg / "author_meta.json", {"revision": revision, "release_id": BATCH_ID})
        if self.before.exists() and self.after.exists():
            (self.pkg / "media").mkdir(exist_ok=True)
            (self.pkg / "media/before.png").write_bytes(self.before.read_bytes())
            (self.pkg / "media/after.png").write_bytes(self.after.read_bytes())
        else:
            screenshot(self.pkg / "media/before.png", 100)
            screenshot(self.pkg / "media/after.png", 100 if duplicate else 200)
        (self.pkg / "unreferenced.txt").write_text("Keep this out of the ZIP.", encoding="utf-8")

    def assert_zip(self):
        manifest = read_json(self.release / "package_manifest.json")
        zip_path = self.release / f"{BATCH_ID}.zip"
        self.assertEqual(manifest, {
            "release_id": BATCH_ID, "product": "desktop", "zip_path": str(zip_path),
            "zip_sha256": hashlib.sha256(zip_path.read_bytes()).hexdigest(), "change_ids": [BUG_ID],
            "media_files": ["media/after.png", "media/before.png"],
        })
        with ZipFile(zip_path) as archive:
            self.assertEqual(sorted(archive.namelist()), ["media/after.png", "media/before.png", "release.json"])
            for name in archive.namelist():
                self.assertEqual(archive.read(name), (self.pkg / name).read_bytes())
        return manifest

    def test_fallback_checks_duplicates_and_supports_repair_and_rebuild(self):
        self.package_workflow()
        packet = self.run_graph(stage="author_release")
        self.assertIsNone(packet["builder_script"])
        self.assertEqual(packet["revision"], 0)
        self.author_release(packet["revision"], duplicate=True)
        packet = self.run_graph(stage="author_release")
        self.assertEqual(packet["revision"], 1)
        feedback = packet["repair_feedback"][-1]
        self.assertEqual(feedback["error_signature"], "duplicate_before_after_media")
        self.assertEqual(feedback["detail"], (self.release / "check.log").read_text(encoding="utf-8"))
        self.assertFalse((self.release / "package_manifest.json").exists())

        screenshot(self.pkg / "media/after.png", 200)
        self.run_graph(stage="author_release", error="revision 1")
        write_json(self.pkg / "author_meta.json", {"revision": 1, "release_id": BATCH_ID})
        (self.release / f"{BATCH_ID}.zip").write_bytes(b"stale ZIP")
        self.run_graph()
        self.assert_zip()
        cached = self.run_graph(max_worker_calls=0)
        self.assertEqual(len(cached["cached_nodes"]), 3)
        screenshot(self.pkg / "media/after.png", 42)
        self.run_graph()
        self.assert_zip()

    @unittest.skipUnless(SCRIPT, "Portyl skill builder is unavailable")
    def test_real_builder_runs_check_and_build_and_reports_schema_errors(self):
        self.package_workflow(builder=SCRIPT)
        packet = self.run_graph(stage="author_release")
        self.assertEqual(Path(packet["builder_script"]), SCRIPT)
        self.author_release(packet["revision"], overrides={"platforms": ["macOS"]})
        packet = self.run_graph(stage="author_release")
        self.assertRegex(packet["repair_feedback"][-1]["error_signature"], r"^vh_check_failed:[0-9a-f]{8}$")
        self.assertIn("unsupported fields", packet["repair_feedback"][-1]["detail"])
        self.author_release(packet["revision"])
        (self.release / f"{BATCH_ID}.zip").write_bytes(b"stale ZIP")
        self.run_graph()
        self.assert_zip()
        self.assertIn("Validated desktop", (self.release / "check.log").read_text(encoding="utf-8"))
        self.assertIn("Validated desktop", (self.release / "build.log").read_text(encoding="utf-8"))

    def histoury_workflow(self, *, direct=False, requires_ui_capture=True, other_bug=None, max_review_retries=3):
        bug = {**BUG, "requires_ui_capture": requires_ui_capture}
        bugs = [bug, other_bug] if other_bug else [bug]
        test = [sys.executable, "-c", "from pathlib import Path; assert Path('settings').read_text().strip() == 'correct'"]
        (self.root / "workflow.py").write_text(textwrap.dedent(f"""\
            from pathlib import Path
            from kgraph_runner import create_histoury_bug_fix_graph

            def build_graph():
                return create_histoury_bug_fix_graph(
                    work_dir=Path({str(self.work)!r}), batch_id={BATCH_ID!r},
                    repos={{"client": Path({str(self.repo)!r})}}, bugs={bugs if direct else None!r},
                    test_cmds={{"client": {test!r}}}, watch_globs=("settings",),
                    max_review_retries={max_review_retries},
                )
        """), encoding="utf-8")

    def capture(self, stage, packet, *, color):
        path, provenance = (self.before, self.before_provenance) if stage == "before" else (self.after, self.after_provenance)
        rev_key = "source_revision" if stage == "before" else "verified_revision"
        self.assertEqual(set(map(Path, packet["outputs"])), {path, provenance})
        screenshot(path, color)
        write_json(provenance, {"revision": packet["revision"], "captures": {BUG_ID: {
            "path": str(path), rev_key: f"{stage}-head", "platform": "macOS",
            "captured_at": "2026-11-23T08:00:00Z", "screen_state": "Settings reopened after saving.",
        }}})

    def fix(self, packet, *, settings):
        (self.repo / "settings").write_text(settings, encoding="utf-8")
        write_json(Path(packet["outputs"][0]), {
            "revision": packet["revision"], "root_cause": "The saved settings were overwritten by defaults.",
            "summary": "Load saved settings when reopening the panel.", "changed_files": ["settings"],
        })

    def reach_user_gate(self, *, direct=False, exercise_repairs=False, other_bug=None, max_review_retries=3):
        self.histoury_workflow(direct=direct, other_bug=other_bug, max_review_retries=max_review_retries)
        if not direct:
            packet = self.run_graph(stage="fetch_bug_reports")
            self.assertEqual(packet["bug_reports_url"], "https://admin.histoury.com/bug-reports")
            self.assertEqual(packet["platforms"], ["WebGL", "Windows", "macOS"])
            write_json(self.work / "bug_batch.json", {"batch_id": BATCH_ID, "product": "desktop", "bugs": [BUG]})
        packet = self.run_graph(stage="capture_baseline")
        fix_packet = self.work / "bugs" / BUG_ID / "fix_packet.json"
        self.assertFalse(fix_packet.exists())
        screenshot(self.before, 100)
        self.run_graph(stage="capture_baseline", error="before_provenance.json")
        self.assertFalse(fix_packet.exists())
        self.capture("before", packet, color=100)

        packet = self.run_graph(stage="apply_fix")
        self.assertEqual(packet["bug_spec"]["repo"], "client")
        if exercise_repairs:
            self.fix(packet, settings="broken")
            packet = self.run_graph(stage="apply_fix")
            self.assertEqual(packet["revision"], 1)
            self.assertRegex(packet["repair_feedback"][-1]["error_signature"], r"^test_exit_1:")
        self.fix(packet, settings="correct")
        if other_bug:
            packet = self.run_graph(stage="apply_fix")
            self.assertEqual(packet["bug_id"], other_bug["id"])
            self.fix(packet, settings="correct")

        packet = self.run_graph(stage="capture_verified_after")
        if exercise_repairs:
            self.capture("after", packet, color=100)
            packet = self.run_graph(stage="capture_verified_after")
            self.assertEqual(packet["revision"], 1)
            self.assertEqual(packet["repair_feedback"][-1]["error_signature"], "after_matches_before")
        self.capture("after", packet, color=200)

        packet = self.run_graph(stage="author_release")
        self.assertEqual(packet["release_id"], BATCH_ID)
        self.assertEqual(Path(packet["changes_manifest_path"]), self.work / "bug_batch.json")
        self.author_release(packet["revision"])
        return self.run_graph(stage="user_verify_gate")

    def assert_release_has_not_started(self):
        state = read_json(self.root / ".kgraph/workflow_state.json")["nodes"]
        self.assertEqual(state["fix_and_review_loop/user_verify_gate"]["kind"], "gate")
        for stage in ("upload_version_history", "commit_to_main"):
            self.assertNotIn(f"release/{stage}", state)
        self.assertFalse((self.review / "upload_packet.json").exists())
        self.assertFalse((self.review / "commit_packet.json").exists())

    def test_histoury_batch_repairs_and_waits_for_user_before_release(self):
        approval_packet = self.reach_user_gate(exercise_repairs=True)
        self.assert_release_has_not_started()
        summary_path = Path(approval_packet["review_summary"])
        markdown = summary_path.read_text(encoding="utf-8")
        package = self.assert_zip()
        for text in (BUG_ID, "Verified on attempt 2", str(self.before), str(self.after),
                     package["zip_sha256"], str(self.repo), "regression checks passed"):
            self.assertIn(text, markdown)
        summary = read_json(self.review / "review_summary.json")
        self.assertEqual(summary["package"], package)
        self.assertEqual(summary["touched_repos"], {"client": str(self.repo)})
        self.assertEqual(summary["before_provenance"], read_json(self.before_provenance))
        self.assertEqual(summary["after_provenance"], read_json(self.after_provenance))

        self.approval.write_text("{", encoding="utf-8")
        self.run_graph(stage="user_verify_gate", error="invalid_approval")
        write_json(self.approval, {**APPROVAL, "approve_commit_main": "true"})
        self.run_graph(stage="user_verify_gate", error="invalid_approval")
        self.assert_release_has_not_started()

        write_json(self.approval, APPROVAL)
        packet = self.run_graph(stage="upload_version_history")
        self.assertEqual(packet["zip_sha256"], package["zip_sha256"])
        self.assertEqual(packet["version_history_url"], "https://admin.histoury.com/version-history")
        receipt = {"previewed": True, "published": True, "release_id": BATCH_ID}
        write_json(self.upload, {**receipt, "uploader_name": " User "})
        self.run_graph(stage="upload_version_history", error="invalid_uploader_attribution")
        self.assertFalse((self.review / "commit_packet.json").exists())
        write_json(self.upload, {**receipt, "uploader_name": "Daniel"})

        packet = self.run_graph(stage="commit_to_main")
        self.assertEqual(packet["touched_repos"], {"client": str(self.repo)})
        self.assertEqual(packet["branch"], "main")
        commit = {"repo": "client", "branch": "main", "sha": "abc123"}
        write_json(self.commit, {"committed": True, "commits": [{**commit, "branch": "feature"}]})
        self.run_graph(stage="commit_to_main", error="commit_incomplete")
        write_json(self.commit, {"committed": True, "commits": [commit]})
        self.run_graph()
        self.run_graph(max_worker_calls=0)

    def test_town_rejection_refixes_only_the_checked_bug(self):
        other = {**BUG, "id": "app-43", "title": "Preserve the selected profile", "requires_ui_capture": False}
        packet = self.reach_user_gate(direct=True, other_bug=other)
        state = self.root / ".kgraph/workflow_state.json"
        report = state.with_name("workflow_report.json")
        snapshot = build_town_snapshot(state_path=state, report_path=report)
        gate = snapshot["attention"]["input_gate"]
        self.assertTrue(gate["can_respond"])
        self.assertEqual(gate["bugs"], [{"id": bug["id"], "title": bug["title"]} for bug in (BUG, other)])
        status, result = submit_gate_response(
            {"state_path": str(state), "report_path": str(report)},
            {"node_path": gate["node_path"], "packet_hash": gate["packet_hash"],
             "approved": False, "rejected_bugs": [other["id"]]},
        )
        self.assertEqual(status, 200, result)
        decision = read_json(self.approval)
        self.assertEqual(decision["rejected_by"], "Reviewer")
        self.assertEqual(decision["review_sha256"], packet["review_sha256"])
        retry = self.run_graph(stage="apply_fix", error="revision 1")
        self.assertEqual(retry["bug_id"], other["id"])
        self.assertEqual(read_json(self.review / "review_feedback.json")["rejected_bugs"], [other["id"]])
        self.assert_release_has_not_started()

    def test_review_rejection_refixes_only_selected_bugs_and_requires_fresh_approval(self):
        other = {**BUG, "id": "app-43", "requires_ui_capture": False}
        first = self.reach_user_gate(direct=True, other_bug=other, max_review_retries=1)
        before = self.before_provenance.read_bytes()
        unaffected = self.work / "bugs" / other["id"] / "fix_report.json"
        unaffected_hash = unaffected.read_bytes()
        qa_path = self.review / "manual_qa.json"
        initial = read_json(qa_path)
        self.assertEqual(first["manual_qa_checklist"], str(qa_path))
        self.assertEqual([(case["id"], case["ai_attempts"], case["automated_status"]) for case in initial["cases"]],
                         [(BUG_ID, 1, "pass"), (other["id"], 1, "pass")])
        initial["cases"][1]["manual_qa"] = "pass"
        initial["cases"][1]["notes"] = "Verified separately on the review build."
        write_json(qa_path, initial)
        self.run_graph(stage="user_verify_gate")
        self.assertEqual(read_json(self.review / "qa_scorecard.json")["counts"]["passed"], 1)
        rejection = {"approved": False, "notes": "Keep the selected profile when the panel reopens.",
                     "rejected_by": "Daniel", "manual_qa": {
                         BUG_ID: {"result": "fail", "gap_type": "missing_verifier", "notes": "Profile reset."},
                     }}
        feedback_path = self.review / "review_feedback.json"

        write_json(self.approval, {**rejection, "rejected_bugs": ["unknown-bug"]})
        self.run_graph(stage="user_verify_gate", error="invalid_approval")
        write_json(self.approval, {**rejection, "review_sha256": "old-review"})
        self.run_graph(stage="user_verify_gate", error="stale_approval")
        self.assertFalse(feedback_path.exists())
        self.assertEqual(read_json(qa_path)["cases"][0]["manual_qa"], "not_tested")

        write_json(self.approval, {**rejection, "review_sha256": first["review_sha256"]})
        rejection_sha = hashlib.sha256(self.approval.read_bytes()).hexdigest()
        packet = self.run_graph(stage="apply_fix", error="revision 1")
        self.assertEqual(packet["bug_id"], BUG_ID)
        check = read_json(self.review / "qa_scorecard.json")
        self.assertEqual([case["id"] for case in check["auto_pass_manual_fail"]], [BUG_ID])
        self.assertEqual(check["gap_breakdown"], {"missing_verifier": 1})
        feedback = read_json(feedback_path)
        self.assertEqual(feedback["round"], 1)
        self.assertEqual(feedback["rejected_bugs"], [BUG_ID])
        self.assertEqual(feedback["rejected_approval_sha256"], rejection_sha)
        self.assertEqual(feedback["fixes"], {BUG_ID: packet["review_feedback"]})
        self.assertEqual(packet["review_feedback"]["fix_revision"], 1)
        self.assertEqual(packet["review_feedback"]["manual_qa"]["notes"], "Profile reset.")
        self.assertEqual(self.run_graph(stage="apply_fix")["review_feedback"], packet["review_feedback"])
        self.assert_release_has_not_started()
        self.fix(packet, settings="correct\n")

        packet = self.run_graph(stage="capture_verified_after", error="source inputs changed")
        self.capture("after", packet, color=175)
        packet = self.run_graph(stage="author_release")
        self.author_release(packet["revision"], overrides={"summary": "Fixed saved settings and selected profile."})
        second = self.run_graph(stage="user_verify_gate", error="stale_approval")
        self.assertNotEqual(first["review_sha256"], second["review_sha256"])
        self.assertEqual(second["stale_approval_sha256"], rejection_sha)
        self.assertEqual(read_json(self.review / "review_summary.json")["review_round"], 2)
        self.assertEqual(self.before_provenance.read_bytes(), before)
        self.assertEqual(unaffected.read_bytes(), unaffected_hash)
        self.assertEqual(read_json(feedback_path)["round"], 1)
        self.assert_release_has_not_started()
        repaired, retained = read_json(qa_path)["cases"]
        self.assertEqual((repaired["ai_attempts"], repaired["manual_qa"]), (2, "not_tested"))
        self.assertEqual((retained["ai_attempts"], retained["manual_qa"], retained["resolved_on_attempt"]), (1, "pass", 1))
        self.assertEqual(retained["notes"], initial["cases"][1]["notes"])

        write_json(self.approval, APPROVAL)
        self.run_graph(stage="user_verify_gate", error="stale_approval")
        write_json(self.approval, {"approved": False, "review_sha256": second["review_sha256"]})
        failed = run_workflow(self, self.root, status="failed", builder_home=self.home)
        self.assertIn("Exhausted 1 review retries (user_rejected)", failed["error"])
        self.assertEqual(read_json(feedback_path)["round"], 1)
        self.assert_release_has_not_started()

        write_json(self.approval, {**APPROVAL, "review_sha256": second["review_sha256"],
                                   "manual_qa": [{"id": BUG_ID, "manual_qa": "pass"}]})
        upload = self.run_graph(stage="upload_version_history")
        card = read_json(self.review / "qa_scorecard.json")
        self.assertEqual((card["rates"]["one_shot_pct"], card["rates"]["within_two_attempts_pct"]), (50.0, 100.0))
        self.assertEqual(upload["zip_sha256"], self.assert_zip()["zip_sha256"])
        write_json(self.upload, {"previewed": True, "published": True, "release_id": BATCH_ID, "uploader_name": "Daniel"})
        self.run_graph(stage="commit_to_main")
        write_json(self.commit, {"committed": True, "commits": [{"repo": "client", "branch": "main", "sha": "abc123"}]})
        self.run_graph()

    def test_review_changes_invalidate_approval_and_selective_approval_skips_commit(self):
        self.reach_user_gate(direct=True)
        write_json(self.approval, {**APPROVAL, "approve_commit_main": False})
        (self.repo / "settings").write_text("correct\n", encoding="utf-8")

        self.run_graph(stage="user_verify_gate", error="stale_approval")
        packet = self.run_graph(stage="user_verify_gate", error="stale_approval")
        self.assert_release_has_not_started()
        write_json(self.approval, {**APPROVAL, "approve_commit_main": False, "review_sha256": packet["review_sha256"]})
        self.run_graph(stage="upload_version_history")
        write_json(self.upload, {"previewed": True, "published": True, "release_id": BATCH_ID, "uploader_name": "Daniel"})
        self.run_graph()
        self.assertEqual(read_json(self.commit), {"skipped": True, "reason": "Not approved for commit to main"})
        self.assertFalse((self.review / "commit_packet.json").exists())

    def test_skipping_both_release_actions_still_requires_human_approval(self):
        self.reach_user_gate(direct=True)
        self.assert_release_has_not_started()
        write_json(self.approval, {**APPROVAL, "approve_upload_zip": False, "approve_commit_main": False})
        self.run_graph()
        self.assertEqual(read_json(self.upload), {"skipped": True, "reason": "Not approved for upload"})
        self.assertTrue(read_json(self.commit)["skipped"])

    def test_mobile_intake_packet_lists_platforms_and_repositories(self):
        (self.root / "workflow.py").write_text(textwrap.dedent(f"""\
            from pathlib import Path
            from kgraph_runner import create_histoury_bug_fix_graph

            def build_graph():
                return create_histoury_bug_fix_graph(
                    batch_id={BATCH_ID!r}, product="app", work_dir=Path({str(self.work)!r}),
                    repos={{"engine": Path({str(self.repo)!r}), "client": Path({str(self.root / "client")!r})}},
                    test_cmds={{"engine": "python -m unittest", "client": "python -m unittest"}},
                    watch_globs=("**/*.py",),
                )
        """), encoding="utf-8")
        packet = self.run_graph(stage="fetch_bug_reports")
        self.assertEqual(packet["product"], "app")
        self.assertEqual(packet["platforms"], ["iOS", "Android"])
        self.assertEqual(packet["repos"], ["engine", "client"])

    def test_validation_bootstraps_safely_and_non_ui_bugs_skip_captures(self):
        for batch_id in ("../bad", "NUL", "Bad"):
            with self.assertRaises(ValueError):
                create_histoury_bug_fix_graph(batch_id=batch_id, work_dir=self.work, repos={"client": self.repo})
        with self.assertRaises(ValueError):
            create_histoury_bug_fix_graph(batch_id=BATCH_ID, work_dir=self.work, repos={})
        with self.assertRaises(ValueError):
            create_version_history_subgraph(work_dir=self.work, product="mobile")
        self.histoury_workflow(direct=True, requires_ui_capture=False)
        packet = self.run_graph(stage="apply_fix")
        self.assertEqual(read_json(self.before_provenance), {"revision": 0, "captures": {}})
        self.fix(packet, settings="correct")
        self.run_graph(stage="author_release")
        self.assertEqual(read_json(self.after_provenance), {"revision": 0, "captures": {}})


if __name__ == "__main__":
    unittest.main()
