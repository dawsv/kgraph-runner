# Move SmithShroom to KGraph Runner

Replace SmithShroom’s bundled Python graph engine with the private `kgraph-runner` dependency. Keep the Town Event and Game Regression workflows, project CLI, verification scripts, and Unity integration in SmithShroom.

## Start from the existing checkout

Use the existing SmithShroom checkout and inspect its branch, local changes, and active Unity or build processes first. Follow the user's persistent `codex` branch rule. As of the source review, GitHub had an existing `codex/cloud-current` branch and no branch named `codex`. Check ownership and ancestry before reusing it. Preserve unrelated work and use no new checkout or worktree.

The bundled engine in [the reviewed SmithShroom commit](https://github.com/dawsv/SmithShroom/blob/2c1d08e837e5f4862ffbb94739f79cbcd351b768/Tools/GraphLibrary/engine.py) matches the engine in [KGraph Runner v0.1.1](https://github.com/dawsv/kgraph-runner/releases/tag/v0.1.1) byte for byte. The release commit is `1490ba739e8628fff571f7486ec7921f3b7684d9`. Compare the engine in the active checkout before replacing it. If it contains later fixes, carry them into the shared package and test a new release before switching.

## Install the pinned package

Add this line to the existing graph-tooling requirements. If there is no requirements file for this environment, use `Tools/GraphLibrary/requirements-kgraph.txt` and wire it into the existing setup instructions or bootstrap.

```text
kgraph-runner==0.1.1 --hash=sha256:7bb1dde08938ca41fc7cc25c1d29f7f8e8289ddee21b1364393cc10ad7ebc41a
```

From the SmithShroom root, install into the same environment that already runs its graph CLI. This example uses the dedicated requirements file and downloads the wheel into the ignored validation folder. GitHub authentication needs read access to `dawsv/kgraph-runner`.

```sh
graph_python=Validation/presentation-tools/bin/python
kgraph_wheels=Validation/kgraph-runner/v0.1.1
mkdir -p "$kgraph_wheels"
gh release download v0.1.1 --repo dawsv/kgraph-runner \
  --pattern 'kgraph_runner-0.1.1-py3-none-any.whl' --dir "$kgraph_wheels" --skip-existing
uv pip install --python "$graph_python" --no-index --no-deps --require-hashes \
  --find-links "$kgraph_wheels" -r Tools/GraphLibrary/requirements-kgraph.txt
```

If you add the pin to a shared requirements file, use its normal installer and private artifact access, or pass this single pinned dependency to the offline install above. Use the existing interpreter on each development or CI machine that runs the graph tools.

## Replace the engine implementation

1. Check references to `Tools.GraphLibrary.engine` and relative `.engine` imports in SmithShroom code, tests, and docs.
2. Replace the implementation in `Tools/GraphLibrary/engine.py` with explicit re-exports from `kgraph_runner` so existing imports keep working. The public names are `GraphFailure`, `GraphRunner`, `LoopSubgraphNode`, `SplitterNode`, `SubgraphNode`, `TaskNode`, `sha256_bytes`, `sha256_file`, and `sha256_json`.
3. Keep `Tools.GraphLibrary` as SmithShroom’s CLI entry point. Update the graph runner skill and setup docs to point to the pinned dependency for engine updates. Keep SmithShroom-specific workflow code and integration tests in place.

## Verify and push

Run the same CLI catalog and graph test suite before and after the change. After installing the dependency, add a regression check that confirms SmithShroom’s existing imports resolve to the installed classes. These commands check class identity and run the real CLI and test entry points.

```sh
graph_python=Validation/presentation-tools/bin/python
"$graph_python" -c 'from importlib.metadata import version
from Tools.GraphLibrary.engine import GraphRunner, TaskNode
from kgraph_runner import GraphRunner as PackagedRunner, TaskNode as PackagedNode
assert version("kgraph-runner") == "0.1.1"
assert GraphRunner is PackagedRunner and TaskNode is PackagedNode'
"$graph_python" -m Tools.GraphLibrary catalog
"$graph_python" -m unittest discover -s Tools/GraphLibrary/tests -v
git diff --check
```

Confirm the catalog still contains `town_event` and `game_regression`. Confirm the suite exercises the Town Event mockup and approval fixtures and the declarative graph execution and cache test. Report any skipped tests, including the Town Event topology test if its real layout spec is absent. Those checks verify the Python migration. Only report live Unity verification if the event was actually exercised in Unity.

Review the scoped diff for engine removal, dependency wiring, compatibility and documentation. Run SmithShroom’s required publication checks on the final tree and unpublished history before pushing. Report the SmithShroom branch, commit, dependency version, and test results.
