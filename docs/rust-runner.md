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
| `gate` | `packet_path`, `packet` (object), nonempty `artifact_paths` | Writes the packet and pauses if artifacts are missing or fail verification. Optional `verify_json` checks each artifact object. `require_resolved` and `require_approved` demand the corresponding boolean be true. `require_fields` names keys that must exist with non-null values. |
| `subgraph` | `nodes` | Groups child nodes under one parent. Child paths in status and reports use the parent/child form. |
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
