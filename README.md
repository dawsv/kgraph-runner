# KGraph Runner

KGraph runs verified workflows with parallel DAG scheduling, nested subgraphs, output caching, and bounded repair loops. Use the native Rust binaries for declarative TOML or JSON workflows, or the Python v0.2.0 reference package for workflows defined in Python. Both produce run data for Hero Town. The K is for Kippy.

## Install

From this checkout, install the native `kgraph` and `kgraph-town` binaries with stable Rust and Cargo.

```sh
cargo install --path crates/kgraph-cli
```

For Python workflows, install the v0.2.0 reference package in a Python 3.9+ virtual environment. It uses only the standard library at runtime.

```sh
python -m pip install -e .
```

Both installations supply the same command names. To select Python explicitly, use `python -m kgraph_runner.cli` in the environment where you installed it.

## Run a workflow

**Native Rust.** Save this task as `workflow.toml` and run it from that directory. It verifies Cargo's output and records a run in `.kgraph/`.

```toml
[[nodes]]
id = "toolchain"
type = "shell"
command = ["cargo", "--version"]
expect_stdout_regex = "^cargo "
```

```sh
kgraph plan workflow.toml
kgraph run workflow.toml
kgraph-town ui workflow.toml
```

**Native v0.3.0 workflows**

- Find slow or unreliable nodes with `kgraph analyze`. Each run records timings, cache decisions, lock waits, and reported agent usage. [Telemetry and trace exports](docs/rust-runner.md#execution-telemetry)
- Run Codex, Claude, Agy, or a custom worker in an `agent` node. Verify its output, request an evaluator critique, and retry with evidence from the failed attempt. [Agent workflows](docs/rust-runner.md#agent-nodes-and-evaluator-retries)
- Validate JSON outputs, pass verified values to dependent nodes, and reduce parallel results in input order. Isolate workers in Git worktrees when they write to the same paths. [Contracts and splitters](docs/rust-runner.md#typed-outputs-and-parallel-reduction)
- Start with `kgraph init`, let workers claim waiting packets with expiring leases, or connect a `/goal` client to the 13 tools served by `kgraph mcp`. [Templates, workboard, and MCP](docs/rust-runner.md#templates-workboard-and-mcp)

**Python.** Define the steps, their dependencies, and the checks for your project. Each node's input hash should cover the inputs that determine its output, including relevant files and configuration.

```python
from pathlib import Path

from kgraph_runner import GraphRunner, TaskNode, sha256_json

output = Path("greeting.txt")


def write_greeting(context, attempt):
    output.write_text(f"Hello, {context['name']}!\n", encoding="utf-8")


def check_greeting(context, result):
    expected = f"Hello, {context['name']}!\n"
    if output.read_text(encoding="utf-8") == expected:
        return True, "", ""
    return False, "greeting_mismatch", "The greeting does not match the input name."


greeting = TaskNode(
    id="greeting",
    kind="code",
    run_fn=write_greeting,
    input_hash_fn=lambda context: sha256_json(context["name"]),
    outputs_fn=lambda context: [output],
    verify_fn=check_greeting,
)

GraphRunner(Path("graph-state.json")).run([greeting], {"name": "Redcap"})
```

The source tree includes [workflow helpers](src/kgraph_runner/helpers.py) for hashing, command checks, and artifact gates.
See [workflow patterns](docs/workflow-patterns.md) for reusable examples.
See the [bug-fixing subgraph handoff](docs/bug-fixing-subgraph-handoff.md) for resumable desktop fixes, Histoury review and release, and game regression suites.

## CLI and Hero Town

`kgraph` and `kgraph-town` expose the same commands.

- Use `run` to execute a workflow, `run --max-parallel N` to enable parallel siblings, and `plan` for a dry run with cache-miss reasons. Parallel gates can pause with multiple packets. Use `run --watch` to resume when a declared output appears or changes or a matching Hub response arrives.
- Use `ui` for a single workflow, `hub` for multiple runs, and `render` for a self-contained HTML export. The live views receive server-sent events (SSE) and offer an Inspector log viewer and inline QA scorecards for local runs.
- Use `push` to publish a snapshot and `pull-response` to retrieve a Hub decision. `run --hub-url` streams lifecycle snapshots as the run executes. Python integrations can observe lifecycle events through the runner's `on_event` hook.
- Use `progress`, `ask`, and `answer` for sub-task updates and decisions, `qa` for manual results and scorecards, and `status` and `report` to inspect saved results.

See the [Hero Town handoff](docs/hero-town-interactive-subgraph-handoff.md) for examples.

## Test

From the workspace root, check Rust and build the native release binaries.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
```

With the Python package installed, run its reference suite.

```sh
python -m unittest discover -s tests -v
```

The initial engine was extracted from [SmithShroom's shared runner](https://github.com/dawsv/SmithShroom/blob/2c1d08e837e5f4862ffbb94739f79cbcd351b768/Tools/GraphLibrary/engine.py).
See the [SmithShroom migration handoff](docs/SmithShroom-migration.md) for the dependency pin and verification steps.
