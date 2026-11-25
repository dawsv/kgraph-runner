"""Standalone coverage for recorded and live Hero Town views."""

from contextlib import contextmanager
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Thread
import unittest
from unittest.mock import patch

from kgraph_runner import (
    build_town_snapshot, create_hub_server, create_town_server, render_town_html, update_node_progress,
)


WORKFLOW = """\
from pathlib import Path
import json

from kgraph_runner import SplitterNode, subagent_gate

ROOT = Path(__file__).parent / ".kgraph"


def gate(item, ctx):
    packet = ROOT / f"{item}_packet.json"
    answer = ROOT / f"{item}_answer.json"
    return subagent_gate(
        item, packet_path=packet, artifact_paths=[answer],
        packet_fn=lambda ctx: {"question": f"Approve {item}?", "output_path": answer.name},
        input_hash_fn=lambda ctx: item,
        verify_fn=lambda ctx, outputs: (json.loads(answer.read_text())["resolved"], "", "Decided"),
    )


def build_graph():
    return [SplitterNode(id="review", items_fn=lambda ctx: ["pilot", "launch"], child_builder_fn=gate)]
"""


class TownTests(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.project = Path(temp.name).resolve()
        env = patch.dict(os.environ, {
            "KGRAPH_HOME": str(self.project / "registry"),
            "KGRAPH_HUB_URL": "",
            "PYTHONDONTWRITEBYTECODE": "1",
        })
        env.start()
        self.addCleanup(env.stop)
        self.state = self.project / ".kgraph" / "workflow_state.json"
        self.report = self.state.with_name("workflow_report.json")
        self.workflow = self.project / "workflow.py"
        self.workflow.write_text(WORKFLOW, encoding="utf-8")

    @contextmanager
    def serve(self, server):
        with server:
            thread = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.025}, daemon=True)
            thread.start()
            try:
                yield server.server_port
            finally:
                server.shutdown()
                thread.join(timeout=2)

    @staticmethod
    def request(port, path, *, payload=None, token=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
        request_headers = dict(headers or {})
        if token:
            request_headers["Authorization"] = f"Bearer {token}"
        try:
            body = None
            if payload is not None:
                request_headers["Content-Type"] = "application/json"
                body = json.dumps(payload).encode()
            connection.request("GET" if body is None else "POST", path, body, request_headers)
            response = connection.getresponse()
            content = response.read().decode()
            if response.getheader("Content-Type", "").startswith("application/json"):
                content = json.loads(content)
            return response.status, content
        finally:
            connection.close()

    def test_snapshot_topology_and_subgraph_layout(self):
        update_node_progress(self.state, "review/pilot", item_id="draft", status="passed")
        update_node_progress(self.state, "review/pilot", item_id="check", deps=["draft"], status="running")
        snapshot = build_town_snapshot(self.workflow, self.state, self.report)
        review, = snapshot["nodes"]
        pilot, launch = review["children"]
        self.assertEqual((review["node_type"], pilot["path"], launch["path"]),
                         ("splitter", "review/pilot", "review/launch"))
        self.assertEqual(pilot["sub_graph"]["edges"], [{"from": "review/pilot::draft", "to": "review/pilot::check"}])
        self.assertEqual([node["wave"] for node in pilot["sub_graph"]["nodes"]], [0, 1])
        self.assertEqual((pilot["sub_progress"]["done"], pilot["sub_progress"]["total"]), (1, 2))
        self.assertNotIn("sub_graph", launch)
        self.assertNotIn("qa_scorecard", snapshot)
        html = render_town_html(snapshot)
        self.assertRegex(html, r"\.inspector-head \{[^}]*position: sticky")
        self.assertRegex(html, r'(?s)class="inspector-head".*?id="open-subgraph".*?id="inspect-subtasks".*?id="inspect-fields"')
        self.assertNotIn("/artifacts/", html)

    def test_parallel_splitter_gates_pause_and_each_resume_through_hub(self):
        cli = [sys.executable, "-B", "-m", "kgraph_runner.cli", "run", str(self.workflow), "--quiet"]
        with self.serve(create_hub_server(port=0, token="test-token")) as port:
            seen = set()
            for _ in range(2):
                paused = subprocess.run(cli, cwd=self.project, capture_output=True, text=True)
                self.assertEqual(paused.returncode, 4, paused.stderr)
                status, runs = self.request(port, "/api/runs", token="test-token")
                self.assertEqual(status, 200)
                run, = runs["runs"]
                status, snapshot = self.request(port, f"/api/snapshot?run={run['id']}", token="test-token")
                self.assertEqual(status, 200)
                gate = snapshot["attention"]["input_gate"]
                self.assertNotIn(gate["node_path"], seen)
                seen.add(gate["node_path"])
                status, response = self.request(
                    port, f"/api/runs/{run['id']}/respond", token="test-token",
                    payload={"node_path": gate["node_path"], "packet_hash": gate["packet_hash"],
                             "decisions": {"q1": "Proceed"}},
                )
                self.assertEqual(status, 200, response)
            self.assertEqual(seen, {"review/pilot", "review/launch"})
            resumed = subprocess.run(cli, cwd=self.project, capture_output=True, text=True)
            self.assertEqual(resumed.returncode, 0, resumed.stderr)

    def test_tokenless_views_and_remote_ingest_authentication(self):
        remote = {"run": {"run_id": "remote-run", "source_host": "other-host"},
                  "snapshot": {"title": "Remote run", "status": "running", "nodes": [], "edges": []}}
        with self.serve(create_hub_server(port=0)) as port:
            self.assertEqual(self.request(port, "/api/snapshot")[0], 200)
            self.assertEqual(self.request(port, "/api/ingest", payload=remote)[0], 401)
            self.assertEqual(self.request(port, "/api/runs/remote-run/respond", payload={})[0], 401)
            self.assertEqual(self.request(port, "/api/runs/remote-run/response")[0], 401)
        with self.serve(create_hub_server(port=0, token="secret")) as port:
            self.assertEqual(self.request(port, "/api/ingest", payload=remote)[0], 401)
            status, _ = self.request(port, "/api/ingest", payload=remote, token="secret",
                                     headers={"Origin": "https://other.invalid"})
            self.assertEqual(status, 403)
            self.assertEqual(self.request(port, "/api/ingest", payload=remote, token="secret")[0], 200)
            status, snapshot = self.request(port, "/api/snapshot?run=remote-run", token="secret")
            self.assertEqual((status, snapshot["title"]), (200, "Remote run"))
            self.assertEqual(self.request(port, "/api/runs/remote-run/response")[0], 401)

    def test_single_server_checks_origin_and_honors_read_only(self):
        def snapshot():
            return build_town_snapshot(self.workflow, self.state, self.report)

        with self.serve(create_town_server(snapshot, port=0, token="secret")) as port:
            url = f"http://127.0.0.1:{port}"
            for origin in (f"https://127.0.0.1:{port}", "https://other.invalid", "null"):
                with self.subTest(origin=origin):
                    status, _ = self.request(port, "/api/respond", payload={}, token="secret",
                                             headers={"Origin": origin, "Sec-Fetch-Site": "same-origin"})
                    self.assertEqual(status, 403)
            status, response = self.request(port, "/api/respond", payload={}, token="secret",
                                            headers={"Origin": url, "Sec-Fetch-Site": "same-origin"})
            self.assertEqual((status, response["error"]), (409, "The requested node is no longer awaiting input"))
        with self.serve(create_town_server(snapshot, port=0, token="secret", read_only=True)) as port:
            self.assertEqual(self.request(port, "/api/respond", token="secret", payload={})[0], 403)
            self.assertIn('"respond": null', self.request(port, "/", token="secret")[1])
