"""Live snapshot, node log, and manual QA HTTP coverage."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import http.client
import json
import os
from pathlib import Path
import py_compile
import socket
from tempfile import TemporaryDirectory
from threading import Thread
import unittest
from unittest.mock import patch
from urllib.parse import urlencode

from kgraph_runner._files import write_json
from kgraph_runner.interactions import find_run_qa_checklist, update_node_progress
from kgraph_runner.qa_metrics import GAP_TYPES, MANUAL_QA_VALUES, init_qa_checklist
from kgraph_runner.registry import ingest_remote_run, register_run
from kgraph_runner.server import create_hub_server, create_town_server
from kgraph_runner.snapshot import build_town_snapshot, cached_town_snapshot


class ServerFeatureTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.project = Path(tmp.name).resolve()
        home = patch.dict(os.environ, {"KGRAPH_HOME": str(self.project / "hub")})
        home.start()
        self.addCleanup(home.stop)
        self.state = self.project / ".kgraph" / "run" / "state.json"
        self.report = self.state.with_name("report.json")
        self.workflow = self.project / "workflow.py"
        self.checklist = self.state.parent / "state_qa" / "manual_qa.json"
        self.write_node(status="running")

    def write_node(self, **fields):
        write_json(self.state, {"nodes": {"work": {"kind": "worker", **fields}}})

    def snapshot(self):
        return build_town_snapshot(state_path=self.state, report_path=self.report)

    @contextmanager
    def serve(self, mode="single", *, token="secret", read_only=False, poll_ms=20, snapshot_fn=None):
        options = dict(port=0, token=token, read_only=read_only, poll_ms=poll_ms)
        if mode == "single":
            server = create_town_server(snapshot_fn or self.snapshot, **options)
            prefix = "/api"
        else:
            run = register_run(self.workflow, self.state, self.report, project_dir=self.project)
            server = create_hub_server(**options)
            prefix = f"/api/runs/{run['id']}"
        with server:
            thread = Thread(target=server.serve_forever, kwargs={"poll_interval": .02}, daemon=True)
            thread.start()
            try:
                yield server, prefix
            finally:
                server.shutdown()
                thread.join(2)

    def request(self, server, path, *, method="GET", payload=None, token="secret", headers=None):
        headers = dict(headers or {})
        if token is not None:
            headers["Authorization"] = f"Bearer {token}"
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
        try:
            body = json.dumps(payload).encode() if payload is not None else None
            connection.request(method, path, body=body, headers=headers)
            response = connection.getresponse()
            data = response.read()
            if response.getheader("Content-Type", "").startswith("application/json"):
                data = json.loads(data)
            return response.status, data
        finally:
            connection.close()

    def events(self, server, *, query="", token="secret"):
        connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
        self.addCleanup(connection.close)
        headers = {"Authorization": f"Bearer {token}"} if token else {}
        connection.request("GET", "/api/events" + query, headers=headers)
        response = connection.getresponse()
        self.addCleanup(response.close)
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream; charset=utf-8")
        self.assertEqual(response.getheader("Cache-Control"), "no-store")
        self.assertEqual(response.getheader("Connection"), "keep-alive")
        self.assertEqual(response.getheader("X-Accel-Buffering"), "no")
        return response

    def next_snapshot(self, stream):
        line = stream.readline()
        while line == b": keepalive\n":
            self.assertEqual(stream.readline(), b"\n")
            line = stream.readline()
        self.assertEqual(line, b"event: snapshot\n")
        data = stream.readline()
        self.assertTrue(data.startswith(b"data: "))
        self.assertEqual(stream.readline(), b"\n")
        return json.loads(data[6:])

    def test_cache_reuses_workflow_and_invalidates_on_source_context_and_telemetry(self):
        counter = self.project / "imports.txt"
        factory_counter = self.project / "factories.txt"
        source = f"""
from pathlib import Path
from kgraph_runner import TaskNode
with Path({str(counter)!r}).open("a") as count:
    count.write("x")

def build_graph(ctx):
    with Path({str(factory_counter)!r}).open("a") as count:
        count.write("x")
    return [TaskNode(node_id, "code", lambda ctx, attempt: None, lambda ctx: "")
            for node_id in ("work", "mode_a" + ctx["suffix"])]
"""
        self.workflow.write_text(source, encoding="utf-8")
        stamp = (self.workflow.stat().st_mtime_ns // 10**9) * 10**9
        os.utime(self.workflow, ns=(stamp, stamp))
        py_compile.compile(str(self.workflow), doraise=True)
        context = self.project / "context.json"
        write_json(context, {"suffix": ""})
        current = cached_town_snapshot(self.workflow, self.state, self.report, ctx_file=context)
        with ThreadPoolExecutor(max_workers=4) as executor:
            snapshots = list(executor.map(lambda _: current(), range(8)))
        initial = snapshots[0]
        self.assertTrue(all(other == initial for other in snapshots))
        self.assertEqual(counter.read_text(), "x")
        self.assertEqual(factory_counter.read_text(), "x")
        initial["nodes"].clear()
        self.assertEqual(current()["nodes"][1]["id"], "mode_a")

        self.write_node(status="failed", cache_miss_reason="input_changed")
        refreshed = current()
        self.assertEqual(refreshed["status"], "failed")
        self.assertEqual(refreshed["nodes"][0]["cache_miss_reason"], "input_changed")
        self.assertEqual(counter.read_text(), "x")
        self.assertEqual(factory_counter.read_text(), "x")
        update_node_progress(self.state, "work", item_id="check", status="running")
        self.assertEqual(current()["nodes"][0]["sub_progress"]["running"], 1)

        write_json(context, {"suffix": "_ctx"})
        self.assertEqual(current()["nodes"][1]["id"], "mode_a_ctx")
        self.assertEqual(counter.read_text(), "x")
        self.assertEqual(factory_counter.read_text(), "xx")

        self.workflow.write_text(source.replace("mode_a", "mode_b"), encoding="utf-8")
        os.utime(self.workflow, ns=(stamp + 10**8, stamp + 10**8))
        self.assertEqual(current()["nodes"][1]["id"], "mode_b_ctx")
        self.assertEqual(counter.read_text(), "xx")
        self.assertEqual(factory_counter.read_text(), "xxx")

    def test_cache_tracks_packets_and_gate_answers_without_changing_primary_packet(self):
        first = self.state.with_name("first.json")
        second = self.state.with_name("second.json")
        outside = self.project / "outside.json"
        write_json(outside, {"title": "Outside"})
        self.write_node(status="awaiting_subagents", kind="gate")
        write_json(self.report, {"status": "awaiting_subagents", "packet_path": str(first),
                                 "packet_paths": [str(first), str(outside), str(second)]})
        current = cached_town_snapshot(state_path=self.state, report_path=self.report)
        self.assertIsNone(current()["attention"]["packet_path"])
        write_json(first, {"question": "Continue?", "output_path": "answer.json"})
        self.assertEqual(current()["attention"]["packet_path"], str(first))
        self.assertNotIn("packet_paths", current()["attention"])
        write_json(second, {"question": "Continue with the second item?"})
        attention = current()["attention"]
        self.assertEqual(attention["packet_paths"], [str(first), str(second)])
        self.assertEqual(attention["packet_path"], str(first))
        answer = first.with_name("answer.json")
        write_json(answer, {"resolved": True, "summary": "Reviewed"})
        self.assertEqual(current()["attention"]["input_gate"]["submitted"]["summary"], "Reviewed")

    def test_cache_notices_replaced_state_with_same_size_and_mtime(self):
        self.write_node(status="failed")
        current = cached_town_snapshot(state_path=self.state, report_path=self.report)
        self.assertEqual(current()["status"], "failed")
        original = self.state.stat()
        replacement = self.state.with_name("replacement.json")
        replacement.write_text(self.state.read_text().replace('"failed"', '"passed"'), encoding="utf-8")
        self.assertEqual(replacement.stat().st_size, original.st_size)
        os.utime(replacement, ns=(original.st_atime_ns, original.st_mtime_ns))
        replacement.replace(self.state)
        self.assertEqual(current()["status"], "passed")

    def test_auth_uses_bounded_request_framing(self):
        with self.serve() as (server, _):
            for headers, expected in (
                (b"Content-Length: bad\r\n", b"400"),
                (b"Content-Length: -1\r\n", b"400"),
                (b"Content-Length: 0\r\nContent-Length: 0\r\n", b"400"),
                (b"Content-Length: 0\r\nTransfer-Encoding: chunked\r\n", b"400"),
                (b"Content-Length: 4097\r\n", b"413"),
                (b"", b"400"),
                (b"Content-Length: 12\r\n", b"302"),
            ):
                with self.subTest(headers=headers), socket.create_connection(
                        ("127.0.0.1", server.server_port), timeout=3) as conn:
                    conn.sendall(b"POST /auth HTTP/1.1\r\nHost: localhost\r\n" + headers + b"\r\ntoken=secret")
                    with conn.makefile("rb") as response:
                        self.assertEqual(response.readline().split()[1], expected)

    def test_events_auth_heartbeat_change_and_shutdown(self):
        with patch("kgraph_runner.server._EVENT_KEEPALIVE_SECONDS", .15):
            with self.serve() as (server, _):
                self.assertEqual(self.request(server, "/api/events", token=None)[0], 401)
                stream = self.events(server)
                self.assertEqual(self.next_snapshot(stream)["status"], "running")
                # The provider produces a new generated_at on every call.
                self.assertEqual(stream.readline(), b": keepalive\n")
                self.assertEqual(stream.readline(), b"\n")
                self.write_node(status="passed")
                self.assertEqual(self.next_snapshot(stream)["status"], "passed")

                stop = Thread(target=server.shutdown, daemon=True)
                stop.start()
                stop.join(1)
                self.assertFalse(stop.is_alive(), "An open event stream blocked shutdown")
                remaining = stream.read()
                self.assertIn(remaining, (b"", b": keepalive\n\n"))

    def test_hub_events_select_run_and_scrub_read_only_gate(self):
        remote = ingest_remote_run({"run_id": "remote-demo", "source_host": "other-host"},
                                   {"title": "Remote", "status": "running", "nodes": [],
                                    "run_id": "origin-local-id",
                                    "attention": {"input_gate": {"can_respond": True}}})
        with self.serve("hub", read_only=True) as (server, _):
            local = self.next_snapshot(self.events(server))
            self.assertNotEqual(local["title"], "Remote")
            data = self.next_snapshot(self.events(server, query=f"?run={remote['id']}"))
            self.assertEqual(data["title"], "Remote")
            self.assertEqual(data["run_id"], remote["id"])
            self.assertFalse(data["attention"]["input_gate"]["can_respond"])

    def test_read_only_hub_rejects_both_run_dismissal_routes(self):
        with self.serve("hub", read_only=True) as (server, prefix):
            error = {"error": "This shared Hero Town view is read-only"}
            self.assertEqual(self.request(server, prefix, method="DELETE"), (403, error))
            self.assertEqual(self.request(server, prefix + "/delete", method="POST"), (403, error))
            status, runs = self.request(server, "/api/runs")
            self.assertEqual(status, 200)
            self.assertEqual([run["id"] for run in runs["runs"]], [prefix.rsplit("/", 1)[1]])
            status, html = self.request(server, "/")
            self.assertEqual(status, 200)
            self.assertRegex(html, rb'id="town-options">[^<]*"readOnly"\s*:\s*true')

    def test_log_routes_read_only_recorded_local_outputs(self):
        log = self.state.with_name("worker.log")
        hashed = log.with_name("hashed.log")
        progress_log = log.with_name("progress.log")
        hidden = log.with_name("hidden.log")
        missing = log.with_name("missing.log")
        folder = log.with_name("folder")
        outside = self.project / "outside.log"
        unrecorded_same_name = log.with_name(outside.name)
        link = log.with_name("outside-link.log")
        log.write_bytes(b"x" * 512 + b"\npassword=sensitive\nlast line \xff\n")
        for path in (hashed, progress_log, hidden, outside, unrecorded_same_name):
            path.write_text("recorded\n", encoding="utf-8")
        folder.mkdir()
        link.symlink_to(outside)
        self.write_node(status="passed", outputs=[str(log), str(missing), str(folder), str(link), outside.name],
                        output_hashes={str(hashed.relative_to(self.project)): "digest"})
        update_node_progress(self.state, "work", item_id="check", outputs=[progress_log.name])

        for mode in ("single", "hub"):
            with self.subTest(mode=mode), self.serve(mode) as (server, prefix):
                endpoint = prefix + "/logs"

                def tail(path, **query):
                    return self.request(server, endpoint + "?" + urlencode({"path": path, **query}))

                status, result = tail(log, max_bytes=1)
                self.assertEqual(status, 200)
                self.assertEqual((result["path"], result["name"]), (str(log), log.name))
                self.assertTrue(result["truncated"])
                self.assertEqual(result["size_bytes"], log.stat().st_size)
                self.assertNotIn("sensitive", result["content"])
                self.assertTrue(result["content"].endswith("[redacted]\nlast line \ufffd\n"))
                self.assertEqual(len(result["content"].splitlines()[0]), 256 - len(b"\npassword=sensitive\nlast line \xff\n"))
                self.assertEqual(self.request(server, endpoint, token=None)[0], 401)
                self.assertEqual(self.request(server, endpoint)[0], 400)
                self.assertEqual(tail(log, max_bytes="invalid")[0], 400)
                self.assertEqual(tail(hashed.relative_to(self.project))[0], 200)
                self.assertEqual(tail(progress_log.name)[0], 200)
                self.assertEqual(tail(missing)[0], 404)
                for invalid in (hidden, outside, unrecorded_same_name, link, folder, "../worker.log",
                                r"..\worker.log", "nul\x00"):
                    with self.subTest(path=invalid):
                        self.assertEqual(tail(invalid)[0], 403)

    def test_log_tail_redacts_credentials_that_begin_before_cutoff(self):
        log = self.state.with_name("worker.log")
        ending = b"\n" + b"x" * 200 + b"\n"
        secret = b"test-password-" * 8
        log.write_bytes(b"log line\n" * 600 + b"password=" + secret + ending)
        self.assertTrue(log.read_bytes()[-256:].startswith(secret[-54:]))
        self.write_node(status="failed", outputs=[str(log)])
        with self.serve() as (server, prefix):
            status, result = self.request(server, prefix + "/logs?" + urlencode({"path": log, "max_bytes": 256}))
        self.assertEqual(status, 200)
        self.assertTrue(result["truncated"])
        self.assertEqual(result["content"], "[redacted]" + ending.decode())

    def test_qa_routes_read_write_and_enforce_submission_security(self):
        payload = {"case_id": "check", "manual_qa": "fail", "gap_type": "ui_regression",
                   "notes": "Text overlaps the panel", "tester": "Reviewer"}
        for mode in ("single", "hub"):
            self.checklist.unlink(missing_ok=True)
            with self.subTest(mode=mode), self.serve(mode) as (server, prefix):
                endpoint = prefix + "/qa"
                origin = f"http://127.0.0.1:{server.server_port}"
                self.assertEqual(self.request(server, endpoint, token=None)[0], 401)
                self.assertEqual(self.request(server, endpoint), (200, {"available": False}))
                self.assertEqual(self.request(server, endpoint, method="POST", payload=payload)[0], 404)
                init_qa_checklist(self.checklist, batch_id="batch", cases=[{"id": "check", "automated_status": "pass"}])
                status, result = self.request(server, endpoint)
                self.assertEqual(status, 200)
                self.assertTrue(result["available"])
                self.assertTrue(result["can_edit"])
                self.assertEqual(result["checklist_path"], "run/state_qa/manual_qa.json")
                self.assertEqual(result["manual_qa_values"], list(MANUAL_QA_VALUES))
                self.assertEqual(result["gap_types"], [value for value in GAP_TYPES if value is not None])
                self.assertEqual(result["scorecard"]["cases"][0]["manual_qa"], "not_tested")
                self.assertEqual(self.request(server, endpoint, method="POST", payload=payload, token=None)[0], 401)
                for headers in ({"Origin": "https://foreign.invalid"}, {"Sec-Fetch-Site": "cross-site"}):
                    self.assertEqual(self.request(server, endpoint, method="POST", payload=payload, headers=headers)[0], 403)
                self.assertEqual(self.request(server, endpoint, method="POST",
                                              payload={**payload, "manual_qa": "unknown"})[0], 400)
                status, updated = self.request(server, endpoint, method="POST", payload=payload,
                                               headers={"Origin": origin, "Sec-Fetch-Site": "same-origin"})
                self.assertEqual(status, 200)
                self.assertTrue(updated["ok"])
                card = updated["scorecard"]
                self.assertEqual((card["tester"], card["counts"]["tested"]), ("Reviewer", 1))
                case, = card["cases"]
                self.assertEqual((case["manual_qa"], case["gap_type"], case["notes"]),
                                 ("fail", "ui_regression", payload["notes"]))
                self.assertEqual(json.loads(self.checklist.read_text())["cases"], [case])
                self.assertEqual(json.loads(self.checklist.with_name("qa_scorecard.json").read_text()), card)
                self.assertIn(payload["notes"], self.checklist.with_name("qa_scorecard.md").read_text())
            with self.serve(mode, read_only=True) as (server, prefix):
                endpoint = prefix + "/qa"
                self.assertFalse(self.request(server, endpoint)[1]["can_edit"])
                self.assertEqual(self.request(server, endpoint, method="POST", payload=payload)[0], 403)
        with self.serve(token=None) as (server, prefix):
            endpoint = prefix + "/qa"
            self.assertFalse(self.request(server, endpoint, token=None)[1]["can_edit"])
            self.assertEqual(self.request(server, endpoint, method="POST", payload=payload)[0], 401)

    def test_qa_checklist_precedence_and_remote_or_symlink_rejection(self):
        run = {"state_path": str(self.state)}
        outside = self.project / "private" / "manual_qa.json"
        write_json(outside, {})
        review = self.state.parent / "review" / "manual_qa.json"
        review.parent.mkdir()
        review.symlink_to(outside)
        self.assertIsNone(find_run_qa_checklist(run))
        unrelated = self.state.parent / "a" / "manual_qa.json"
        write_json(unrelated, {})
        self.assertIsNone(find_run_qa_checklist(run))
        review.unlink()
        inline = self.state.with_name("manual_qa.json")
        for path in (inline, review, self.checklist):
            write_json(path, {})
            self.assertEqual(find_run_qa_checklist(run), path)
        self.assertIsNone(find_run_qa_checklist({**run, "source": "remote"}))
        self.assertIsNone(find_run_qa_checklist({"state_path": str(outside)}))

    def test_shared_kgraph_checklists_are_scoped_to_the_run_or_its_report(self):
        root = self.state.parent.parent
        a_state, b_state = (root / f"{name}_state.json" for name in ("a", "b"))
        a_report, b_report = (root / f"{name}_report.json" for name in ("a", "b"))
        a_checklist = root / "a_state_qa" / "manual_qa.json"
        declared = root / "review" / "manual_qa.json"
        for path in (a_state, b_state, a_checklist, declared, root / "manual_qa.json"):
            write_json(path, {})
        a = {"state_path": str(a_state), "report_path": str(a_report)}
        b = {"state_path": str(b_state), "report_path": str(b_report)}
        self.assertEqual(find_run_qa_checklist(a), a_checklist)
        self.assertIsNone(find_run_qa_checklist(b))
        (root / "b_state_qa").symlink_to(a_checklist.parent, target_is_directory=True)
        self.assertIsNone(find_run_qa_checklist(b))

        write_json(a_report, {"qa_checklist_path": str(declared.relative_to(self.project))})
        self.assertEqual(find_run_qa_checklist(a), declared)
        self.assertIsNone(find_run_qa_checklist(b))
        outside = self.project / "outside.json"
        write_json(outside, {})
        alias = root / "alias.json"
        alias.symlink_to(declared)
        for unsafe in (outside, alias, declared.parent):
            with self.subTest(unsafe=unsafe):
                write_json(a_report, {"qa_checklist_path": str(unsafe)})
                self.assertEqual(find_run_qa_checklist(a), a_checklist)


if __name__ == "__main__":
    unittest.main()
