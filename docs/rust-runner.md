# Native KGraph runner

The Rust workspace builds two standalone executables, `kgraph` and `kgraph-town`. Both expose the same commands. Use either to run a declarative workflow or to inspect an existing Python v0.2.0 run in Hero Town.

## Build and install

From the repository root with stable Rust and Cargo installed, run either command.

```sh
cargo build --release --workspace
cargo install --path crates/kgraph-cli
```

The build places both executables in `target/release/` by default, with an `.exe` suffix on Windows. The install places them in Cargo's binary directory. No Python interpreter is needed to start either binary. Workflow commands and optional external package builders need their own tools. If the Python package is also installed, invoke the native binary by path or invoke Python explicitly with `python -m kgraph_runner.cli`.

## A workflow you can run

Save this as `review.toml` in an empty directory. The first task asks `rustc` for its sysroot and logs the answer. The dependent gate requests a review.

```toml
title = "Toolchain review"

[default_context]
query = "sysroot"
backtrace = "1"

[[nodes]]
id = "inspect"
type = "shell"
command = ["rustc", "--print", "{{ctx.query}}"]
env = { RUST_BACKTRACE = "{{ctx.backtrace}}" }
expect_stdout_regex = ".+"
log_path = ".kgraph/inspect.log"

[[nodes]]
id = "review"
type = "gate"
deps = ["inspect"]
packet_path = ".kgraph/review_packet.json"
packet = { title = "Review the toolchain log", output_path = "decision.json" }
artifact_paths = [".kgraph/decision.json"]
verify_json = { require_approved = true, require_fields = ["reviewer"] }
```

From the directory containing the file, preview the graph and start the run.

```sh
kgraph plan review.toml
kgraph run review.toml
```

The first run writes the packet and exits with code 4. Read the log, then save the following decision as `.kgraph/decision.json` and rerun `kgraph run review.toml`. The gate verifies the decision and the run passes. To keep the runner waiting while someone supplies the file, use `kgraph run review.toml --watch` on the first run. The packet's `output_path` tells the watcher which file to observe, relative to the packet directory.

```json
{"approved": true, "reviewer": "Sam"}
```

### Schema and interpolation

A TOML or JSON document contains exactly one of `nodes` (an array) or `preset` (a name). Optional top-level fields include `title`, `default_context` (an object), and `qa_checklist_path`. The loader starts with `default_context`, overlays the JSON object in `--ctx-file`, then overlays the JSON object supplied to `--ctx`. Each overlay replaces matching top-level keys.

Node paths such as `cwd`, `outputs`, `log_path`, and `artifact_paths` resolve from the workflow directory. A shell task's working directory also defaults there. Changing `cwd` changes where the command runs, while `outputs` still name paths from the workflow directory.

Every node has a `type` and an `id` matching `[A-Za-z0-9_][A-Za-z0-9_-]*`. Use `deps` to name dependencies at the same level and `resource` to serialize tasks that share a resource.

| Node type | Required fields beyond ID and type | Options and behavior |
| --- | --- | --- |
| `shell` | `command` | Set `cwd`, string-valued `env`, `timeout_s`, `log_path`, or `max_retries`. Check `outputs` for existence, `expect_stdout_regex` for a stdout match, and `verify_pngs` for valid, nonblank images. Each PNG entry is a path or an object with `path` and optional `min_width` and `min_height`. |
| `agent` | `adapter` and exactly one of `prompt` or `prompt_file` | Dispatches a worker, verifies its outputs, then runs an optional `evaluator`. See [agent nodes and evaluator retries](#agent-nodes-and-evaluator-retries). |
| `gate` | `packet_path`, `packet` (object), nonempty `artifact_paths` | Writes the packet and pauses if artifacts are missing or fail verification. Optional `verify_json` checks each artifact object. `require_resolved` and `require_approved` demand the corresponding boolean be true. `require_fields` names keys that must exist with non-null values. |
| `subgraph` | Exactly one of `nodes`, `from`, or `preset` | Groups child nodes under one parent. Child paths in status and reports use the parent/child form. |
| `loop_subgraph` | `nodes` and at least one of `verify_cmd` or `verify_json_path` | Runs the children, then verifies. A command must exit zero. A JSON verdict must contain a boolean `ok` and may include `error_signature` and `detail`. If both checks exist, the command runs first. `dirty_nodes` selects children to retry (default all), bounded by `max_retries` (default 2). |
| `splitter` | `template` (one node object) and either `items` (array) or `items_from_ctx` (context key naming an array) | Builds one child per item. Give children distinct IDs. `max_workers` sets a positive concurrency limit for this splitter (default 4). |

Shell and gate nodes accept `watch_paths`, `watch_globs`, and `cacheable` (default true). Watched inputs and the rendered node specification contribute to the input hash. Shell tasks default to `kind = "code"` and also accept `worker`, `verifier`, or `gate`. The `type` determines execution behavior.

Use `{{ctx.key}}` for context values and `{{item}}` or `{{item.key}}` inside a splitter template. Dotted paths can read nested object fields. Interpolation renders string values, including individual argv entries and environment values. Missing referenced values cause an error. Supply an argv array to execute the program directly and preserve spaces within each argument. A string command runs via `sh -c` on Unix or `cmd /C` on Windows. String commands, including loop and preset verification/build commands, reject template placeholders. For shell syntax, keep the script fixed and supply variable data through the shell node's `env` map.

### Split a task and verify the group

Save this as `checks.json` and run `kgraph run checks.json` with the Rust toolchain on your path. The splitter expands two compiler queries. The loop then checks Cargo and can retry the splitter once if verification fails.

```json
{
  "default_context": {"queries": ["sysroot", "target-list"]},
  "nodes": [{
    "id": "checks", "type": "loop_subgraph",
    "verify_cmd": ["cargo", "--version"], "max_retries": 1,
    "dirty_nodes": ["queries"],
    "nodes": [{
      "id": "queries", "type": "splitter", "items_from_ctx": "queries", "max_workers": 2,
      "template": {
        "id": "print-{{item}}", "type": "shell",
        "command": ["rustc", "--print", "{{item}}"], "expect_stdout_regex": ".+"
      }
    }]
  }]
}
```

### Reusable subgraphs (`from`, `@lib/...`, and nested `preset`)

Save a reusable check as `subgraphs/check.toml`. It accepts a package name and inherits the caller's working directory.

```toml
[default_context]
package = "kgraph-core"

[[nodes]]
id = "tests"
type = "shell"
command = ["cargo", "test", "-p", "{{ctx.package}}"]
```

A workflow can call the check, run a shared review stage, then start a built-in regression suite. Each stage has its own ID. Use distinct output paths or preset work directories when stages produce different artifacts. For concurrent presets, set `watch_globs` to source inputs so one stage’s temporary output files do not enter another stage’s source hash.

```toml
[[nodes]]
id = "test_subgraphs"
type = "subgraph"
from = "subgraphs/check.toml"
with = { package = "kgraph-subgraphs" }

[[nodes]]
id = "review"
type = "subgraph"
from = "@lib/review_gate"
deps = ["test_subgraphs"]

[[nodes]]
id = "smoke"
type = "subgraph"
preset = "game_regression"
deps = ["review"]
config = { suite_id = "smoke", work_dir = ".kgraph/smoke", test_cmd = ["cargo", "test", "-p", "kgraph-core"] }
```

Install `review_gate.toml` or `review_gate.json` under `$KGRAPH_HOME/subgraphs/` (default `~/.kgraph/subgraphs/`). An extensionless library name tries TOML first, then JSON. You can specify the extension or a subdirectory such as `@lib/ci/check.json`.

Relative imports resolve from the file containing `from`. The resolved file must stay within the entry workflow directory for project imports, or within the shared library for library imports. This includes symlink targets. Imports accept regular TOML or JSON files up to 2 MiB and eight nested imports. A circular import is an error.

Execution paths, including `cwd`, `outputs`, `watch_paths`, `log_path`, and `artifact_paths`, resolve from the **entry workflow directory**, including for library nodes. Imported defaults fill missing parent context keys. The caller's `with` values override both and are private to that invocation. An imported file can itself declare `nodes` or a top-level `preset`.

Splitter templates accept the same subgraph sources. For example, use `id = "check-{{item.name}}"`, `from = "subgraphs/check.toml"`, and `with = { package = "{{item.name}}" }` in a splitter's `[nodes.template]` table. Both `with` and nested preset `config` support caller context and item interpolation. Built-in presets run in their configured project or work directories and do not support splitter worktree isolation.

### Built-in presets

Each row is a complete JSON workflow. Save one as `preset.json` and inspect it with `kgraph plan preset.json`. The sample `cargo --version` commands exercise the schema. Replace them with the project's actual reproduction or test commands before using the presets for QA. Gate-based presets pause for the requested evidence.

| Preset | Example | Purpose |
| --- | --- | --- |
| `desktop_bug_fix` | `{"preset":"desktop_bug_fix","work_dir":".kgraph/fix","bug_spec":{"id":"settings","title":"Settings crash"},"repro_cmd":["cargo","--version"]}` | Verify one fix in a bounded loop. Supply at least one reproduction, build, or test command. |
| `bug_batch` | `{"preset":"bug_batch","work_dir":".kgraph/batch","bugs":[{"id":"settings","title":"Settings crash"}],"test_cmd":["cargo","--version"]}` | Build one fix subgraph per bug and a consolidated report. Individual bugs can override the shared commands. |
| `game_regression` | `{"preset":"game_regression","suite_id":"smoke","work_dir":".kgraph/suite","test_cmd":["cargo","--version"],"visual_checks":["overview"]}` | Run the test command and request visual evidence. The example expects `visual/overview.png` under the suite's work directory. |
| `version_history` | `{"preset":"version_history","work_dir":".kgraph/history","release_id":"v1"}` | Request release content, validate its manifest and media, then build and verify the ZIP. |
| `histoury_bug_fix` | `{"preset":"histoury_bug_fix","batch_id":"desktop-1","work_dir":".kgraph/release","repos":{"client":"."}}` | Coordinate bug selection, before evidence, fix/review loops, and release packaging across named repositories. |

Preset fields may also go in a `config` object (a `[config]` table in TOML). Fields inside it override fields of the same name beside `preset`.

## Execution telemetry

Native `kgraph run` appends one JSON record per execution to `telemetry.jsonl` beside the state file, normally `.kgraph/telemetry.jsonl`. Paused and failed runs are recorded too. Set `--telemetry PATH` to choose a different ledger or `--no-telemetry` to skip recording for a run.

```sh
kgraph analyze
kgraph analyze --workflow review.toml --last 20 --json
kgraph analyze --workflow review.toml --last 20 --md-out review.md --chrome-trace review.trace.json
```

For a custom ledger, pass its path as the first argument to `analyze`. The CLI filters workflow references by substring before selecting the last matching runs. The Markdown report summarizes that selection. The Chrome trace contains the **latest matching run** and opens in Chrome's trace viewer or Perfetto.

| Question | Evidence |
| --- | --- |
| Which work determines elapsed time? | Each record identifies the critical path, the dependency chain with the largest sum of execution durations. The parallelism factor divides total work by wall time, accounting for overlapping parent and child measurements. |
| Which nodes have slow outliers? | Compare each node's p50, p90, and p99 latency. Run-level wall time has p50 and p90. |
| Why does work repeat or stall? | The JSON analysis gives per-node cache-miss reason counts, top error signatures, flakiness scores, and mean resource-lock waits. Recommendations identify frequent cache misses, unstable outcomes, and contention with evidence for the suggested change. |
| Which agent attempts consume the budget? | Node and model totals aggregate reported token usage and USD cost. Failure burn counts usage from failed attempts, including attempts that a later retry repairs. |

With `kgraph-town ui review.toml` running, `GET /api/telemetry?last=20` returns `latest_run` and `analysis`. Add `workflow` for an exact workflow-reference match or `chrome_trace=1` for trace events. Hero Town reads the ledger named `telemetry.jsonl` beside the local state under `.kgraph` and returns `{"available":false}` for an empty selection.

## Agent nodes and evaluator retries

Save this as `agent.toml` and run `kgraph run agent.toml` with the configured Codex and Claude CLIs and Python 3 available. The generator writes its final answer to `note.txt`. Python verifies the length before the evaluator reviews the content.

```toml
[[nodes]]
id = "draft"
type = "agent"
adapter = "codex"
prompt = "Write one sentence explaining when to renew a KGraph workboard lease."
response_output_path = "note.txt"
verify_cmd = ["python3", "-c", "from pathlib import Path; assert len(Path('note.txt').read_text().strip()) >= 20"]
max_retries = 2

[nodes.evaluator]
adapter = "claude"
prompt = "Approve if the note tells the worker to send a heartbeat before its lease expires. Otherwise explain what to add."
```

Choose `adapter = "codex"`, `"claude"`, `"agy"`, or `"custom"`. For a built-in adapter, the object form accepts `kind`, `bin`, `model`, and `extra_args`. For a custom process, use `adapter = "custom"` and a nonempty `argv` array. Its literal arguments can contain `{{prompt}}`, `{{prompt_file}}`, `{{output_file}}`, `{{cwd}}`, or the one-based `{{attempt}}`. The adapter can return its answer on stdout or write it to the staged output file.

The runner checks the process exit and declared outputs, then applies configured checks such as `output_schema`, `verify_json`, `verify_pngs`, and `verify_cmd`. Only a passing attempt reaches the evaluator. The evaluator receives the generator's task and answer and must return a JSON verdict such as `{"approved":false,"error_signature":"missing_heartbeat","feedback":"Say when to renew."}`.

On a retry, the runner starts a fresh worker prompt with the previous critique, when present, and a node-specific `RepairContext`. The bundle carries the failure signature and detail, recent attempt history, remaining attempts, and available log tail, Git diff statistics, and changed file names. Set `log_path` to capture worker diagnostics or `prompt_artifact_path` to inspect the dispatched prompt. `max_retries` defaults to zero and counts retries after the first attempt. A repeated failure signature can open the circuit breaker earlier.

## Typed outputs and parallel reduction

On shell, gate, and agent nodes, `output_schema` checks declared `.json` files. Declare them under `outputs` for shell or agent tasks, `artifact_paths` for gates, or `response_output_path` for an agent answer. The validator supports a JSON Schema subset including types, required properties, enums, bounds, array items, and `allOf`/`anyOf`/`oneOf`. Unknown keywords are ignored. A violation identifies the JSON Pointer, for example `schema_violation:/count`. Use `export_json = { path = "counts.json", key = "counts" }` to publish parsed JSON to workflow context after verification. A dependent node can then reference `{{ctx.counts}}`.

Save this as `counts.toml` and run `kgraph run counts.toml --max-parallel 2` with Python 3. Each branch verifies an integer and exports it under the same branch-local key. The dependent check sees the sum after both branches pass.

```toml
[[nodes]]
id = "count"
type = "splitter"
items = [3, 2]
max_workers = 2

[nodes.template]
id = "part-{{item}}"
type = "shell"
command = ["python3", "-c", "import pathlib, sys; n = sys.argv[1]; pathlib.Path(n + '.json').write_text(n)", "{{item}}"]
outputs = ["{{item}}.json"]
output_schema = { type = "integer", minimum = 0 }
export_json = { path = "{{item}}.json", key = "count" }

[nodes.reduce]
from_ctx_key = "count"
op = "sum"
to_ctx = "total"

[[nodes]]
id = "check"
type = "shell"
deps = ["count"]
command = ["python3", "-c", "import sys; assert int(sys.argv[1]) == 5", "{{ctx.total}}"]
```

A reducer reads exactly one source per item, either a branch export (`from_ctx_key`) or a JSON file (`from_output`, for example `"{{item}}.json"`). It publishes the result to `to_ctx` in original item order, regardless of completion order.

| `op` | Result |
| --- | --- |
| `collect_list` / `unique_list` | All input values in order / the first occurrence of each distinct value. |
| `merge_object` | One object. Later items take precedence for shared keys. |
| `sum` | The numeric sum. |
| `concat` | Joined strings or arrays flattened by one level. Inputs must all be strings or all be arrays. |
| `first` / `last` | The first or last value, or JSON null for an empty splitter. |

### Worktree isolation

To run the counting example in a Git repository with one checkout per worker, insert the following immediately after `max_workers = 2`, before `[nodes.template]`. You can also set `isolation` directly on a shell or agent node. The runner verifies outputs and captures reduction inputs before cleaning up the checkout.

```toml
isolation = "worktree"

[nodes.worktree]
base_ref = "HEAD"
keep_on_failure = true
merge_policy = "none"
patch_dir = ".kgraph/patches"
```

`base_ref` defaults to `HEAD`. By default workers use detached worktrees under `.kgraph/worktrees` relative to the workflow directory. Set `branch_prefix` to create named worker branches. `keep_on_failure` defaults to false. An optional `patch_dir` captures changed files as per-node patches. The default `merge_policy = "none"` keeps changes isolated. Select `"patch"` to apply verified changes to the source checkout, or `"fast_forward"` when the worker has committed its changes and the destination can fast-forward.

## Templates, workboard, and MCP

`kgraph init` writes `.kgraph/workflow.toml` by default. The positional template is one of `ci` (default), `agent-loop`, `swarm-splitter`, `bug-fix`, `bug-batch`, or `release`. Use `--dir` to select the project and `--output` for a TOML path relative to it. Replacing an existing workflow requires `--force`. Templates that need project commands look for `Cargo.toml`, then `package.json`, then `pyproject.toml`, and otherwise emit placeholders.

```sh
kgraph init ci --dir . --output ci.toml
kgraph plan ci.toml
kgraph run ci.toml --max-parallel 3
```

**Work stealing.** Workers sharing a run's local state can atomically claim waiting gate packets. Using the paused `review.toml` example above, one worker can take the next available packet and renew its lease while reviewing the log.

```sh
state=.kgraph/review_state.json
kgraph workboard "$state" --json
kgraph claim "$state" --worker sam --ttl 300 --json
kgraph heartbeat "$state" --node review --worker sam --ttl 300
```

The claim contains the packet, expected outputs, and lease with its node path and expiration. Omit `--node` to take any available packet, or supply it to claim a particular one. The default TTL is 300 seconds. An expired lease can be claimed by another worker. A CLI claim with no available work exits with code 4.

After the review, write the required decision and complete the lease. Completion records progress and releases ownership. Rerunning the workflow verifies the artifact. To hand unfinished work back instead, use `kgraph release "$state" --node review --worker sam --reason "Needs a second reviewer"`.

```sh
printf '%s\n' '{"approved":true,"reviewer":"Sam"}' > .kgraph/decision.json
kgraph complete "$state" --node review --worker sam --summary "Reviewed the toolchain log"
kgraph run review.toml
```

Hero Town exposes waiting packets and active leases at `GET /api/workboard`. In Hub mode the corresponding routes for a locally accessible run are `GET /api/runs/<id>/workboard` and `GET /api/runs/<id>/telemetry`.

**MCP for a /goal client.** Configure the client's stdio server command as `kgraph` with argument `mcp` and set its working directory to the project root. The server speaks newline-delimited JSON-RPC. `tools/list` returns the argument schemas for all 15 tools, and `resources/list` / `resources/read` expose `kgraph://schema`, `kgraph://subgraphs`, and `kgraph://templates`.

| Use | MCP tool names |
| --- | --- |
| Plan, execute, and inspect | `kgraph_plan`, `kgraph_run`, `kgraph_status`, `kgraph_report`, `kgraph_analyze` |
| Coordinate workers | `kgraph_workboard`, `kgraph_claim`, `kgraph_heartbeat`, `kgraph_release`, `kgraph_complete` |
| Report progress and resolve decisions | `kgraph_progress`, `kgraph_ask`, `kgraph_answer` |
| Diagnostics and workspace hygiene | `kgraph_doctor`, `kgraph_clean` |

## Loop engineering and graph controls (v0.4.0)

Declarative TOML/JSON workflows support guardrails designed specifically for reliable agentic loops:

- **Hard run budgets (`[budgets]` or CLI `--max-cost-usd`, `--max-tokens`, `--max-wall-time`, `--max-workers`):** Enforced before every node dispatch and retry attempt. Exceeding a cap exits cleanly with code 3 (`budget_exceeded`).
- **Oscillation & stagnation circuit breakers:** In addition to consecutive `A -> A` failures, loops automatically detect ping-pong cycles (`A -> B -> A`) and trip the circuit breaker (exit code 2). Set `oscillation_breaker = false` on a node or loop to opt out.
- **Transient backoff retries:** Set `transient_retries`, `retry_backoff_s`, `retry_max_backoff_s`, `retry_jitter`, and optional `transient_error_regex` so upstream rate limits (`429`/`529`) and timeouts back off exponentially without consuming `max_retries` or tripping the logic circuit breaker.
- **Conditional execution (`when`) and guaranteed cleanup (`finally`):** Any node accepts a safe deterministic `when` expression over `ctx.*`, `outputs.*`, and `item.*` (`!`, `==`, `!=`, `in`, `&&`, `||`). Top-level `finally = [...]` nodes always run after the main graph completes, receiving `ctx.__kgraph_run_status` and `ctx.__kgraph_failure`.
- **Anti-reward-hijacking (`immutable_paths` / `protect_globs`):** Snapshot SHA-256 fingerprints of protected test or config files before running an agent or loop; if modified or deleted, verification fails with `error_signature = "protected_path_modified"`.
- **Compacted retry feedback (`max_feedback_bytes`):** Bounds repair context and evaluator critiques injected into retry prompts while preserving key error lines and trailing context.
- **Workspace hygiene, schema & pings:** Use `kgraph doctor` to verify setup, `kgraph clean [--dry-run] [--all]` to prune orphaned worktrees and stale locks, `kgraph schema` to emit Draft 2020-12 JSON Schema, and `kgraph run --notify-webhook URL [--notify-secret S] [--notify-desktop]` for gate/completion alerts.

For example, a goal client can call `kgraph_run`, inspect the workboard when the run pauses, and claim a packet using the same state path. This is the claim request on the wire.

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kgraph_claim","arguments":{"state":".kgraph/review_state.json","worker":"sam","ttl":300}}}
```

## CLI and Python interoperability

The native and Python v0.2.0 CLIs share the command families below. `run` and `plan` require a definition in the executing runner's format. Python loads a `.py` factory (default `build_graph`), while Rust loads `.toml` or `.json`. The native CLI accepts `--factory` for compatibility and records it in run metadata. A declarative workflow always gets its graph from the document.

| Commands | Use |
| --- | --- |
| `plan`, `run` | Preview cache decisions or execute the graph. Both accept `--until`, repeatable `--retry`, and `--force`. On `run`, `--max-parallel` limits parallel siblings and `--max-workers` sets the total worker-attempt budget. `--watch` resumes paused gates on packet-output changes or a matching Hub decision. |
| `status`, `report` | Read a state file or report from either runner. Add `--json` for the full document. |
| `progress`, `ask`, `answer` | Update a node's sub-task progress, request a decision, or retrieve it using the state file, node path, and item ID. |
| `qa record`, `qa scorecard`, `qa history` | Record manual QA, produce a scorecard from a checklist, or inspect QA history. |
| `ui`, `render` | Serve a single live Town or export self-contained HTML. Rust can show saved telemetry for a Python workflow without importing its factory. Use explicit state and report paths to view it without a workflow file. |
| `hub`, `hub list`, `push`, `pull-response` | Serve the multi-run Hub, list registered runs, publish a snapshot, or retrieve a queued decision. `pull-response` applies the decision by default and accepts `--no-apply` to inspect it. |

For `run`, the defaults are `.kgraph/<workflow-stem>_state.json` and `.kgraph/<workflow-stem>_report.json` under the **current working directory**. Use `--state` and `--report` for custom locations. The progress sidecar appends `_progress.json` to the state file's stem, so `review_state.json` pairs with `review_state_progress.json`. Either CLI can inspect the same files or update sub-tasks for the same node paths. When translating a Python workflow, declare and verify its inputs and outputs in Rust before relying on cached results.

```sh
kgraph status .kgraph/review_state.json
kgraph report .kgraph/review_report.json --json
kgraph-town render --state .kgraph/review_state.json --report .kgraph/review_report.json --output review.html
python -m kgraph_runner.cli status .kgraph/review_state.json
```

To stream lifecycle snapshots to a Hub, start `kgraph-town hub` on the receiving machine and use `kgraph run review.toml --hub-url URL` on the sender. Either implementation can serve the Hub. `KGRAPH_HUB_URL` and `KGRAPH_HUB_TOKEN` configure the client. Use `kgraph-town hub list --json` for registered run IDs. The Hub defaults to loopback port 8765. See the [Hero Town handoff](hero-town-interactive-subgraph-handoff.md) for interactive workflows.

Native workflow exit codes are 0 for success or an intentional `--until` stop, 1 for a run error, 2 for an opened circuit breaker, 3 for an exhausted worker budget, and 4 for a pending gate. Interrupting a watch wait returns 130.

## Workspace architecture

| Crate | Responsibilities |
| --- | --- |
| [kgraph-core](../crates/kgraph-core) | Schedules DAG nodes and owns state, reports, shell tasks, gates, and QA metrics. Rayon hashes file contents in parallel, then combines SHA-256 digests in sorted path order. Unix command timeouts kill the spawned process group and reap its leader. Windows terminates the direct child. |
| [kgraph-subgraphs](../crates/kgraph-subgraphs) | Loads declarative TOML/JSON definitions and constructs the five presets. Release evidence uses native Rust image verifiers from core. JPEG and WebP checks validate image structure. PNG inspection bounds encoded and compressed data at 64 MiB each, pixels at 67,108,864, and decoded scanline data at 256 MiB before checking image content. |
| [kgraph-town](../crates/kgraph-town) | Builds snapshots, tracks progress, registers runs, and serves HTTP and SSE with Tokio and Axum. Viewers of the same feed share a poller that builds and compares snapshots on a blocking thread. A raw HTTP/1 request-header guard checks framing before Hyper normalizes it, including duplicate content lengths, transfer encodings, and the 2 MiB request-body limit. On Unix, local gate and sub-task response writes under `.kgraph` use directory descriptors, no-follow opens, and atomic rename. |
| [kgraph-cli](../crates/kgraph-cli) | Provides the shared command parser, run/watch loop, Hub client commands, and both native binary entry points. |
