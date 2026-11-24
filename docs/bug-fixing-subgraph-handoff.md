# Bug-fixing subgraph handoff

## Overview and module map

Daws or a coordinating coding agent can use these builders to run repository fixes, resume work after artifact delivery, and assemble evidence for review. The library runs on Python 3.9+ with the standard library. The coordinating agent supplies code edits, captures, and any authorized release actions.

| Module | Responsibility |
| --- | --- |
| [engine](../src/kgraph_runner/engine.py) | Run tasks, subgraphs, and repair loops. Persist state and verify cached outputs. |
| [helpers](../src/kgraph_runner/helpers.py) | Hash source inputs, run command checks, and request artifact delivery through `subagent_gate`. |
| [bug_fix](../src/kgraph_runner/subgraphs/bug_fix.py) | Triage, request a fix, run build and regression checks, and consolidate a batch. |
| [histoury_bug_fix](../src/kgraph_runner/subgraphs/histoury_bug_fix.py) | Select bugs, preserve baseline evidence, verify repairs, and collect decisions and release receipts. |
| [regression](../src/kgraph_runner/subgraphs/regression.py) | Build, run a game test suite, verify optional screenshots, and publish a report. |
| [version_history](../src/kgraph_runner/subgraphs/version_history.py) | Validate a release manifest and media, then build and verify a local ZIP. |
| [qa_metrics](../src/kgraph_runner/qa_metrics.py) | Maintain manual QA, render scorecards, and aggregate batch history. |
| [verifiers](../src/kgraph_runner/verifiers.py) | Decode PNGs, inspect transparency, and check screenshot size and visible variation. |

## Setup and verification

Run these commands in the `kgraph-runner` checkout. Use a virtual environment dedicated to the checkout if you also use a shared KGraph installation.

```sh
git checkout daniel/bug-fixing-subgraph
python -m pip install -e .
python -m unittest discover -s tests -v
```

## AI agent execution protocol

1. Choose a stable absolute work directory and state file for this run. Keep the same graph configuration, paths, and checkout across resumes. Serialize access by assigning one controller per checkout and state file.
2. Call `GraphRunner(state_path).run(nodes, ctx)`. On normal completion it returns the supplied context dictionary, which nodes may update. With an `until` checkpoint it also returns normally, setting `runner.reached_path`. A checkpoint reports `stopped` when the caller writes a report with `overall_status="passed"`.
3. Catch `GraphFailure` and read its `awaiting_subagents`, `circuit_broken`, and `budget_exceeded` flags, plus `node_path`, `reason`, and `packet_path`. When `subagent_gate` finds missing artifacts or its artifact verifier rejects them, it atomically writes or replaces the packet, raises `GraphFailure(awaiting_subagents=True, packet_path=...)`, and unwinds the run. `packet_worker` uses that gate, counts as a worker, and saves an input and artifact hash receipt after delivery. The runner persists the gate or worker node with status `awaiting_subagents`. Use the exception or the latest run report to identify the active packet because packet files can persist after successful delivery. A wave with multiple waiting siblings can surface just one packet path, so fulfill it and resume to discover the next request.
4. For `stage: apply_fix`, read `bug_spec` and the expanded `bug_spec.json` at `spec_path`. Review `repair_feedback`, any `review_feedback`, and the existing files listed in `log_paths`. Apply the smallest justified edit in `repo_dir`, run a focused check, then write `fix_report.json` at the listed output path. Use the packet's integer `revision`, record the root cause and summary, and list the changed paths relative to the repository. For other stages, follow the packet's `outputs` and `instructions`. Deliver the evidence before resuming.
5. Rebuild the same graph with a fresh `GraphRunner` pointing at the original state file and run it again. The runner reuses passed cacheable tasks when the input hash and recorded outputs still match. A failed build skips the reproduction and test commands. The bug-fix loop checkpoints normalized failure feedback, increments the requested revision when it routes to repair, and pauses for a new report. When a packet says source inputs changed but the revalidated output is byte-identical, include the current packet's `input_hash` in the delivered JSON to give it a new receipt.
6. On `circuit_broken`, `budget_exceeded`, another failure, or an unexpected exception, preserve the state and capture the blocker. Resume after a concrete change to the inputs, verifier, or explicitly chosen budget. For successful runs, inspect `final_summary.json` for one bug, `batch_report.json` for a desktop batch, `review/review_summary.json` and release receipts for a Histoury batch, or `regression_report.json` for a game suite. Check the latest run status before trusting any retained artifact.

The state file starts with `version: 3` and a `nodes` map keyed by node path, with task records such as `kind`, `status`, `input_hash`, `attempts`, and `detail`. Successful tasks record `outputs` and `output_hashes`. A task that records a check result can have status `passed` while the check artifact contains `ok: false`. Inspect the loop status and the check JSON for the verification verdict. The final summary's `attempts` is calculated from recorded repair feedback and review rounds. Engine node attempts describe a particular execution.

`write_run_report` is explicit. Pass the status, error, and packet path from the current execution. Its version 1 JSON includes `status`, `error`, `awaiting_subagents`, `packet_path`, checkpoint fields, `state_file`, `scope`, `summary`, `failure_signatures`, `slowest_nodes`, and `evolution_hints`. Waiting events are represented by the top-level status and node state. `failure_signatures` lists failed and circuit-broken trace events. For individual cache hits, inspect `runner.trace` for events with `status == "cached"`.

## Runnable examples

Save the following blocks under their indicated filenames in one scratch directory, then run each script from that directory. The scripts use separate state and work directories. Steps that require real evidence or a human decision pause and print the packet path.

### Shared driver

Save as `driver.py`. These exit codes are a convention of this example wrapper.

```python
from kgraph_runner import GraphFailure, GraphRunner


def run(nodes, work_dir, *, max_worker_calls=None):
    runner = GraphRunner(
        work_dir / "graph_state.json",
        max_worker_calls=max_worker_calls,
        max_parallel_nodes=1,
    )
    report_path = work_dir / "run_report.json"
    try:
        runner.run(nodes, {})
    except GraphFailure as exc:
        if exc.awaiting_subagents:
            status, exit_code = "awaiting_subagents", 4
        elif exc.circuit_broken:
            status, exit_code = "circuit_broken", 2
        elif exc.budget_exceeded:
            status, exit_code = "budget_exceeded", 3
        else:
            status, exit_code = "failed", 1
        runner.write_run_report(report_path, status, str(exc), exc.packet_path)
        print(status, exc.node_path, exc.reason)
        if exc.packet_path:
            print(exc.packet_path)
        return exit_code
    runner.write_run_report(report_path)
    return 0
```

### Single desktop bug

Save as `single.py` and run `python single.py`. The first run creates a local fixture and requests an edit. Set `timeout = 30` in the packet's `repo_dir/settings.py`, deliver the report, and rerun the script. The fix summary is under `.kgraph/single/bugs/desktop-42/`.

```python
from pathlib import Path
import sys

from driver import run
from kgraph_runner import create_desktop_bug_fix_subgraph

work = Path(".kgraph/single").resolve()
repo = Path("single-demo").resolve()
repo.mkdir(exist_ok=True)
settings = repo / "settings.py"
if not settings.exists():
    settings.write_text("timeout = 0\n", encoding="utf-8")

fix = create_desktop_bug_fix_subgraph(
    {"id": "desktop-42", "title": "Restore the request timeout", "expected": "Timeout is 30"},
    work_dir=work,
    repo_dir=repo,
    test_cmd=[sys.executable, "-B", "-c", "from settings import timeout; assert timeout == 30"],
    watch_globs=("settings.py",),
)
raise SystemExit(run([fix], work))
```

### Multi-bug batch

Save as `batch.py` and run `python batch.py`. For the first packet set only `timeout = 30` in `batch-demo/settings.py`, deliver its report, and resume. For the second set `retries = 3`, deliver its report, and resume again. Read `.kgraph/batch/batch_report.json` and `batch_report.md` after a passing run.

```python
from pathlib import Path
import sys

from driver import run
from kgraph_runner import create_bug_batch_graph

work = Path(".kgraph/batch").resolve()
repo = Path("batch-demo").resolve()
repo.mkdir(exist_ok=True)
settings = repo / "settings.py"
if not settings.exists():
    settings.write_text("timeout = 0\nretries = 0\n", encoding="utf-8")

nodes = create_bug_batch_graph(
    [
        {"id": "desktop-42", "title": "Restore timeout"},
        {
            "id": "desktop-43",
            "title": "Restore retries",
            "test_cmd": [sys.executable, "-B", "-c", "from settings import retries; assert retries == 3"],
        },
    ],
    batch_id="desktop-demo",
    work_dir=work,
    repo_dir=repo,
    test_cmd=[sys.executable, "-B", "-c", "from settings import timeout; assert timeout == 30"],
    watch_globs=("settings.py",),
)
raise SystemExit(run(nodes, work))
```

### Histoury review and release

Save as `histoury.py`. Run `python histoury.py /absolute/path/to/review-checkout`. The script requests a baseline before any edits. Substitute the assigned ticket, product, watch patterns, and actual project build and regression commands when using it on a release. Configure the platform that will supply the screenshots. With separate repositories, give each a key in `repos`, assign each bug a `repo`, and configure checks by repository key or bug ID. To use Admin intake, omit `bugs` and fulfill `fetch_bug_reports` with the packet's `bug_batch.json` schema.

```python
from argparse import ArgumentParser
from pathlib import Path
import sys

from driver import run
from kgraph_runner import create_histoury_bug_fix_graph

parser = ArgumentParser()
parser.add_argument("repo", type=Path)
repo = parser.parse_args().repo.resolve(strict=True)
if not repo.is_dir():
    parser.error("repo must be a directory")
work = Path(".kgraph/histoury-desktop-42").resolve()
nodes = create_histoury_bug_fix_graph(
    batch_id="desktop-42",
    product="desktop",
    work_dir=work,
    repos={"client": repo},
    bugs=[{
        "id": "app-42",
        "title": "Restore persisted settings",
        "description": "Reopening settings discards the saved value.",
        "repo": "client",
        "platform": "macOS",
        "repro_steps": ["Change the setting, save, and reopen the panel."],
        "expected": "The saved value is visible.",
        "requires_ui_capture": True,
    }],
    test_cmds={"client": [sys.executable, "-m", "unittest", "discover", "-s", "tests"]},
    watch_globs=("**/*.py",),
)
raise SystemExit(run(nodes, work))
```

1. At `capture_baseline`, run the unedited build and capture the actual affected screen. Write `media/app-42-before.png` and `evidence/before_provenance.json` using the packet paths. After delivering and verifying the fix, handle `capture_verified_after` on the same platform, viewport, and screen state. Write the matching after PNG and provenance. Preserve the original baseline on review retries. The revision in each provenance object is the capture packet revision. In each capture entry, record the actual tested source or build identifier, including a patch identifier for an uncommitted build when needed. The verifier checks PNG decoding, declared platform and paths, and that before and after bytes differ. The reviewer checks runtime provenance and visual correspondence.
2. At `author_release`, copy the matched runtime PNGs into the requested package's `media/` directory and write the two JSON files shown below. Include visual changes with real evidence and document non-visual fixes in the review. The ZIP verifier requires local before and after images for every manifest change and at least one change. For an entirely non-visual batch, use the desktop bug batch for verification and review the release plan separately. A passing package produces `version_history/desktop-42.zip`, `package_manifest.json` with the SHA-256, and build and validation logs. The library can use an explicit Portyl builder script or discover it under the user's home or current working directory. The built-in preflight and ZIP builder run when the script is absent.
3. At `user_verify_gate`, hand Daniel `review_summary.md`, the ZIP and hash, the captures, and `manual_qa.json`. The reviewer supplies `approval.json` for the current `review_sha256`. A rejection can name specific bugs in `rejected_bugs` or report failed manual QA cases. The graph retries the union of those IDs, or the whole batch if the list and manual failures are empty. It emits `review_feedback` and a higher fix revision for each selected bug. Follow the new fix and capture packets, rebuild the ZIP, and request fresh approval for the new review hash. Include the new authoring packet's `input_hash` in `author_meta.json` if your revalidated manifest and author metadata would otherwise be byte-identical to the previous delivery.
4. The ported graph emits the upload worker before the commit worker when both are approved. Use the release sequence Daniel authorizes for the batch. For Daniel's post-push Histoury closure, approve the reviewed commit and set `approve_upload_zip` to `false` so publication stays in the separate, explicitly approved post-push step. Verify the pushed SHAs on `origin/main`, move the pushed Admin fixes in the wave to `ready_for_qa` and verify persistence, and preserve Art/3DGS ownership. Use the Portyl Version History workflow to validate, preview, publish, and verify the exact reviewed ZIP under the actual signed-in Admin display name. If the graph handles an authorized upload, its worker also requires import preview and verification of persisted publication. Write receipts only after observing the corresponding action. Refresh manual QA and the scorecard after release.

### Game regression

Save as `regression.py` and run `python regression.py`. This example runs a local command check, then asks for `main-menu.png`. Supply an 8-bit, non-interlaced PNG of a running game's menu, at least 16×16 with visible variation, at the path in `visual_packet.json`. Rerun and read `.kgraph/regression/regression_report.md`. For a command-only smoke run, omit `visual_checks` before the first run. For a real suite, set `repo_dir`, `test_cmd`, and any `build_cmd` to your game checkout and checks.

```python
from pathlib import Path
import sys

from driver import run
from kgraph_runner import create_game_regression_subgraph

work = Path(".kgraph/regression").resolve()
repo = Path("game-demo").resolve()
repo.mkdir(exist_ok=True)
suite = create_game_regression_subgraph(
    "game-smoke",
    work_dir=work,
    repo_dir=repo,
    test_cmd=[sys.executable, "-c", "assert len({(0, 0), (0, 1)}) == 2; print('smoke passed')"],
    visual_checks=("main-menu",),
)
raise SystemExit(run([suite], work))
```

## Packet and artifact schemas

### Bug fix

| Artifact | Fields |
| --- | --- |
| `bugs/<id>/bug_spec.json` | The supplied bug spec plus `repo_dir`, `commands` (`build`, `repro`, `test`), `timeout_s`, `watch_globs`, planned `log_paths`, `revision`, and `repair_feedback`. The desktop builder requires a valid `id`. Include the title, observed behavior, reproduction steps, and expected behavior for the agent. |
| `bugs/<id>/fix_packet.json` | `stage` is `apply_fix`. Includes `bug_id`, original `bug_spec`, `spec_path`, check settings, existing `log_paths`, `instructions`, `input_hash`, `outputs`, `delivery_note`, `revision`, and `repair_feedback`. A review-driven request includes `review_feedback`. Each repair feedback item has `revision`, `error_signature`, and `detail`. |
| `bugs/<id>/final_summary.json` | `bug_id`, `title`, `root_cause`, `summary`, `changed_files`, `revision`, `attempts`, and existing `log_paths`. The runner writes it after verification. |
| `batch_report.json` | `fixed_count` and `bugs`, an ordered list of final summaries. |

For the first single-bug packet, deliver the following as `fix_report.json`. Use the current packet revision for every subsequent delivery. The verifier accepts an integer at or above the requested revision and requires nonblank text for `root_cause`, `summary`, and each entry in `changed_files`.

```json
{
  "revision": 0,
  "root_cause": "The default timeout was set to zero.",
  "summary": "Set the default timeout to 30 seconds.",
  "changed_files": ["settings.py"]
}
```

### Histoury evidence and release

The intake file is an object with `batch_id`, `product`, and a nonempty `bugs` list. Each entry uses the fields in the Histoury example. `repo` can be inferred when a batch has one repository. `requires_ui_capture` defaults to true. An optional `repro_cmd` can supply a bug-specific verification command.

Example before provenance. Replace the revision and capture fields with values from the packet and actual build. For `after_provenance.json`, use the after path and the field `verified_revision` in place of `source_revision`.

```json
{
  "revision": 0,
  "captures": {
    "app-42": {
      "path": "media/app-42-before.png",
      "source_revision": "<baseline build or source identifier>",
      "platform": "macOS",
      "captured_at": "<ISO 8601 timestamp>",
      "screen_state": "Settings panel reopened after saving"
    }
  }
}
```

Example `release.json` for the local source stage. Use the actual version, date, change details, and verification status. Put the packet revision and matching release ID in `author_meta.json`, for example `{"revision": 0, "release_id": "desktop-42"}`. The package manifest's fields include `schema_version`, `product`, `release_id`, `version`, `title`, `date`, `stage`, `summary`, and `changes`. Use the supported stage values `source`, `staging`, `internal`, or `production`. The service supplies the platforms from the product.

```json
{
  "schema_version": 2,
  "product": "desktop",
  "release_id": "desktop-42",
  "version": "1.0.42",
  "title": "Desktop settings fix",
  "date": "2026-11-24",
  "stage": "source",
  "summary": "Restored the saved setting.",
  "changes": [
    {
      "id": "app-42",
      "type": "fix",
      "title": "Restore persisted settings",
      "description": "The reopened panel displays the saved value.",
      "verification": "Local automated check passed. Manual QA is pending.",
      "evidence": [
        {"label": "Before", "kind": "image", "stage": "before", "url": "media/app-42-before.png"},
        {"label": "After", "kind": "image", "stage": "after", "url": "media/app-42-after.png"}
      ]
    }
  ]
}
```

Example **reviewer-authored** `review/approval.json` for a local review with release actions declined. Set both booleans explicitly because the implementation defaults omitted action fields to true. Always copy `review_sha256` from the current packet. The gate can infer it for an initial unchanged review, but requires an explicit match after rejection and rejects stale approval bytes when the review changes.

```json
{
  "approved": true,
  "approved_by": "Daniel",
  "approve_upload_zip": false,
  "approve_commit_main": false,
  "review_sha256": "<review_sha256 from user_approval_packet.json>",
  "notes": "Verified settings on the review build."
}
```

For a rejection, use `approved: false`, `rejected_by`, `review_sha256`, `notes`, and optionally `rejected_bugs` and `manual_qa`. For example, `"manual_qa": {"app-42": {"result": "fail", "gap_type": "missing_verifier", "notes": "Changing profiles still resets the value."}}`. Use the decision from Daniel as supplied.

An approved upload receipt requires `previewed: true`, `published: true`, the matching `release_id`, and a real `uploader_name` (the placeholder `User` is rejected). A commit receipt requires `committed: true` and `commits` entries with `repo`, `branch: "main"`, and `sha`, covering every touched repository in the review. The runner validates receipt fields. The worker verifies the external action and any separately authorized push.

### Manual QA

`manual_qa.json` has `version: 1`, `batch_id`, `workflow_type`, `title`, `tester`, `build_ref`, `updated_at`, and `cases`. Each case has `id`, `title`, `platform`, `kind`, `automated_status`, `ai_attempts`, `manual_qa`, `resolved_on_attempt`, `gap_type`, and `notes`. Record results as `pass`, `fail`, `blocked`, `not_tested`, or `skipped`. A pass defaults to the recorded AI attempt count, with a minimum of one.

`qa_scorecard.md` is the rendered companion to `qa_scorecard.json`. It reports counts, Manual QA Pass %, One-Shot %, ≤2-Shot %, gaps where automation passed but manual QA failed, and breakdowns by platform and case. The percentages use only manually passed or failed cases. The scorecard shows an absent rate when no cases have been manually tested. Batch, Histoury review, and regression flows generate the scorecards. For a single fix, call `write_qa_scorecard` on its checklist. The API can also refresh the artifacts after a human re-test.

```python
from pathlib import Path

from kgraph_runner import record_qa_result, write_qa_scorecard

checklist = Path(".kgraph/histoury-desktop-42/review/manual_qa.json")
record_qa_result(checklist, case_id="app-42", manual_qa="pass", notes="Re-tested the saved setting.")
write_qa_scorecard(checklist, record_history=True)
```

Run the example with the actual manual result after re-testing. With `record_history=True`, history is upserted by batch ID in `$KGRAPH_HOME/qa_history.jsonl` or `~/.kgraph/qa_history.jsonl`. Supply `history_path` to track a separate history and `aggregate_qa_history` to read the weighted rates. Where the shared CLI is installed, its `kgraph qa record` and `kgraph qa scorecard` commands can update the same files.

## Guardrails and failure handling

- Configure actual verification commands and set `watch_globs` to the source and assets that affect the fix. An omitted desktop bug watch list hashes no source files. The regression suite defaults to hashing repository inputs and excludes its generated artifacts. Pass trusted argument vectors when possible. String commands run in a shell.
- Use sensible timeouts and retry budgets. A loop can make the initial check and up to `max_retries` repair iterations. Bug, capture, and package repair state also tracks unresolved attempts across resume calls. Histoury tracks review rounds separately with `max_review_retries`. Budget exhaustion is a failure to investigate. A `max_worker_calls` cap counts executed worker nodes per runner instance, including workers that pause, while gate nodes can pause without consuming that budget.
- Preserve the original logs when comparing signatures. Desktop check failures use the stage and exit code plus a short hash of normalized diagnostics. Normalization removes volatile timestamps, durations, addresses, line numbers, and checkout paths. Two consecutive failures with the same signature trip the circuit breaker when the loop can reach a second verification. After a resume, the repair state recognizes the repeated signature and lets the loop recheck the same result so the breaker can surface it. Resolve the cause before issuing a fresh worker request.
- Keep the default `resource="repo"` for desktop fixes and preserve the order of a shared-checkout batch. Named resource locks are reentrant, in-process locks held by a single runner during node execution. They release when the graph returns or raises, including when an external worker is requested. Use an exclusive checkout or a controller lock across processes and tool calls.
- Require the Histoury `user_verify_gate` decision for upload or commit, and explicit push authorization in addition to the commit decision. For single, desktop batch, standalone packaging, and game regression builders, keep any external side effects behind a separate human approval. Use the current review hash and verify evidence, local diffs, ZIP bytes, identities, and remote state before recording release receipts.

## Audit checklist for autonomous scaling

- [ ] Run a disposable single-bug and batch fixture. Confirm initial pauses, valid delivery, a cached rerun, and source or output changes that invalidate the relevant checks.
- [ ] Force two different failures, then two equal normalized signatures with changing log noise. Confirm correct repair revisions and circuit breaker behavior. Exercise a timeout, a worker budget stop, and a resume after process exit.
- [ ] Confirm packet paths resolve to the expected work directory and code edits stay within the assigned checkout. Review commands, changed-file paths, and packet context before dispatch. Apply a controller-level budget across resumes if the workflow runs unattended.
- [ ] Exercise missing and malformed screenshot delivery on a test suite. Verify matched Histoury runtime captures, evidence hashes, ZIP contents, and rejection of a stale review. Confirm selected bugs return to repair while unaffected manual QA carries forward.
- [ ] Rehearse the release gate with both actions set to false. Verify that approval still comes from the reviewer, that each declined action is recorded as skipped, and that genuine receipts and explicit permissions are required for live actions.
- [ ] Confirm manual QA re-tests the actual reviewed or released build. Compare automated and manual results and turn recurring gaps into targeted regression coverage.
