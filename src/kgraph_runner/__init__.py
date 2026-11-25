"""Public API for the portable graph runner."""

from .engine import (
    GraphFailure,
    GraphRunner,
    LoopSubgraphNode,
    SplitterNode,
    SubgraphNode,
    TaskNode,
    sha256_bytes,
    sha256_file,
    sha256_json,
)
from .helpers import DEFAULT_IGNORE_DIRS, hash_globs, hash_paths, shell_task, subagent_gate
from .interactions import (
    ask_node_question,
    read_node_progress,
    read_remote_response,
    read_subtask_answer,
    submit_gate_response,
    update_node_progress,
)
from .qa_metrics import (
    aggregate_qa_history,
    compute_qa_scorecard,
    init_qa_checklist,
    record_qa_result,
    render_qa_scorecard_markdown,
    write_qa_scorecard,
)
from .registry import list_runs, register_run
from .server import create_hub_server, create_town_server
from .snapshot import build_town_snapshot
from .subgraphs import (
    create_bug_batch_graph,
    create_desktop_bug_fix_subgraph,
    create_game_regression_graph,
    create_game_regression_subgraph,
    create_histoury_bug_fix_graph,
    create_version_history_subgraph,
    find_version_history_builder,
    require_bug_id,
)
from .ui import render_town_html
from .verifiers import inspect_png, verify_png_file

__all__ = [
    "DEFAULT_IGNORE_DIRS",
    "GraphFailure",
    "GraphRunner",
    "LoopSubgraphNode",
    "SplitterNode",
    "SubgraphNode",
    "TaskNode",
    "hash_globs",
    "hash_paths",
    "sha256_bytes",
    "sha256_file",
    "sha256_json",
    "shell_task",
    "subagent_gate",
    "aggregate_qa_history",
    "compute_qa_scorecard",
    "create_bug_batch_graph",
    "create_desktop_bug_fix_subgraph",
    "create_game_regression_graph",
    "create_game_regression_subgraph",
    "create_histoury_bug_fix_graph",
    "create_version_history_subgraph",
    "find_version_history_builder",
    "init_qa_checklist",
    "inspect_png",
    "record_qa_result",
    "render_qa_scorecard_markdown",
    "require_bug_id",
    "verify_png_file",
    "write_qa_scorecard",
    "ask_node_question",
    "build_town_snapshot",
    "create_hub_server",
    "create_town_server",
    "list_runs",
    "read_node_progress",
    "read_remote_response",
    "read_subtask_answer",
    "register_run",
    "render_town_html",
    "submit_gate_response",
    "update_node_progress",
]
