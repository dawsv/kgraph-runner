"""Render the Hero Town as a dependency-free HTML page."""

from functools import lru_cache
from html import escape
from importlib.resources import files
import json
from pathlib import Path
from typing import Optional
from urllib.parse import urlsplit


@lru_cache(maxsize=None)
def _load_asset(name: str) -> str:
    try:
        return files("kgraph_runner").joinpath("ui_assets").joinpath(name).read_text(encoding="utf-8")
    except FileNotFoundError:
        return (Path(__file__).with_name("ui_assets") / name).read_text(encoding="utf-8")


LAYOUT_MODES = {"grid": "2D Grid", "iso": "3D Iso", "strip": "1-Row"}

PWA_META = """  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <meta name="apple-mobile-web-app-capable" content="yes">
  <meta name="theme-color" content="#49b7fb">
  <link rel="manifest" href="/manifest.webmanifest">
  <link rel="icon" type="image/svg+xml" href="/icon.svg">"""

_EMBLEM = _load_asset("emblem.svg")

APP_ICON = f"""<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 64 64">
  <rect x="1" y="1" width="62" height="62" rx="13" fill="#49b7fb" stroke="#172033" stroke-width="2"/>
  <g transform="translate(4 4)">{_EMBLEM}</g>
</svg>"""


_STYLES = _load_asset("town.css")
_SCENERY = _load_asset("scenery.svg")
_HEROES = _load_asset("heroes.svg")
_SCRIPT = _load_asset("town.js")


def _json_script(data):
    # Escape closing tags and keep HTML's script tokenizer out of its
    # comment/escaped states.
    return json.dumps(data).replace("</", r"<\/").replace("<!--", r"\u003c!--")


def _device_menu(access_urls):
    links = []
    for entry in access_urls or ():
        url = entry["url"]
        if urlsplit(url).scheme not in ("http", "https"):
            continue
        label = escape(entry["label"])
        href = escape(url)
        address = escape(url.partition("?")[0])
        links.append(f'<li><a href="{href}" target="_blank" rel="noreferrer">{label}<span>{address}</span></a>'
                     f'<button type="button" aria-label="Copy {label} address">Copy</button></li>')
    if not links:
        return ""
    return f"""<details class="devices">
        <summary>Devices</summary>
        <div class="device-panel">
          <ul id="device-list">{''.join(links)}</ul>
          <input class="copy-fallback" id="device-fallback" aria-label="Device address" readonly hidden>
          <p class="copy-note" id="device-copy-result" role="status"></p>
        </div>
      </details>"""


def render_town_html(snapshot: dict, *, live_url: Optional[str] = None,
                     runs_url: Optional[str] = None, active_run_id: Optional[str] = None,
                     poll_ms: int = 1200, embed: bool = False, access_urls=None,
                     layout_mode: str = "grid", respond_url: Optional[str] = None,
                     events_url: Optional[str] = None, logs_url: Optional[str] = None,
                     qa_url: Optional[str] = None, read_only: bool = False) -> str:
    """Return an interactive HTML document with no external assets."""
    if poll_ms < 1:
        raise ValueError("poll_ms must be at least 1")
    if layout_mode not in tuple(LAYOUT_MODES):
        raise ValueError("layout_mode must be grid, iso, or strip")
    initial = _json_script(snapshot)
    options = _json_script({"live": live_url, "runs": runs_url, "run": active_run_id, "pollMs": poll_ms,
                            "layoutMode": layout_mode, "respond": respond_url,
                            "events": events_url, "logs": logs_url, "qa": qa_url, "readOnly": bool(read_only)})
    layouts = "".join(
        f'<button data-layout="{mode}" aria-pressed="{str(mode == layout_mode).lower()}">{label}</button>'
        for mode, label in LAYOUT_MODES.items()
    )
    title = escape(str(snapshot.get("title", "Hero Town")))
    shell = "embed" if embed else "full"
    meta = '<meta name="viewport" content="width=device-width, initial-scale=1">' if embed else PWA_META
    switcher = (
        '<nav class="run-bar" id="run-bar" aria-label="Workflow runs"></nav>'
        '<p class="run-feedback" id="run-feedback" role="status" hidden></p>'
    ) if runs_url else ""
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  {meta}
  <title>{title} | Hero Town</title>
  <style>{_STYLES}</style>
</head>
<body class="{shell}" data-theme="day">
<main class="town">
  <header class="hud">
    <div class="hud-top">
      <div class="identity">
        <svg class="emblem" viewBox="0 0 56 56" aria-hidden="true">
          {_EMBLEM}
        </svg>
        <div><p class="wordmark">Hero Town</p><h1 id="workflow-title">{title}</h1></div>
      </div>
      <div class="hud-actions">
        <span id="overall" class="overall">Pending</span>
        {_device_menu(access_urls)}
      </div>
    </div>
    {switcher}
    <div class="hud-metrics">
      <span class="chip status-passed"><span class="dot"></span>Passed <b id="count-passed">0</b></span>
      <span class="chip status-cached"><span class="dot"></span>Cached <b id="count-cached">0</b></span>
      <span class="chip status-running"><span class="dot"></span>Running <b id="count-running">0</b></span>
      <span class="chip status-failed"><span class="dot"></span>Failed <b id="count-failed">0</b></span>
      <span class="chip status-pending"><span class="dot"></span>Pending <b id="count-pending">0</b></span>
      <div class="budget">Worker Budget <progress id="budget-meter" aria-label="Worker budget" hidden></progress>
        <span id="budget-label">— / —</span></div>
    </div>
    <div class="progress-bar-row">
      <span class="progress-count" id="progress-count">0 / 0 tasks (0%)</span>
      <div class="task-progress" id="task-progress" role="progressbar" aria-label="Task completion"
        aria-valuemin="0" aria-valuemax="100" aria-valuenow="0"></div>
      <button type="button" class="now-at" id="now-at" disabled>
        <span class="now-at-label">Now At</span> <span id="now-at-text"></span>
      </button>
    </div>
  </header>
  <p class="warning" id="warning" role="status" hidden></p>
  <div class="workspace">
  <section class="attention-card" id="attention-card" aria-labelledby="attention-headline" hidden>
    <div class="attention-top">
      <span class="attention-tag" id="attention-tag" role="status"></span>
      <button type="button" id="attention-focus">Focus</button>
    </div>
    <h2 id="attention-headline"></h2>
    <p class="attention-detail" id="attention-detail" hidden></p>
    <div id="input-gate" tabindex="-1" hidden></div>
    <ul class="attention-items" id="attention-items" aria-label="Action items" tabindex="0" hidden></ul>
  </section>
  <section class="stage" aria-label="Workflow map">
    {_SCENERY}
    <div class="controls" role="group" aria-label="Map controls">
      <button class="zoom" id="zoom-in" aria-label="Zoom in">+</button>
      <button class="zoom" id="zoom-out" aria-label="Zoom out">−</button>
      <button id="fit-town">Fit Town</button>
      <div class="layout-controls" role="group" aria-label="Town layout">{layouts}</div>
      <button id="theme" aria-label="Night Patrol theme" aria-pressed="false">☾ Night Patrol</button>
      <div class="live-control" id="live-controls" hidden>
        <span class="chip live-label" id="live-badge" role="status"><span class="dot"></span><span id="live-text">Live</span></span>
        <button id="auto-refresh" aria-pressed="true">Auto-Refresh</button>
      </div>
      <nav id="drill-breadcrumbs" class="drill-breadcrumbs" aria-label="Sub-graph navigation" hidden></nav>
    </div>
    <svg id="viewport" class="viewport" aria-label="Workflow buildings and districts">
      {_HEROES}
      <g id="world"></g>
    </svg>
    <p class="empty" id="empty" hidden>No stages to display</p>
    <aside class="inspector" id="inspector" aria-labelledby="inspect-title" hidden>
      <div class="inspector-head">
        <h2 id="inspect-title"></h2>
        <div class="inspector-head-actions">
          <button type="button" id="open-subgraph" class="inspect-subgraph" hidden>Open Sub-graph ↗</button>
          <button id="close-inspector" aria-label="Close inspector">×</button>
        </div>
      </div>
      <button type="button" id="inspect-input" hidden>Jump to Input Form</button>
      <section id="inspect-subtasks" class="inspect-subtasks" aria-labelledby="inspect-subtasks-title" hidden>
        <h3 id="inspect-subtasks-title">Sub-tasks</h3>
        <progress id="inspect-subtask-progress" max="1" value="0" aria-label="Sub-task completion"></progress>
        <ul id="inspect-subtask-list" class="inspect-subtask-list"></ul>
      </section>
      <dl id="inspect-fields"></dl>
      <h3>Outputs and hashes</h3>
      <ul id="inspect-outputs"></ul>
      <section id="inspect-log" class="inspect-log" hidden aria-labelledby="inspect-log-title">
        <div class="inspect-log-head">
          <h3 id="inspect-log-title"></h3>
          <button type="button" id="close-log">Close</button>
        </div>
        <p id="inspect-log-status" role="status"></p>
        <pre id="inspect-log-content" aria-labelledby="inspect-log-title" tabindex="0" hidden></pre>
      </section>
      <div class="actions"><button id="copy-retry">Copy retry</button><button id="copy-until">Copy until</button></div>
      <p class="copy-note" id="copy-note"></p>
      <input class="copy-fallback" id="copy-fallback" aria-label="Command" readonly hidden>
      <p class="copy-note" id="copy-result" role="status"></p>
    </aside>
  </section>
  <div class="dock" id="dock">
  <section class="roster" id="roster" aria-labelledby="roster-summary">
    <h2 id="roster-summary">All Tasks (0/0)</h2>
    <div class="roster-filters" id="roster-filters" role="group" aria-label="Filter tasks">
      <button type="button" data-filter="all" aria-pressed="true">All</button>
      <button type="button" data-filter="active" aria-pressed="false">Active</button>
      <button type="button" data-filter="failed" aria-pressed="false">Failed</button>
      <button type="button" data-filter="done" aria-pressed="false">Done</button>
      <button type="button" data-filter="pending" aria-pressed="false">Pending</button>
    </div>
    <ol class="roster-list" id="roster-list" aria-label="Workflow tasks"></ol>
    <p class="roster-empty" id="roster-empty" hidden></p>
  </section>
  </div>
  </div>
  <details class="report">
    <summary>Mission Report</summary>
    <div class="report-grid">
      <section><h3>Slowest nodes</h3><ul id="slowest-nodes"></ul></section>
      <section><h3>Failure signatures</h3><ul id="failure-signatures"></ul></section>
      <section><h3>Evolution hints</h3><ul id="evolution-hints"></ul></section>
    </div>
  </details>
  <details class="qa-panel" id="qa-panel" hidden>
    <summary>Manual QA Scorecard <span class="qa-kpis" id="qa-kpis"></span></summary>
    <p class="qa-status" id="qa-status" role="status"></p>
    <ul class="qa-cases" id="qa-cases" aria-label="Manual QA cases"></ul>
  </details>
</main>
<script type="application/json" id="town-snapshot">{initial}</script>
<script type="application/json" id="town-options">{options}</script>
<script>{_SCRIPT}</script>
</body>
</html>
"""
