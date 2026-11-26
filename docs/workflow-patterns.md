# Workflow patterns

## Fingerprint source inputs

Use `hash_paths` for named files and directories and `hash_globs` for selected file types. Both hash sorted file names relative to a root and the corresponding file contents. They deduplicate matches, so overlapping paths and reordered inputs produce the same digest. Missing paths contribute no files.

```python
from pathlib import Path

from kgraph_runner import DEFAULT_IGNORE_DIRS, hash_globs, hash_paths

root = Path.cwd()
source_hash = hash_paths(["src", "pyproject.toml"], root=root)
test_hash = hash_globs(
    ["tests/**/*.py"],
    root=root,
    ignore_dirs=DEFAULT_IGNORE_DIRS | {"fixtures_generated"},
)
```

Keep inputs under the root for fingerprints that survive moving the checkout. Pass `ignore_dirs` to replace the default set, or take the union to add exclusions. The defaults skip directory names such as `.git`, `.venv`, `node_modules`, `Library`, and `.kgraph`. Combine file hashes with any settings, tool versions, and other inputs that affect the result.

Both helpers default to `use_cache=True`. They reuse file digests within the process when the resolved path, inode, size, and modification time match. They rehash recently modified files. Pass `use_cache=False` to read the file contents on every call, or call `clear_hash_cache()` to discard all cached digests.

## Run a build or test gate

`shell_task` accepts an argument list, a string interpreted by the platform shell, or a callable `command(ctx)` that returns either form for each attempt. Include the resolved command and other context-dependent inputs in the task fingerprint. Nonzero exits produce stable `exit_<code>` signatures. A timeout produces a `timeout` signature. When you set `log_path`, stdout and stderr stream into the same file as they arrive, and each attempt replaces the previous log.

```python
import sys
from pathlib import Path

from kgraph_runner import GraphRunner, hash_globs, sha256_json, shell_task

command = [sys.executable, "-m", "unittest", "discover", "-s", "tests"]
log = Path(".kgraph/test.log")
tests = shell_task(
    "tests",
    command,
    input_hash_fn=lambda _ctx: sha256_json([
        command, sys.version, hash_globs(["src/**/*.py", "tests/**/*.py", "pyproject.toml"]),
    ]),
    outputs_fn=lambda _ctx: [log],
    timeout_s=120,
    log_path=log,
)
GraphRunner(Path(".kgraph/test-state.json")).run([tests], {})
```

For a build, declare the expected artifact in `outputs_fn` and use `verify_fn(ctx, completed_process)` to check it after exit zero. Return `(ok, signature, detail)` from a verifier. Use the same signature for the same cause and put variable diagnostics in the detail. The runner rechecks cached file outputs for changes.

## Preview and observe execution

`GraphRunner.plan()` previews one topological pass without running tasks or saving run state. Call it with the same nodes and context you would pass to `run()`. Task entries show `cached` or `will_run` and a reason such as `no_prior_state`, `input_hash_changed`, `missing_output:<path>`, or `output_hash_changed:<path>`. Plans use the current inputs and files, so results can change after an upstream task runs. Planning calls input hash, output path, and splitter callbacks on a shallow copy of the context. Keep those callbacks read-only. If a task's inputs are unavailable, its entry can report `inputs_unavailable`.

Pass `on_event=callback` to `GraphRunner` to observe lifecycle events. The callback receives a dictionary with an `event` name, a UTC `timestamp`, and event fields such as the node path, attempt, or duration. Events include `run_start`, `run_end`, `node_start`, `node_cached`, `node_retry`, `node_passed`, `node_failed`, and `node_awaiting_subagents`. Callbacks can run concurrently during parallel execution. Keep them thread-safe and brief. The runner ignores callback exceptions.

## Pause for work or approval

`subagent_gate` atomically writes the JSON returned by `packet_fn(ctx)` when a required artifact is missing or fails verification. Catch `GraphFailure` and check `awaiting_subagents`. Use `packet_paths` to find all waiting packets when multiple in-flight gates pause in parallel. `packet_path` still gives the first packet. Give each packet to its worker or reviewer, have them save the response at the requested path, then rerun the graph with the same state file.

This example binds approval to the hash of an existing build artifact. The reviewer supplies a JSON object with `"approved": true` and the requested `"artifact_sha256"`.

```python
import json
from pathlib import Path

from kgraph_runner import GraphFailure, GraphRunner, sha256_file, subagent_gate

bundle = Path("dist/application.zip")
approval = Path(".kgraph/approval.json")
revision = sha256_file(bundle)


def verify_approval(_ctx, paths):
    try:
        response = json.loads(paths[0].read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False, "invalid_approval", "Supply a readable JSON approval"
    if (isinstance(response, dict) and response.get("approved") is True
            and response.get("artifact_sha256") == revision):
        return True, "", "Approved"
    return False, "approval_required", "Approve the current artifact revision"


gate = subagent_gate(
    "review",
    packet_path=Path(".kgraph/review-request.json"),
    artifact_paths=[approval],
    packet_fn=lambda _ctx: {
        "action": "review",
        "artifact": bundle.as_posix(),
        "artifact_sha256": revision,
        "reply_path": approval.as_posix(),
    },
    input_hash_fn=lambda _ctx: revision,
    verify_fn=verify_approval,
)

try:
    GraphRunner(Path(".kgraph/review-state.json")).run([gate], {})
except GraphFailure as exc:
    if not exc.awaiting_subagents:
        raise
    for packet in exc.packet_paths:
        print(f"Review request at {packet}")
```

A work packet can similarly include a task description, input revision, and paths for a report or generated files. Verify those artifacts and their revision before resuming dependent nodes. A successful gate caches its verified file outputs until the input hash changes or an output is removed or edited.

## Compose bounded repair and regression checks

Use `LoopSubgraphNode` when a verifier needs evidence from multiple stages. A bug-fix graph can prepare a reproduction, apply a fix, and collect build and test evidence. A game or desktop regression graph can build once per case, run a scenario, and check its result and captured diagnostics. Have the loop verifier read the collected evidence and let its repair callback return just the child paths that need to run again. With `max_retries=2`, the loop allows an initial pass and up to two repairs. Two consecutive failures with the same signature trip the circuit breaker.

Use `SplitterNode` to run the same structure for several independent cases. Define each case's steps and callbacks in your project and pass the cases in the run context.

```python
from kgraph_runner import LoopSubgraphNode, SplitterNode


def build_case(case, _ctx):
    return LoopSubgraphNode(
        id=case["id"],
        nodes=case["steps"],
        verify_fn=case["verify"],
        repair_fn=case["repair"],
        max_retries=2,
    )


regressions = SplitterNode(
    id="regressions",
    items_fn=lambda ctx: ctx["cases"],
    child_builder_fn=build_case,
    max_workers=2,
)
```

Give each case a stable ID and its own output directory. Set `max_workers=1` for cases that modify the same checkout. Assign a common `resource` to leaf steps that need exclusive access to a tool. The loop repair callback takes `(ctx, attempt, signature, detail)` and can return `["run_scenario", "collect_evidence"]` to reuse a fresh build. Handle command failures at the task level with `shell_task` retries and repair.
