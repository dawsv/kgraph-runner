from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stdout
from io import StringIO
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
import threading
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from kgraph_runner import (
    ask_node_question, build_town_snapshot, read_node_progress, read_subtask_answer, render_town_html,
    update_node_progress,
)
from kgraph_runner.cli import main
from kgraph_runner.interactions import (
    build_node_sub_graph, extract_input_gate, progress_path_for_state, read_remote_response,
    submit_gate_response, validate_kgraph_output_path,
)
from kgraph_runner.registry import ingest_remote_run, list_runs, register_run
from kgraph_runner.server import create_hub_server, create_town_server


WORKFLOW = """
from pathlib import Path
import json
from kgraph_runner import SubgraphNode, subagent_gate

root = Path(__file__).parent / ".kgraph" / "mission"
output = root / "answer.json"

def build_graph():
    gate = subagent_gate(
        "confirm", packet_path=root / "packet.json", artifact_paths=(output,),
        packet_fn=lambda ctx: {
            "stage": "confirm",
            "question": "Continue with the pilot?",
            "recommended": "Proceed with the pilot",
            "output_path": "answer.json",
        },
        input_hash_fn=lambda ctx: "pilot",
        verify_fn=lambda ctx, paths: (json.loads(output.read_text())["resolved"], "", "Decided"),
    )
    return [SubgraphNode(id="release", nodes=[gate])]
"""


class InteractionsTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.project = Path(tmp.name).resolve()
        self.home = self.project / "hub-home"
        home = patch.dict(os.environ, {"KGRAPH_HOME": str(self.home)})
        home.start()
        self.addCleanup(home.stop)
        self.state = self.project / ".kgraph" / "mission" / "state.json"
        self.report = self.state.with_name("report.json")
        self.packet = self.state.with_name("packet.json")
        self.output = self.state.with_name("answer.json")
        self.node_path = "release/confirm"
        self.run_entry = {"id": "test", "state_path": str(self.state), "report_path": str(self.report),
                          "project_dir": str(self.project)}

    @staticmethod
    def write(path, doc):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(doc), encoding="utf-8")
        return path

    def paused(self, packet, *, node_path=None):
        if node_path is not None:
            self.node_path = node_path
        self.write(self.state, {"nodes": {self.node_path: {"kind": "gate", "status": "awaiting_subagents"}}})
        self.write(self.report, {"status": "awaiting_subagents", "error": f"[{self.node_path}] Waiting",
                                 "packet_path": str(self.packet)})
        self.write(self.packet, packet)

    def extract(self, packet):
        return extract_input_gate(packet, self.packet, node_path=self.node_path,
                                  state_path=self.state, project_dir=self.project)

    def snapshot(self):
        return build_town_snapshot(state_path=self.state, report_path=self.report)

    def submit(self, **payload):
        return submit_gate_response(self.run_entry, {"node_path": self.node_path, **payload})

    def serve(self, server):
        thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .025}, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join, 2)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_port}"

    @staticmethod
    def request(base, path, method="GET", body=None, *, authorized=True, headers=None):
        request_headers = {"Authorization": "Bearer test-token"} if authorized else {}
        if headers:
            request_headers.update(headers)
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body).encode()
        request = Request(base + path, data=body, headers=request_headers, method=method)
        try:
            response = urlopen(request, timeout=5)
        except HTTPError as exc:
            response = exc
        with response:
            return response.status, json.loads(response.read())

    def test_progress_merge_order_waves_and_redaction(self):
        update_node_progress(self.state, self.node_path, item_id="z-build", status="running",
                             kind="worker", agent="Builder", progress=15, detail='Check password="very-secret multi-word fragment"')
        update_node_progress(self.state, self.node_path, item_id="a-smoke", status="running",
                             kind="verifier", deps=["z-build"], outputs=["smoke.json"], progress=30.25)
        update_node_progress(self.state, self.node_path, item_id="z-build", status="cached", progress=None)
        data = read_node_progress(self.state)
        first = data[self.node_path]["items"]["z-build"]
        self.assertEqual((first["kind"], first["agent"], first["progress"]), ("worker", "Builder", None))
        self.assertTrue(first["updated_at"].endswith("+00:00"))
        warnings = []
        graph, progress = build_node_sub_graph(self.node_path, data[self.node_path], warnings)
        self.assertEqual(warnings, [])
        self.assertEqual([(node["id"], node["wave"]) for node in graph["nodes"]], [("z-build", 0), ("a-smoke", 1)])
        self.assertEqual(graph["edges"], [{"from": self.node_path + "::z-build", "to": self.node_path + "::a-smoke"}])
        self.assertEqual(progress, {"total": 2, "done": 1, "running": 1, "failed": 0, "waiting": 0, "pending": 0})
        self.assertIn("[redacted]", graph["nodes"][0]["detail"])
        self.assertNotIn("very-secret", json.dumps(graph))
        self.assertNotIn("multi-word fragment", json.dumps(graph))
        snapshot = self.snapshot()
        self.assertEqual(snapshot["nodes"][0]["children"][0]["sub_graph"], graph)
        update_node_progress(self.state, self.node_path, item_id="z-build", clear=True)
        self.assertEqual(list(read_node_progress(self.state)[self.node_path]["items"]), ["a-smoke"])
        update_node_progress(self.state, self.node_path, clear=True)
        self.assertEqual(read_node_progress(self.state), {})

    def test_parallel_progress_and_list_sidecar(self):
        path = progress_path_for_state(self.state)
        self.assertEqual(path.name, "state_progress.json")
        self.write(path, {self.node_path: {"items": [{"id": "first", "order": 5, "status": "pending"}]}})
        with ThreadPoolExecutor(max_workers=8) as executor:
            results = executor.map(lambda i: update_node_progress(self.state, self.node_path, item_id=f"worker-{i}"),
                                   range(24))
            list(results)
        items = read_node_progress(self.state)[self.node_path]["items"]
        self.assertEqual(len(items), 25)
        self.assertEqual(sorted(item["order"] for item in items.values()), list(range(5, 30)))
        path.write_text("{", encoding="utf-8")
        warnings = []
        self.assertEqual(read_node_progress(self.state, warnings), {})
        self.assertIn("state_progress.json", warnings[0])

    def test_progress_validation_and_dependency_warnings(self):
        bad = ({"node_path": "a/../b"}, {"node_path": "a//b"}, {"item_id": "../item"}, {"status": "stopped"},
               {"progress": True}, {"progress": float("nan")}, {"progress": 101}, {"kind": "unknown"})
        for change in bad:
            with self.subTest(change=change), self.assertRaises(ValueError):
                update_node_progress(self.state, **{"node_path": self.node_path, "item_id": "item", **change})
        for name, deps in (("first", ["second", "missing"]), ("second", ["first"])):
            update_node_progress(self.state, self.node_path, item_id=name, deps=deps)
        warnings = []
        graph, _ = build_node_sub_graph(self.node_path, read_node_progress(self.state)[self.node_path], warnings)
        self.assertEqual(len(graph["edges"]), 2)
        self.assertTrue(any("unknown sibling missing" in warning for warning in warnings))
        self.assertTrue(any("Dependency cycle" in warning for warning in warnings))

    def test_progress_cli_push_and_clear(self):
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        args = ["progress", str(self.state), "--node", self.node_path]
        with redirect_stdout(StringIO()) as stdout:
            result = main([*args, "--item", "smoke", "--status", "running", "--progress", "55.5",
                           "--agent", "QA", "--output", "result.json", "--push", "--hub-url", base,
                           "--token", "test-token", "--run-id", "progress-mission", "--host-label", "origin", "--json"])
        self.assertEqual(result, 0)
        self.assertEqual(json.loads(stdout.getvalue())[self.node_path]["items"]["smoke"]["progress"], 55.5)
        status, snapshot = self.request(base, "/api/snapshot?run=progress-mission")
        self.assertEqual(status, 200)
        child = snapshot["nodes"][0]["children"][0]["sub_graph"]["nodes"][0]
        self.assertEqual((child["path"], child["progress"], child["agent"]), ("release/confirm::smoke", 55.5, "QA"))
        with redirect_stdout(StringIO()) as stdout:
            result = main([*args, "--clear", "--hub-url", base, "--token", "test-token", "--host-label", "origin",
                           "--run-id", "progress-mission", "--json"])
        self.assertEqual((result, json.loads(stdout.getvalue())), (0, {}))
        self.assertEqual(self.request(base, "/api/snapshot?run=progress-mission")[1]["nodes"], [])

    def test_ask_and_answer_cli_with_same_host_push(self):
        self.paused({"title": "Verify WebGL runtime", "instructions": ["Collect the After captures"]})
        workflow = self.project / "workflow.py"
        run = register_run(workflow, self.state, self.report, project_dir=self.project, status="awaiting_subagents")
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        run_id = run["id"]
        with redirect_stdout(StringIO()):
            self.assertEqual(main(["push", "--state", str(self.state), "--report", str(self.report),
                                   "--hub-url", base, "--token", "test-token"]), 0)
        self.assertEqual(list_runs()[0]["source"], "remote")
        decision = [str(self.state), "--node", self.node_path, "--item", "pointer-lock", "--json"]
        with redirect_stdout(StringIO()) as stdout:
            self.assertEqual(main(["ask", *decision, "--question", "How should Walk mode exit full screen?",
                                   "--option", "Use Esc", "--option", "Add a button", "--recommended", "Use Esc",
                                   "--ticket", "20705b78", "--no-note", "--push", "--report", str(self.report),
                                   "--hub-url", base, "--token", "test-token", "--run-id", run_id]), 0)
        stored = json.loads(stdout.getvalue())[self.node_path]["items"]["pointer-lock"]
        self.assertEqual((stored["status"], stored["kind"], stored["agent"]), ("awaiting_subagents", "gate", "Reviewer"))
        self.assertEqual(list_runs()[0]["source"], "remote")
        with redirect_stdout(StringIO()) as stdout:
            self.assertEqual(main(["answer", *decision, "--wait", "--poll-ms", "2", "--timeout", "0.01"]), 4)
        self.assertIsNone(json.loads(stdout.getvalue()))

        snapshot_url = f"/api/snapshot?run={run_id}"
        _, snapshot = self.request(base, snapshot_url)
        gate = snapshot["attention"]["input_gate"]
        question, = gate["questions"]
        self.assertEqual(question["options"], ["Use Esc", "Add a button"])
        self.assertEqual((question["recommended"], question["ticket"], question["allow_note"]), ("Use Esc", "20705b78", False))
        respond = f"/api/runs/{run_id}/respond"
        payload = {"node_path": gate["node_path"], "packet_hash": gate["packet_hash"],
                   "submitted_at": None, "decisions": {question["id"]: "Use Esc"}, "notes": "Keep the hint."}
        self.assertEqual(self.request(base, respond, "POST", payload, authorized=False)[0], 401)
        self.assertEqual(self.request(base, respond, "POST", {**payload, "node_path": self.node_path + "::../outside"})[0], 400)
        self.assertEqual(self.request(base, respond, "POST", {**payload, "packet_hash": "old"})[0], 409)
        self.assertEqual(self.request(base, respond, "POST", {**payload, "decisions": {question["id"]: "Other"}})[0], 400)
        status, submitted = self.request(base, respond, "POST", payload)
        self.assertEqual((status, submitted["delivery"]), (200, "local_subtask"))
        output = self.state.parent / "artifacts" / "pointer-lock_decision.json"
        self.assertEqual(Path(submitted["output_path"]), output)
        self.assertEqual(json.loads(output.read_text()), submitted["decision"])
        self.assertEqual(self.request(base, respond, "POST", payload)[0], 409)
        _, fresh = self.request(base, snapshot_url)
        leaf, = fresh["nodes"][0]["children"][0]["sub_graph"]["nodes"]
        self.assertEqual((leaf["status"], leaf["progress"], leaf["decision_summary"]), ("passed", 100, submitted["summary"]))
        self.assertEqual(leaf["submitted"]["notes"], "Keep the hint.")
        self.assertEqual(fresh["attention"]["kind"], "waiting")
        self.assertEqual(list_runs()[0]["source"], "local")
        with redirect_stdout(StringIO()) as stdout:
            self.assertEqual(main(["answer", *decision]), 0)
        self.assertEqual(json.loads(stdout.getvalue()), submitted["decision"])
        progress_path_for_state(self.state).unlink()
        self.assertEqual(read_subtask_answer(self.state, self.node_path, "pointer-lock"), submitted["decision"])
        self.assertIsNone(read_subtask_answer(self.state, "another/confirm", "pointer-lock"))

    def test_legacy_human_wait_is_elevated_and_can_be_edited(self):
        self.paused({"title": "Verify WebGL runtime"}, node_path="fix_verify_and_review/verify_webgl_runtime_and_after")
        item_id = "b1_20705b78_decision"
        detail = "Pointer Lock blocks clicking Exit full screen during Walk without Esc. Which UX should we use?"
        update_node_progress(self.state, self.node_path, item_id=item_id, status="awaiting_subagents",
                             agent="rEvIeWeR", title="Reviewer input needed for pointer lock", detail=detail)
        update_node_progress(self.state, self.node_path, item_id="worker", status="awaiting_subagents", agent="QA bot")
        attention = self.snapshot()["attention"]
        gate = attention["input_gate"]
        self.assertEqual((attention["kind"], attention["node_path"]), ("decision", f"{self.node_path}::{item_id}"))
        self.assertEqual(attention["items"], [detail])
        self.assertEqual(gate["questions"], [{"id": item_id, "ticket": None, "question": detail, "options": [],
                                              "recommended": None, "multi_select": False, "allow_note": True}])
        status, saved = self.submit(packet_hash=gate["packet_hash"], decisions={item_id: "Use Esc"})
        self.assertEqual((status, saved["delivery"]), (200, "local_subtask"))
        stored = read_node_progress(self.state)[self.node_path]["items"][item_id]
        self.assertEqual((stored["status"], stored["decision"]), ("passed", saved["decision"]))
        self.assertEqual(self.submit(node_path=gate["node_path"], submitted_at=saved["answered_at"],
                                     packet_hash=gate["packet_hash"], decisions={item_id: "Add a button"})[0], 200)
        self.assertEqual(read_subtask_answer(self.state, self.node_path, item_id)["decisions"], {item_id: "Add a button"})
        ask_node_question(self.state, self.node_path, item_id=item_id, question="Where should the button appear?")
        self.assertIsNone(read_subtask_answer(self.state, self.node_path, item_id))
        self.assertEqual(self.submit(node_path=gate["node_path"], submitted_at=saved["answered_at"],
                                     packet_hash=gate["packet_hash"], decisions={item_id: "Old answer"})[0], 409)

    def test_progress_question_refreshes_cached_same_host_run(self):
        self.paused({"title": "Verify WebGL runtime"})
        run = register_run(self.project / "workflow.py", self.state, self.report, project_dir=self.project,
                           status="awaiting_subagents")
        ingest_remote_run({"source_host": socket.gethostname()}, self.snapshot())
        self.assertEqual(list_runs()[0]["source"], "remote")
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        with redirect_stdout(StringIO()):
            self.assertEqual(main(["progress", str(self.state), "--node", self.node_path, "--item", "review",
                                   "--status", "awaiting_subagents", "--question", "Continue?", "--option", "Yes",
                                   "--recommended", "Yes", "--ticket", "b1"]), 0)
        _, live = self.request(base, f"/api/snapshot?run={run['id']}")
        self.assertEqual(live["attention"]["input_gate"]["questions"][0]["ticket"], "b1")
        self.assertEqual(list_runs()[0]["source"], "local")

    def test_newest_subtask_question_and_outer_packet_precedence(self):
        self.paused({"title": "Runtime review"})
        ask_node_question(self.state, self.node_path, item_id="older", question="First question?")
        ask_node_question(self.state, self.node_path, item_id="newer", question="Second question?")
        self.assertEqual(self.snapshot()["attention"]["node_path"], f"{self.node_path}::newer")
        self.write(self.packet, {"question": "Outer question?", "output_path": self.output.name})
        self.assertEqual(self.snapshot()["attention"]["node_path"], self.node_path)

    def test_subtask_decision_cannot_write_outside_kgraph(self):
        self.paused({"title": "Runtime review"})
        ask_node_question(self.state, self.node_path, item_id="review", question="Continue?")
        outside = self.project / "outside"
        outside.mkdir()
        (self.state.parent / "artifacts").symlink_to(outside, target_is_directory=True)
        target = f"{self.node_path}::review"
        self.assertEqual(self.submit(node_path=target, decisions={"q1": "Yes"})[0], 403)
        self.assertFalse((outside / "review_decision.json").exists())
        self.assertIsNone(read_subtask_answer(self.state, self.node_path, "review"))

    def test_running_remote_subtask_response_survives_ingest(self):
        self.write(self.state, {"nodes": {self.node_path: {"status": "running"}}})
        questions = [{"id": "modes", "question": "Which modes?", "multi_select": True,
                      "options": ["Walk", "Fly"], "allow_note": False}]
        question_file = self.write(self.project / "questions.json", questions)
        with redirect_stdout(StringIO()):
            self.assertEqual(main(["ask", str(self.state), "--node", self.node_path, "--item", "flight",
                                   "--questions-json", str(question_file)]), 0)
        snapshot = self.snapshot()
        gate = snapshot["attention"]["input_gate"]
        meta = {"run_id": "remote-subtask", "source_host": "another-host"}
        run = ingest_remote_run(meta, snapshot)
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        prefix = f"/api/runs/{run['id']}"
        payload = {"node_path": gate["node_path"], "packet_hash": gate["packet_hash"],
                   "decisions": {"modes": ["Walk", "Fly"]}}
        status, response = self.request(base, prefix + "/respond", "POST", payload)
        self.assertEqual((status, response["delivery"]), (200, "remote_queue"))
        self.assertIsNone(read_subtask_answer(self.state, self.node_path, "flight"))
        _, queued = self.request(base, prefix + "/response")
        self.assertEqual(queued["decisions"], payload["decisions"])
        saved = json.loads(Path(run["remote_snapshot_path"]).read_text())
        self.assertEqual(saved["attention"]["input_gate"]["submitted"]["answered_at"], queued["answered_at"])
        ingest_remote_run(meta, snapshot)
        _, live = self.request(base, f"/api/snapshot?run={run['id']}")
        parent = live["nodes"][0]["children"][0]
        item, = parent["sub_graph"]["nodes"]
        self.assertEqual((item["status"], parent["sub_progress"]["done"], parent["sub_progress"]["waiting"]), ("passed", 1, 0))
        self.assertEqual(item["submitted"]["decisions"], payload["decisions"])

    def test_questions_gate_choices_and_persisted_answer(self):
        packet = {"questions": [
            {"id": "approach", "ticket": "issue-1", "question": "Choose the token=do-not-show rollout",
             "options": ["Pilot", "Broad"], "recommended": "Pilot", "allow_note": False},
            {"id": "checks", "question": "Choose checks", "options": ["Desktop", "Mobile"], "multi_select": True,
             "allow_note": False},
        ], "output_path": self.output.name}
        self.paused(packet)
        gate = self.extract(packet)
        self.assertEqual(gate["gate_type"], "questions")
        self.assertEqual(gate["questions"][0]["recommended"], "Pilot")
        self.assertTrue(gate["can_respond"])
        self.assertEqual(gate["packet_hash"], self.extract(dict(reversed(list(packet.items()))))["packet_hash"])
        self.assertNotIn(self.output.name, json.dumps(gate))
        self.assertNotIn("do-not-show", json.dumps(gate))
        self.assertEqual(self.submit(decisions={"approach": "Unlisted", "checks": ["Desktop"]})[0], 400)
        self.assertEqual(self.submit(decisions={"approach": "Pilot", "checks": "Desktop"})[0], 400)
        self.assertEqual(self.submit(decisions={"approach": "Pilot"})[0], 400)
        status, result = self.submit(decisions={"approach": "Pilot", "checks": ["Desktop", "Mobile", "Desktop"]},
                                     approved_by=" Reviewer ", notes="Use token=do-not-save",
                                     output_path=str(self.project / "injected.json"), packet_hash=gate["packet_hash"])
        self.assertEqual((status, result["delivery"]), (200, "local"))
        doc = json.loads(self.output.read_text())
        self.assertEqual(doc, result["decision"])
        self.assertEqual((doc["approved_by"], doc["decisions"]["checks"]), ("Reviewer", ["Desktop", "Mobile"]))
        self.assertFalse((self.project / "injected.json").exists())
        self.assertNotIn("do-not-save", doc["notes"])
        attention = self.snapshot()["attention"]
        self.assertEqual(attention["kind"], "decision")
        self.assertTrue(attention["items"][0].startswith("[issue-1]"))
        self.assertEqual(attention["input_gate"]["submitted"]["answered_at"], doc["answered_at"])
        answers = {"approach": "Broad", "checks": ["Desktop"]}
        self.assertEqual(self.submit(decisions=answers, submitted_at=None)[0], 409)
        self.assertEqual(self.submit(decisions=answers, submitted_at=doc["answered_at"])[0], 200)

    def test_recommended_only_and_review_revision(self):
        packet = {"questions": [{"id": "choice", "question": "Which approach?", "recommended": "Pilot"}],
                  "output_path": self.output.name, "revision": 1}
        self.paused(packet)
        spec = self.extract(packet)
        self.assertEqual((spec["questions"][0]["recommended"], spec["questions"][0]["options"]), ("Pilot", []))
        self.assertEqual(self.submit(decisions={"choice": "Use a smaller pilot"}, packet_hash=spec["packet_hash"])[0], 200)
        packet["revision"] += 1
        self.write(self.packet, packet)
        os.utime(self.packet, ns=(self.output.stat().st_mtime_ns + 1_000_000_000,) * 2)
        self.assertNotIn("submitted", self.extract(packet))
        self.assertEqual(self.submit(decisions={"choice": "Pilot"}, packet_hash=spec["packet_hash"])[0], 409)

    def test_approval_and_rejection_with_independent_fields(self):
        packet = {"stage": "user_verify_gate", "review_sha256": "abc", "review_summary": "Review assets",
                  "outputs": [self.output.name], "bugs": [{"id": "one", "title": "Fix one"}, {"id": "two", "title": "Fix two"}],
                  "approval_example": {"approved": True, "approve_pilot": True, "approve_release": True,
                                       "approve_secret": True}}
        self.paused(packet, node_path="release/user_verify_gate")
        spec = self.extract(packet)
        self.assertEqual(spec["gate_type"], "approval")
        self.assertEqual(spec["approved_fields"], [
            {"key": "approve_pilot", "label": "Approve pilot", "default": True},
            {"key": "approve_release", "label": "Approve release", "default": True}])
        self.assertEqual(self.submit(approved="true")[0], 400)
        self.assertEqual(self.submit(approved=True, approve_pilot="false")[0], 400)
        status, result = self.submit(approved=True, approved_fields={"approve_pilot": False})
        self.assertEqual(status, 200)
        doc = result["decision"]
        self.assertEqual((doc["review_sha256"], doc["approve_pilot"], doc["approve_release"]),
                         ("abc", False, True))
        self.assertEqual(self.submit(approved=False, rejected_bugs=["unknown"])[0], 400)
        status, result = self.submit(approved=False, approved_by="Release Reviewer", rejected_bugs=["two", "two"])
        self.assertEqual(status, 200)
        doc = result["decision"]
        self.assertEqual((doc["rejected_by"], doc["rejected_bugs"]), ("Release Reviewer", ["two"]))
        self.assertFalse(self.extract(packet)["submitted"]["approved"])
        self.write(self.output, {**doc, "review_sha256": "different"})
        self.assertNotIn("submitted", self.extract(packet))

    def test_hybrid_single_question_and_dictionary_fields(self):
        packet = {"question": "Which team?", "recommended": "QA", "id": "team",
                  "approval_path": self.output.name, "approved_fields": {
                      "submit_bundle": {"label": "Send bundle", "default": False},
                      "notes": True, "private_key": True}}
        self.paused(packet)
        gate = self.extract(packet)
        self.assertEqual(gate["gate_type"], "hybrid")
        self.assertEqual(gate["approved_fields"], [{"key": "submit_bundle", "label": "Send bundle", "default": False}])
        self.assertEqual(self.submit(approved=True)[0], 400)
        status, result = self.submit(approved=True, decisions={"team": "QA"})
        self.assertEqual(status, 200)
        self.assertEqual(result["decision"]["decisions"], {"team": "QA"})
        self.assertFalse(result["decision"]["submit_bundle"])
        self.assertEqual(self.submit(approved_by="", approved=True, decisions={"team": "QA"})[0], 400)
        self.assertEqual(self.submit(notes="a" * 8001, approved=True, decisions={"team": "QA"})[0], 400)

    def test_gate_paths_and_passed_nodes_are_rejected(self):
        outside = self.project / "outside.json"
        bypass = self.state.parent / "linked"
        self.state.parent.mkdir(parents=True)
        bypass.symlink_to(self.project, target_is_directory=True)
        direct = self.state.parent / "escape.json"
        direct.symlink_to(outside)
        for output in ("../outside.json", r"..\outside.json", str(outside), "linked/outside.json", direct.name, "output.txt",
                       self.state.name, self.report.name, self.packet.name, "nul\x00.json"):
            with self.subTest(output=output):
                packet = {"question": "Continue?", "output_path": output}
                self.paused(packet)
                status, _ = self.submit(decisions={"q1": "Yes"})
                self.assertIn(status, (400, 403))
        self.assertFalse(outside.exists())
        self.write(outside, {"question": "Continue?", "output_path": self.output.name})
        self.write(self.report, {"status": "awaiting_subagents", "packet_path": str(outside)})
        self.assertEqual(self.submit(decisions={"q1": "Yes"})[0], 403)
        link_packet = self.state.with_name("redirect_packet.json")
        link_packet.symlink_to(outside)
        self.write(self.report, {"status": "awaiting_subagents", "packet_path": str(link_packet)})
        self.assertEqual(self.submit(decisions={"q1": "Yes"})[0], 403)
        self.assertNotIn("input_gate", self.snapshot()["attention"])
        self.assertEqual(validate_kgraph_output_path("answer.json", state_path=self.state, packet_path=self.packet),
                         self.output)
        self.paused({"question": "Continue?", "output_path": "answer.json"})
        self.write(self.state, {"nodes": {self.node_path: {"status": "passed"}}})
        self.assertEqual(self.submit(decisions={"q1": "Yes"})[0], 409)
        self.assertFalse(self.output.exists())
        outside_run = {**self.run_entry, "state_path": str(outside)}
        self.assertEqual(submit_gate_response(outside_run, {"decisions": {"q1": "Yes"}})[0], 403)
        self.assertFalse(self.project.joinpath("outside_response.lock").exists())

    def test_parallel_report_points_to_first_gate(self):
        self.paused({"question": "Continue?", "output_path": self.output.name})
        self.write(self.state, {"nodes": {
            self.node_path: {"kind": "gate", "status": "awaiting_subagents"},
            "another": {"kind": "gate", "status": "awaiting_subagents"},
        }})
        self.write(self.report, {"status": "awaiting_subagents", "packet_path": str(self.packet),
                                 "error": "[root] [release] [confirm] Waiting; [another] Waiting"})
        self.assertEqual(self.snapshot()["attention"]["node_path"], self.node_path)
        self.assertEqual(self.submit(node_path="another", decisions={"q1": "Yes"})[0], 409)
        self.assertEqual(self.submit(decisions={"q1": "Yes"})[0], 200)

    def test_hub_post_writes_decision_and_cli_resumes(self):
        workflow = self.project / "workflow.py"
        workflow.write_text(WORKFLOW, encoding="utf-8")
        args = [sys.executable, "-m", "kgraph_runner.cli", "run", str(workflow), "--state", str(self.state),
                "--report", str(self.report), "--quiet"]
        first = subprocess.run(args, cwd=self.project, capture_output=True, text=True)
        self.assertEqual(first.returncode, 4, first.stderr)
        run_id = list_runs()[0]["id"]
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        target = f"/api/runs/{run_id}/respond"
        _, snapshot = self.request(base, f"/api/snapshot?run={run_id}")
        gate = snapshot["attention"]["input_gate"]
        self.assertEqual(gate["node_path"], self.node_path)
        payload = {"node_path": self.node_path, "packet_hash": gate["packet_hash"],
                   "decisions": {gate["questions"][0]["id"]: "Proceed with the pilot"}}
        self.assertEqual(self.request(base, target, "POST", payload, authorized=False)[0], 401)
        self.assertEqual(self.request(base, target, "POST", {**payload, "node_path": "release/other"})[0], 409)
        self.assertEqual(self.request(base, target, "POST", payload)[0], 200)
        self.assertTrue(json.loads(self.output.read_text())["resolved"])
        second = subprocess.run(args, cwd=self.project, capture_output=True, text=True)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(list_runs()[0]["status"], "passed")
        self.assertEqual(self.request(base, target, "POST", payload)[0], 409)

    def test_single_server_post_validation(self):
        self.paused({"question": "Continue?", "output_path": self.output.name})
        base = self.serve(create_town_server(self.snapshot, port=0, token="test-token"))
        target = "/api/respond"
        for invalid in ([], 2, b'{"bad": NaN}', b'{"bad":'):
            self.assertEqual(self.request(base, target, "POST", invalid)[0], 400)
        headers = {"Content-Length": str(256 * 1024 + 1)}
        self.assertEqual(self.request(base, target, "POST", b"", headers=headers)[0], 413)
        headers = {"Content-Length": "2", "Transfer-Encoding": "chunked"}
        self.assertEqual(self.request(base, target, "POST", b"{}", headers=headers)[0], 400)
        payload = {"decisions": {"q1": "Yes"}}
        self.assertEqual(self.request(base, target, "POST", payload, authorized=False)[0], 401)
        self.assertEqual(self.request(base, target, "POST", payload, headers={"Sec-Fetch-Site": "cross-site"})[0], 403)
        self.assertEqual(self.request(base, target, "POST", payload)[0], 200)
        self.assertTrue(self.output.is_file())

    def test_remote_queue_survives_progress_refresh(self):
        self.paused({"question": "Continue?", "output_path": self.output.name})
        snapshot = self.snapshot()
        meta = {"run_id": "remote-review", "source_host": "origin", "project_name": "Remote"}
        run = ingest_remote_run(meta, snapshot)
        base = self.serve(create_hub_server(host="127.0.0.1", port=0, token="test-token"))
        prefix = f"/api/runs/{run['id']}"
        self.assertEqual(self.request(base, prefix + "/response")[0], 404)
        self.assertEqual(self.request(base, prefix + "/response", authorized=False)[0], 401)
        self.assertEqual(self.request(base, "/api/runs/%2E%2E/response")[0], 404)
        self.assertIsNone(read_remote_response("bad/id"))
        gate = snapshot["attention"]["input_gate"]
        payload = {"node_path": gate["node_path"], "packet_hash": gate["packet_hash"], "decisions": {"q1": "Yes"}}
        status, response = self.request(base, prefix + "/respond", "POST", payload)
        self.assertEqual((status, response["delivery"]), (200, "remote_queue"))
        self.assertFalse(self.output.exists())
        status, queued = self.request(base, prefix + "/response")
        self.assertEqual(status, 200)
        self.assertEqual(queued["node_path"], self.node_path)
        self.assertEqual(queued["packet_hash"], gate["packet_hash"])
        self.assertEqual(queued["decisions"], response["decision"]["decisions"])
        saved = json.loads(Path(run["remote_snapshot_path"]).read_text())
        self.assertEqual(saved["attention"]["input_gate"]["submitted"]["answered_at"], response["answered_at"])
        ingest_remote_run(meta, snapshot)
        _, current = self.request(base, f"/api/snapshot?run={run['id']}")
        self.assertEqual(current["attention"]["input_gate"]["submitted"]["answered_at"], response["answered_at"])
        gate["packet_hash"] = "new-packet"
        ingest_remote_run(meta, snapshot)
        _, current = self.request(base, f"/api/snapshot?run={run['id']}")
        self.assertNotIn("submitted", current["attention"]["input_gate"])
        self.assertEqual(self.request(base, prefix + "/respond", "POST", payload)[0], 409)

    def test_ui_embeds_input_drilldown_and_valid_inline_javascript(self):
        self.paused({"question": "Continue?", "output_path": self.output.name})
        update_node_progress(self.state, self.node_path, item_id="smoke", status="running")
        static = render_town_html(self.snapshot())
        live = render_town_html(self.snapshot(), live_url="/api/snapshot", respond_url="/api/respond")
        self.assertIn('"respond": null', static)
        self.assertIn('"respond": "/api/respond"', live)
        for setting in ('id="input-gate"', 'id="drill-breadcrumbs"', 'Jump to Input Form', 'Open Sub-graph ↗',
                        'inputGateSignature', 'Edit & Resubmit', 'Read-only input preview', 'drillStack',
                        'release/confirm::smoke', 'INPUT NEEDED'):
            self.assertIn(setting, static)
        self.assertRegex(static, r"\.drill-breadcrumbs \{[^}]*flex-wrap: wrap")
        self.assertRegex(static, r"\.drill-breadcrumbs \.gate-pill \{[^}]*flex: none; white-space: nowrap")
        self.assertIn("nodeIndex.get(path.split('::', 1)[0])", live)
        self.assertIn("openSubGraph(parent, path, {scroll})", live)
        self.assertIn("node.input_gate && !node.input_gate.submitted", live)
        node = shutil.which("node")
        if node is not None:
            for html in (static, live):
                scripts = re.findall(r"<script>(.*?)</script>", html, re.DOTALL)
                self.assertEqual(len(scripts), 1)
                result = subprocess.run([node, "--check"], input=scripts[0], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
