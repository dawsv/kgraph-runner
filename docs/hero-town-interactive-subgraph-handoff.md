# Hero Town interactive sub-graphs

Install this checkout with Python 3.9 or later. The runner uses the Python standard library. The installed `kgraph-town` command and `python -m kgraph_runner.cli` expose the same CLI.

```sh
python -m pip install -e .
python -m unittest discover -s tests -v
kgraph-town --help
```

The suite covers the engine, concurrent progress writes, input validation, authenticated HTTP delivery, remote queues, and the HTML and JavaScript for live and exported views. With Node.js on `PATH`, the interaction tests also run `node --check` on both inline scripts.

## Module map

| Module | Responsibility |
| --- | --- |
| `engine.py`, `helpers.py` | Run workflows, hash inputs, wrap command tasks, and pause for verified gate outputs |
| `_files.py`, `interactions.py` | Lock and atomically update sidecars, construct gate descriptions, and deliver decisions |
| `workflow.py`, `snapshot.py` | Load local workflows and combine graph definitions, telemetry, and progress |
| `registry.py`, `network.py` | Track local and remote runs, refresh same-host telemetry, and publish snapshots |
| `server.py`, `ui.py` | Serve the single-workflow or Hub view, route authenticated decisions, and render Hero Town |
| `cli.py` | Run, inspect, serve, render, and publish workflows and their interactions |

## Report sub-task progress

Run from the workflow's project directory. The default state and report paths are `.kgraph/<workflow-stem>_state.json` and `.kgraph/<workflow-stem>_report.json`. Run the viewer in a separate terminal.

```sh
kgraph-town run workflow.py
kgraph-town ui workflow.py --auth --open
```

Use the full parent node path and stable item IDs. A progress update merges the supplied fields into `<state-stem>_progress.json` beside the state file. Declare dependent items with repeatable `--dep` flags and record outputs with repeatable `--output` flags. Use `--clear --item ID` to remove one item or `--clear` to remove the parent entry.

```sh
kgraph-town progress .kgraph/workflow_state.json --node verify/runtime --item build --status passed
kgraph-town progress .kgraph/workflow_state.json --node verify/runtime --item smoke \
  --dep build --kind verifier --status running --progress 40 --detail "Checking the pilot"
```

The snapshot places `sub_graph` (items, edges, and status counts) and `sub_progress` (the building's completion summary) on the parent node. Double-click the building or select it and use **Open Sub-graph ↗** in the Inspector header. Use the breadcrumbs to go back. The live view polls every 1.2 seconds and keeps the current sub-graph and the draft for the current gate as progress changes.

`kgraph-town hub --auth --open` shows registered local runs. To share across machines, bind the Hub with `--lan --token "$KGRAPH_HUB_TOKEN"` and set `KGRAPH_HUB_URL` and `KGRAPH_HUB_TOKEN` on the origin. Use `kgraph-town push workflow.py` or add `--push` to `progress` or `ask`. Set `KGRAPH_HOME` to control where the registry, remote snapshots, and queued responses live.

## Ask questions and approve work

Use `subagent_gate` for an outer workflow checkpoint. The gate packet and its declared JSON output must be inside the state file's `.kgraph/` directory. The packet can provide `questions` as a list or a single `question` with optional `options` and `recommended`. Use `multi_select: true` for a list-valued answer and `allow_note: false` to restrict answers to the offered values.

```json
{
  "title": "Confirm the rollout",
  "questions": [
    {
      "id": "rollout",
      "question": "Which rollout should we use?",
      "options": ["Pilot", "Full release"],
      "recommended": "Pilot",
      "allow_note": false
    }
  ],
  "output_path": "decision.json"
}
```

Add `approved_fields` entries with `key`, `label`, and `default` for independent approval choices. Include `review_sha256` to carry the reviewed revision in the decision and `bugs` with IDs and titles to enable `rejected_bugs` on rejection. Use the gate's verifier to check the submitted decision and any revision binding. A paused run exits with code 4. After submission, run the same workflow with the same state to verify the decision and resume.

`create_histoury_bug_fix_graph` supplies those bug IDs and titles at its release review gate. Keep its work directory under the same `.kgraph/` directory as the state file to reject individual bugs from the Hub and resume only those fixes.

For an active stage, use `ask` and `answer` with the same parent path and item ID.

```sh
kgraph-town ask .kgraph/workflow_state.json --node verify/runtime --item smoke \
  --question "Which build should we smoke-test?" --option Pilot --option Candidate --recommended Pilot
kgraph-town answer .kgraph/workflow_state.json --node verify/runtime --item smoke --wait --timeout 300 --json
```

Hero Town elevates unanswered human sub-tasks to the Attention Card. **Focus** opens the sub-graph at the question. The form defaults to `Reviewer` and remembers the submitted name in that browser. `answer` exits 4 while a decision is pending or when its wait expires.

## Deliver responses

Use `POST /api/respond` for a single-workflow viewer or `POST /api/runs/<run_id>/respond` for a Hub. Set a token with `--auth` or `--token`. The browser uses the cookie set at unlock. Programmatic clients can use `Authorization: Bearer <token>`. Browser submissions must be same-origin according to `Sec-Fetch-Site` and `Origin` when supplied. Tokenless views, `ui --read-only`, and static `render` exports show the gate without a submit action.

Send the current `node_path` and `packet_hash` from the snapshot with `decisions` or `approved` and `approved_fields`. A sub-task uses `parent/path::item-id`. Include the last `submitted_at` when editing an answer to avoid overwriting a newer submission. The server rechecks the active gate against current telemetry. For local runs, the destination comes from the local packet or the sub-task ID. Paths are resolved within `.kgraph/` and JSON is written atomically.

For a run published by an origin on another machine, the Hub queues the response. The origin can retrieve it through authenticated `GET /api/runs/<run_id>/response`. Match the queued `node_path` and `packet_hash` against the origin's active gate before applying its decision. For an outer gate, deliver the decision to the validated local output path, then rerun the workflow. `kgraph-town answer` reads decisions delivered locally.
