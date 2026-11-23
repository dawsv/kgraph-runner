# KGraph Runner

A Python runner for verified workflows. KGraph runs ordered and parallel steps, nests subgraphs, caches verified output, and retries individual failed steps with bounded repair loops. It has no runtime dependencies beyond Python 3.9 or later. The K is for Kippy.

## Install

Use GitHub credentials with read access to this private repository. Pin a release tag or full commit SHA in each consuming project's dependencies.

```sh
python -m pip install 'kgraph-runner @ git+https://github.com/dawsv/kgraph-runner.git@v0.1.1'
```

## Run a workflow

Define the steps, their dependencies, and the checks for your project. Each node's input hash should cover the inputs that determine its output, including relevant files and configuration.

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

## Test

```sh
python -m pip install -e .
python -m unittest discover -s tests -v
```

The initial engine was extracted from [SmithShroom's shared runner](https://github.com/dawsv/SmithShroom/blob/2c1d08e837e5f4862ffbb94739f79cbcd351b768/Tools/GraphLibrary/engine.py).
