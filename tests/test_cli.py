from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Event, Thread
from time import monotonic, sleep
import unittest
from unittest.mock import patch
from urllib.request import Request, urlopen

from kgraph_runner import build_town_snapshot
from kgraph_runner._files import read_object, write_json
from kgraph_runner.cli import main
from kgraph_runner.network import fetch_remote_response, push_snapshot
from kgraph_runner.qa_metrics import init_qa_checklist
from kgraph_runner.registry import list_runs
from kgraph_runner.server import create_hub_server, ingest_remote_run


PLAN_WORKFLOW = """
from pathlib import Path
from kgraph_runner import TaskNode

output = Path(__file__).parent / 'task.txt'
DEFAULT_CTX = {'version': 'default'}

def create(ctx):
    def write(ctx, attempt):
        output.write_text(ctx['version'])

    return [TaskNode('first', 'code', write, lambda ctx: ctx['version'],
                     lambda ctx: [output], resource='editor'),
            TaskNode('second', 'verifier', lambda ctx, attempt: None, lambda ctx: 'fixed',
                     lambda ctx: [], deps=('first',))]
"""


GATE_WORKFLOW = """
import json
from pathlib import Path
from kgraph_runner import subagent_gate

root = Path(__file__).parent / '.kgraph'

def build_graph(ctx):
    approval = root / 'approval.json'
    photo = root / 'evidence' / 'screenshot.png'
    gates = [subagent_gate('confirm', packet_path=root / 'confirm_packet.json', artifact_paths=[approval],
                           packet_fn=lambda ctx: {'approval_path': approval.name,
                                                  'approved_fields': {'approve_visuals': True}},
                           input_hash_fn=lambda ctx: 'review',
                           verify_fn=lambda ctx, paths: (json.loads(approval.read_text()).get('approved') is True,
                                                        'unapproved', 'Approval required'))]
    if ctx.get('parallel'):
        gates.append(subagent_gate('evidence', packet_path=photo.with_name('evidence_packet.json'),
                                   artifact_paths=[photo], packet_fn=lambda ctx: {'outputs': {'smoke': photo.name}},
                                   input_hash_fn=lambda ctx: 'evidence'))
    return gates
"""


class CLITests(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.project = Path(directory.name).resolve()
        self.home = self.project / 'home'
        self.state = self.project / '.kgraph' / 'state.json'
        self.report = self.state.with_name('report.json')
        self.output = self.state.with_name('approval.json')
        self.workflow = self.project / 'workflow.py'
        self.token = 'test-token'
        env = patch.dict(os.environ, {'KGRAPH_HOME': str(self.home), 'KGRAPH_HUB_URL': '',
                                      'KGRAPH_HUB_TOKEN': '', 'KGRAPH_TESTER': ''})
        env.start()
        self.addCleanup(env.stop)

    def cli(self, *args):
        with redirect_stdout(StringIO()) as out, redirect_stderr(StringIO()) as err:
            code = main([str(arg) for arg in args])
        return code, out.getvalue(), err.getvalue()

    def run_args(self):
        return (self.workflow, '--state', self.state, '--report', self.report)

    def hub(self):
        server = create_hub_server(port=0, token=self.token)
        thread = Thread(target=lambda: server.serve_forever(poll_interval=0.02), daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join, 3)
        self.addCleanup(server.shutdown)
        return f'http://127.0.0.1:{server.server_address[1]}'

    def request(self, url, payload=None):
        headers = {'Authorization': f'Bearer {self.token}'}
        data = None
        if payload is not None:
            headers['Content-Type'] = 'application/json'
            data = json.dumps(payload).encode()
        with urlopen(Request(url, data=data, headers=headers), timeout=3) as response:
            return json.load(response)

    def remote_gate(self, hub, run_id):
        snapshot = build_town_snapshot(None, self.state, self.report)
        run = push_snapshot(hub, self.token, {'run_id': run_id, 'source_host': 'origin-machine'}, snapshot)
        self.assertEqual(run['id'], run_id)
        return snapshot['attention']['input_gate']

    def queue(self, hub, run_id, gate, **decision):
        result = self.request(f'{hub}/api/runs/{run_id}/respond', {
            'node_path': gate['node_path'], 'packet_hash': gate['packet_hash'], **decision,
        })
        self.assertEqual(result['delivery'], 'remote_queue')
        return result

    def wait_for(self, condition):
        deadline = monotonic() + 3
        while monotonic() < deadline:
            if value := condition():
                return value
            sleep(0.01)
        self.fail('Timed out waiting for the test workflow')

    def test_qa_record_scorecard_and_history_in_text_and_json(self):
        checklist = self.project / 'manual.json'
        history = self.project / 'history.jsonl'
        init_qa_checklist(checklist, batch_id='beta', cases=[
            {'id': 'login', 'title': 'Sign in', 'platform': 'iOS', 'automated_status': 'pass'},
            {'id': 'map', 'title': 'Open map', 'platform': 'Android', 'automated_status': 'pass'},
        ])
        code, out, err = self.cli('qa', 'record', checklist, '--case', 'login', '--result', 'pass',
                                  '--attempt', 2, '--tester', 'Andrea', '--record-history', '--history', history)
        self.assertEqual((code, err), (0, ''))
        self.assertIn('Recorded pass for login (attempt 2)', out)
        self.assertTrue(checklist.with_name('qa_scorecard.md').is_file())
        self.assertTrue(history.is_file())

        code, out, err = self.cli('qa', 'record', checklist, '--case', 'map', '--result', 'fail',
                                  '--gap', 'ui_regression', '--notes', 'Map tiles blank', '--scorecard', '--json')
        self.assertEqual((code, err), (0, ''))
        record = json.loads(out)
        self.assertEqual(record['tester'], 'Andrea')
        self.assertEqual(record['cases'][1]['notes'], 'Map tiles blank')
        self.assertEqual(record['cases'][1]['gap_type'], 'ui_regression')
        self.assertEqual(record['scorecard']['counts']['failed'], 1)

        json_out, md_out = self.project / 'custom.json', self.project / 'custom.md'
        code, markdown, err = self.cli('qa', 'scorecard', checklist, '--md-out', md_out, '--json-out', json_out,
                                       '--record-history', '--history', history)
        self.assertEqual((code, err), (0, ''))
        self.assertIn('Manual QA Pass %', markdown)
        self.assertEqual(markdown, md_out.read_text())
        code, out, err = self.cli('qa', 'scorecard', checklist, '--json')
        self.assertEqual((code, err), (0, ''))
        self.assertEqual(json.loads(out), read_object(json_out))

        code, out, err = self.cli('qa', 'history', '--history', history, '--json')
        self.assertEqual((code, err), (0, ''))
        totals = json.loads(out)
        self.assertEqual((totals['batches'], totals['tested'], totals['manual_pass_pct']), (1, 2, 50.0))
        self.assertEqual((totals['one_shot_pct'], totals['within_two_attempts_pct']), (0, 50.0))
        self.assertEqual(totals['gap_breakdown'], {'ui_regression': 1})
        code, out, err = self.cli('qa', 'history', '--history', history)
        self.assertEqual((code, err), (0, ''))
        self.assertIn('Manual pass  50.0%', out)
        self.assertIn('Passed 1 | Failed 1 | Blocked 0', out)

    def test_plan_is_read_only_and_predicts_cache_and_retry(self):
        self.workflow.write_text(PLAN_WORKFLOW)
        config = self.project / 'context.json'
        write_json(config, {'version': 'from-file'})
        args = (self.workflow, '--state', self.state, '--factory', 'create', '--ctx-file', config, '--until', 'first')
        code, out, err = self.cli('plan', *args)
        self.assertEqual((code, err), (0, ''))
        self.assertEqual(out.splitlines()[0].split(), ['Path', 'Kind', 'Status', 'Reason', 'Resource'])
        self.assertEqual(out.splitlines()[1].split(), ['first', 'code', 'will_run', 'no_prior_state', 'editor'])
        self.assertFalse(self.state.parent.exists())
        self.assertFalse((self.project / 'task.txt').exists())

        code, _, err = self.cli('run', *args, '--report', self.report, '--quiet')
        self.assertEqual((code, err), (0, ''))
        self.assertEqual((self.project / 'task.txt').read_text(), 'from-file')
        original_state = self.state.read_bytes()
        for options, expected in [((), ('cached', 'fresh')), (('--retry', 'first'), ('will_run', 'retry_requested')),
                                  (('--ctx', '{"version":"inline"}'), ('will_run', 'input_hash_changed')),
                                  (('--force',), ('will_run', 'forced'))]:
            with self.subTest(options=options):
                code, out, err = self.cli('plan', *args, '--json', *options)
                self.assertEqual((code, err), (0, ''))
                entry, = json.loads(out)
                self.assertEqual((entry['status'], entry['reason']), expected)
                self.assertEqual(self.state.read_bytes(), original_state)

    def test_run_report_includes_declared_qa_checklist_path(self):
        checklist = self.project / 'qa' / 'manual.json'
        relative = os.path.relpath(checklist)
        self.workflow.write_text(PLAN_WORKFLOW + f'''
create_tasks = create

def build_graph(ctx):
    nodes = create_tasks(ctx)
    nodes[0].qa_checklist_path = Path({relative!r})
    return nodes
''')
        code, _, err = self.cli('run', *self.run_args(), '--quiet')
        self.assertEqual((code, err), (0, ''))
        self.assertEqual(read_object(self.report)['qa_checklist_path'], str(checklist))

    def test_run_streams_node_transitions_and_final_report_to_hub(self):
        self.workflow.write_text(GATE_WORKFLOW)
        base = self.hub()
        snapshots = []

        def record(meta, snapshot):
            snapshots.append((meta, snapshot))
            return ingest_remote_run(meta, snapshot)

        with patch('kgraph_runner.server.ingest_remote_run', side_effect=record):
            code, _, err = self.cli('run', *self.run_args(), '--hub-url', base, '--hub-token', self.token, '--quiet')
        self.assertEqual(code, 4, err)
        self.assertEqual(len({meta['run_id'] for meta, _ in snapshots}), 1)
        started, paused, final = [snapshot for _, snapshot in snapshots]
        self.assertEqual(started['event']['event'], 'node_start')
        self.assertEqual(started['nodes'][0]['status'], 'running')
        self.assertEqual(paused['event']['event'], 'node_awaiting_subagents')
        self.assertEqual(final['attention']['input_gate']['node_path'], 'confirm')
        self.assertEqual(final['status'], 'awaiting_subagents')
        hub_snapshot = self.request(f"{base}/api/snapshot?run={snapshots[0][0]['run_id']}")
        self.assertEqual(hub_snapshot['attention']['input_gate'], final['attention']['input_gate'])

    def test_failed_live_push_does_not_abort_workflow(self):
        self.workflow.write_text(PLAN_WORKFLOW)
        code, _, err = self.cli('run', *self.run_args(), '--factory', 'create', '--quiet',
                                '--hub-url', self.hub(), '--hub-token', 'wrong-token')
        self.assertEqual(code, 0, err)
        self.assertIn('could not report to Hero Town Hub', err)
        self.assertEqual(read_object(self.report)['status'], 'passed')

    def test_slow_hub_does_not_block_the_graph_and_final_snapshot_replaces_pending_live_event(self):
        self.workflow.write_text(GATE_WORKFLOW)
        sending, release, final_sent = Event(), Event(), Event()
        snapshots = []

        def publish(snapshot, *_args, run_id):
            snapshots.append(snapshot)
            if len(snapshots) == 1:
                sending.set()
                self.assertTrue(release.wait(5), 'Test did not unblock the slow Hub')
            else:
                final_sent.set()
            return {'id': run_id}

        with patch('kgraph_runner.cli._publish_snapshot', side_effect=publish), ThreadPoolExecutor(1) as executor:
            run = executor.submit(self.cli, 'run', *self.run_args(), '--hub-url', 'https://hub.example', '--quiet')
            try:
                self.assertTrue(sending.wait(2))
                code, _, err = run.result(timeout=2)
                self.assertEqual(code, 4, err)
                self.assertEqual(read_object(self.report)['status'], 'awaiting_subagents')
                self.assertEqual(len(snapshots), 1)
            finally:
                release.set()
            self.assertTrue(final_sent.wait(2))
        started, final = snapshots
        self.assertEqual(started['event']['event'], 'node_start')
        self.assertEqual(final['status'], 'awaiting_subagents')
        self.assertNotIn('event', final)

    def test_watch_resumes_on_each_parallel_packet_output(self):
        self.workflow.write_text(GATE_WORKFLOW)
        confirm_packet = self.state.with_name('confirm_packet.json')
        screenshot = self.state.parent / 'evidence' / 'screenshot.png'
        evidence_packet = screenshot.with_name('evidence_packet.json')

        def answer_packets():
            def pending():
                return read_object(self.report).get('packet_paths') if self.report.is_file() else None

            self.assertCountEqual(self.wait_for(pending), [str(confirm_packet), str(evidence_packet)])
            write_json(self.output, {'approved': True})
            self.wait_for(lambda: pending() == [str(evidence_packet)])
            screenshot.write_bytes(b'screenshot')

        with ThreadPoolExecutor(max_workers=1) as executor:
            responder = executor.submit(answer_packets)
            code, out, err = self.cli('run', *self.run_args(), '--ctx', '{"parallel":true}', '--max-parallel', 2,
                                      '--watch', '--watch-interval', '0.01', '--watch-timeout', '4')
            responder.result()
        self.assertEqual(code, 0, err)
        self.assertIn('Waiting packets:', out)
        self.assertIn(str(confirm_packet), out)
        self.assertIn(str(evidence_packet), out)
        self.assertEqual(read_object(self.report)['status'], 'passed')
        self.assertEqual({entry['status'] for entry in read_object(self.state)['nodes'].values()}, {'passed'})

    def test_watch_times_out_and_rejects_invalid_options(self):
        self.workflow.write_text(GATE_WORKFLOW)
        args = self.run_args()
        code, _, err = self.cli('run', *args, '--watch', '--quiet', '--watch-interval', '0.5', '--watch-timeout', '0.02')
        self.assertEqual(code, 4)
        self.assertIn('watch timed out', err)
        self.assertEqual(read_object(self.report)['status'], 'awaiting_subagents')
        for option, value in [('--watch-interval', '0'), ('--watch-interval', 'nan'),
                              ('--watch-timeout', '-1'), ('--watch-timeout', 'inf')]:
            with self.subTest(option=option, value=value):
                code, _, err = self.cli('run', *args, '--watch', option, value)
                self.assertEqual(code, 1)
                self.assertIn(option, err)

    def test_watch_keeps_total_worker_budget_after_resuming(self):
        self.workflow.write_text(GATE_WORKFLOW + '''
from kgraph_runner import TaskNode

build_gates = build_graph

def build_graph(ctx):
    confirm, = build_gates(ctx)
    confirm.deps = ('first',)
    marker = root / 'second-worker-ran'
    first = TaskNode('first', 'worker', lambda *_: None, lambda _: 'first')
    second = TaskNode('second', 'worker', lambda *_: marker.touch(), lambda _: 'second', deps=('confirm',))
    return [first, confirm, second]
''')

        def approve():
            self.wait_for(lambda: self.report.is_file() and read_object(self.report)['status'] == 'awaiting_subagents')
            write_json(self.output, {'approved': True})

        with ThreadPoolExecutor(1) as executor:
            responder = executor.submit(approve)
            code, _, err = self.cli('run', *self.run_args(), '--max-workers', '1', '--watch',
                                    '--watch-interval', '0.01', '--watch-timeout', '4', '--quiet')
            responder.result()
        self.assertEqual(code, 3, err)
        report = read_object(self.report)
        self.assertEqual(report['status'], 'budget_exceeded')
        self.assertEqual((report['summary']['worker_calls_used'], report['summary']['max_worker_calls']), (1, 1))
        self.assertEqual(read_object(self.state)['nodes']['confirm']['status'], 'passed')
        self.assertFalse(self.state.with_name('second-worker-ran').exists())

    def test_watch_exits_when_pause_has_no_watchable_outputs_or_hub(self):
        self.workflow.write_text('''
from kgraph_runner import GraphFailure, TaskNode

def pause(*_):
    raise GraphFailure('review', 'No packet', awaiting_subagents=True)

GRAPH = [TaskNode('review', 'gate', pause, lambda _: 'review')]
''')
        with patch('kgraph_runner.cli._wait_for_outputs') as wait:
            code, _, err = self.cli('run', *self.run_args(), '--watch', '--quiet')
        self.assertEqual(code, 4, err)
        self.assertIn('no packet outputs or Hero Town Hub run to watch', err)
        wait.assert_not_called()

    def test_watch_detects_an_edit_to_a_preexisting_output(self):
        self.workflow.write_text(GATE_WORKFLOW)
        write_json(self.output, {'approved': False})

        def update_decision():
            self.wait_for(self.report.is_file)
            write_json(self.output, {'approved': True})

        with ThreadPoolExecutor(max_workers=1) as executor:
            responder = executor.submit(update_decision)
            code, _, err = self.cli('run', *self.run_args(), '--quiet', '--watch', '--watch-timeout', '4',
                                    '--watch-interval', '0.01')
            responder.result()
        self.assertEqual(code, 0, err)
        self.assertEqual(read_object(self.report)['status'], 'passed')

    def test_fetch_and_pull_response_for_outer_gate(self):
        self.workflow.write_text(GATE_WORKFLOW)
        code, _, err = self.cli('run', *self.run_args(), '--quiet')
        self.assertEqual(code, 4, err)
        base = self.hub()
        run_id = 'remote-review'
        gate = self.remote_gate(base, run_id)
        pull = ('pull-response', self.state, '--run-id', run_id, '--hub-url', base, '--token', self.token)
        self.assertIsNone(fetch_remote_response(base, self.token, run_id))
        with self.assertRaisesRegex(ValueError, 'HTTP 401'):
            fetch_remote_response(base, 'wrong-token', run_id)
        for url, invalid_run_id in [(base, '../review'), (base, ''), ('file:///tmp', run_id),
                                    ('http://user@localhost', run_id)]:
            with self.subTest(url=url, run_id=invalid_run_id), self.assertRaises(ValueError):
                fetch_remote_response(url, self.token, invalid_run_id)
        self.assertEqual(self.cli(*pull, '--wait', '--poll-ms', '10', '--timeout', '0.03', '--json'), (4, 'null\n', ''))

        self.queue(base, run_id, gate, approved=True, approved_fields={'approve_visuals': False})
        queued = fetch_remote_response(base, self.token, run_id)
        self.assertEqual((queued['node_path'], queued['packet_hash']), (gate['node_path'], gate['packet_hash']))
        code, out, err = self.cli(*pull, '--no-apply', '--json')
        self.assertEqual((code, err), (0, ''))
        self.assertEqual(json.loads(out), queued)
        self.assertFalse(self.output.exists())

        packet = self.state.with_name('confirm_packet.json')
        original = read_object(packet)
        write_json(packet, {**original, 'revision': 'new'})
        code, _, err = self.cli(*pull, '--apply', '--report', self.report)
        self.assertEqual(code, 1)
        self.assertIn('packet hash', err)
        self.assertFalse(self.output.exists())
        write_json(packet, original)

        code, out, err = self.cli(*pull, '--apply')
        self.assertEqual((code, err), (0, ''))
        self.assertIn('Approved', out)
        decision = read_object(self.output)
        self.assertTrue(decision['approved'])
        self.assertFalse(decision['approve_visuals'])
        self.assertEqual(self.cli('run', *self.run_args(), '--quiet')[0], 0)
        self.assertEqual(self.cli(*pull)[0], 1)
        self.assertEqual(read_object(self.output), decision)

    def test_answer_waits_for_matching_remote_subtask(self):
        write_json(self.state, {'nodes': {'release': {'kind': 'worker', 'status': 'running'}}})
        ask = ('ask', self.state, '--node', 'release', '--item', 'confirm', '--option', 'Yes')
        self.assertEqual(self.cli(*ask, '--question', 'Review the old packet?')[0], 0)
        base = self.hub()
        run_id = 'remote-subtask'
        original_gate = self.remote_gate(base, run_id)
        self.queue(base, run_id, original_gate, decisions={'q1': 'Yes'})
        self.assertEqual(self.cli(*ask, '--question', 'Review the updated packet?')[0], 0)
        gate = self.remote_gate(base, run_id)
        self.assertNotEqual(gate['packet_hash'], original_gate['packet_hash'])
        answer = ('answer', self.state, '--node', 'release', '--item', 'confirm', '--run-id', run_id,
                  '--hub-url', base, '--token', self.token, '--json')
        self.assertEqual(self.cli(*answer), (4, 'null\n', ''))

        def delayed_decision():
            sleep(0.04)
            self.queue(base, run_id, gate, decisions={'q1': 'Yes'})

        with ThreadPoolExecutor(max_workers=1) as executor:
            responder = executor.submit(delayed_decision)
            code, out, err = self.cli(*answer, '--wait', '--poll-ms', '10', '--timeout', '3')
            responder.result()
        self.assertEqual((code, err), (0, ''))
        decision = json.loads(out)
        self.assertEqual((decision['node_path'], decision['packet_hash']), ('release::confirm', gate['packet_hash']))
        self.assertEqual(read_object(self.state.parent / 'artifacts' / 'confirm_decision.json'), decision)
        self.assertEqual(self.cli('answer', self.state, '--node', 'release', '--item', 'confirm',
                                  '--hub-url', base, '--run-id', run_id, '--token', 'wrong-token'),
                         (0, 'q1: Yes\n', ''))

    def test_watch_fetches_remote_decision_and_resumes(self):
        self.workflow.write_text(GATE_WORKFLOW)
        base = self.hub()

        def remote_reviewer():
            def active_gate():
                runs = list_runs()
                if runs and runs[0]['status'] == 'awaiting_subagents':
                    run_id = runs[0]['id']
                    snapshot = self.request(f'{base}/api/snapshot?run={run_id}')
                    if gate := snapshot['attention'].get('input_gate'):
                        return run_id, gate

            run_id, gate = self.wait_for(active_gate)
            self.queue(base, run_id, gate, approved=True)

        # The origin and Hub normally have separate registries. Only register on the Hub in this test.
        with patch('kgraph_runner.cli.register_run', return_value=None), ThreadPoolExecutor(max_workers=1) as executor:
            responder = executor.submit(remote_reviewer)
            code, _, err = self.cli('run', *self.run_args(), '--watch', '--watch-interval', '0.01',
                                    '--watch-timeout', '4', '--hub-url', base, '--hub-token', self.token, '--quiet')
            responder.result()
        self.assertEqual(code, 0, err)
        self.assertEqual(read_object(self.report)['status'], 'passed')
        self.assertTrue(read_object(self.output)['approved'])


if __name__ == '__main__':
    unittest.main()
