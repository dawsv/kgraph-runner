"""Render the Hero Town as a dependency-free HTML page."""

from html import escape
import json
from typing import Optional
from urllib.parse import urlsplit


LAYOUT_MODES = {"grid": "2D Grid", "iso": "3D Iso", "strip": "1-Row"}

PWA_META = """  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <meta name="apple-mobile-web-app-capable" content="yes">
  <meta name="theme-color" content="#49b7fb">
  <link rel="manifest" href="/manifest.webmanifest">
  <link rel="icon" type="image/svg+xml" href="/icon.svg">"""

_EMBLEM = """<path d="M28 4l7 8 12-1 1 12 6 9-9 7-3 12-13-2-11 5-7-10-9-6 4-12-1-10 11-4Z" fill="#172033"/>
  <path d="M26 2l7 8 12-1 1 12 6 9-9 7-3 12-13-2-11 5-7-10-9-6 4-12-1-10 11-4Z"
    fill="#ffcf54" stroke="#172033" stroke-width="2" stroke-linejoin="round"/>
  <path d="M13 28h8v-8h8v-6h8v27H13Z" fill="#fff9e6" stroke="#172033" stroke-width="2.5" stroke-linejoin="round"/>
  <path d="M18 32v4m7-11v4m0 4v4m8-17v4m0 4v4m0 4v1" fill="none" stroke="#e03a3e" stroke-width="2.6"/>"""

APP_ICON = f"""<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 64 64">
  <rect x="1" y="1" width="62" height="62" rx="13" fill="#49b7fb" stroke="#172033" stroke-width="2"/>
  <g transform="translate(4 4)">{_EMBLEM}</g>
</svg>"""


_STYLES = """
:root {
  color-scheme: light;
  font: 15px/1.45 system-ui, sans-serif;
  --ink: #172033;
  --paper: #fff8e8;
  --panel: #fffdf6;
  --surface: #e9f5ff;
  --sky-top: #49b7fb;
  --sky-bottom: #d8f7ff;
  --horizon: #c4ebf3;
  --skyline: #83c9e9;
  --skyline-far: #a7def3;
  --sun: #ffe373;
  --cloud: #fbffff;
  --green: #2bb673;
  --red: #e03a3e;
  --gold: #f5a623;
  --road: #45516a;
}
* { box-sizing: border-box; }
body { margin: 0; color: var(--ink); }
body.full { background: var(--surface); padding: 20px; }
body[data-theme=night] {
  --sky-top: #222f65; --sky-bottom: #7667b5; --horizon: #545b99; --skyline: #464f87;
  --skyline-far: #605c98; --sun: #fcf5b9; --cloud: #acafda; --road: #323952; --surface: #242c48;
}
.town { max-width: min(100%, 1920px); margin-inline: auto; }
.full .town { border: 3px solid var(--ink); border-radius: 17px; box-shadow: 4px 4px 0 var(--ink); overflow: hidden; }
.hud { background: var(--paper); padding: 12px 16px 10px; }
.hud-top,.hud-metrics,.identity,.controls,.actions { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
.hud-top { justify-content: space-between; gap: 12px; }
.identity { flex-wrap: nowrap; min-width: 0; gap: 12px; }
.emblem { width: 51px; height: 51px; flex: none; }
.wordmark { margin: 0; font-size: 11px; font-weight: 900; letter-spacing: .17em; text-transform: uppercase; color: #9c2b33; }
h1 { margin: 0; line-height: 1.05; font-size: clamp(21px, 4.5vw, 33px); font-weight: 950; text-wrap: balance; overflow-wrap: anywhere; }
.hud-metrics { margin-top: 13px; gap: 6px; }
.progress-bar-row { display: grid; grid-template-columns: auto minmax(60px, 1fr) minmax(0, max-content);
  align-items: center; gap: 8px 12px; margin-top: 10px; }
.progress-count { font-size: 12px; font-weight: 900; font-variant-numeric: tabular-nums; white-space: nowrap; }
.task-progress { display: flex; height: 16px; overflow: hidden; border: 2px solid var(--ink);
  border-radius: 7px; background: #dce6ef; box-shadow: 2px 2px 0 var(--ink); }
.progress-segment { flex: 1; min-width: 0; background: var(--status, #dce6ef); }
.progress-segment + .progress-segment { box-shadow: inset 1px 0 #17203355; }
.progress-segment.status-running { animation: progress-pulse 1.45s ease-in-out infinite; }
.now-at { min-width: 0; max-width: 360px; text-align: start; overflow-wrap: anywhere; }
.now-at-label { margin-right: 5px; font-weight: 950; text-transform: uppercase; letter-spacing: .06em; }
.now-at:not(:disabled) { background: var(--status); }
.now-at:not(:disabled):hover { background: #ffe483; }
.chip,.overall { display: inline-flex; align-items: center; gap: 5px; min-height: 24px; padding: 2px 8px;
  border: 1.5px solid var(--ink); border-radius: 6px; font-size: 11px; font-weight: 750; background: #fff; }
.chip b { font-variant-numeric: tabular-nums; font-size: 13px; }
.dot { width: 7px; height: 7px; border: 1px solid var(--ink); border-radius: 50%; background: var(--status, #b8c5d3); }
.overall { background: var(--status, #d8e3ed); text-transform: uppercase; font-weight: 900; box-shadow: 2px 2px 0 var(--ink); }
.budget { display: flex; align-items: center; flex-wrap: wrap; gap: 6px; font-weight: 750; font-size: 11px; margin-left: auto; }
.budget progress { width: 75px; height: 8px; accent-color: #dc8112; }
.hud-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.run-bar { display: flex; gap: 7px; overflow-x: auto; margin-top: 9px; padding: 3px 3px 6px; }
.run-card { display: flex; align-items: stretch; flex: 0 0 auto; max-width: min(100%, 304px); }
.run-choice { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 7px; align-items: center;
  text-align: start; min-width: 0; min-height: 44px; border-radius: 8px 0 0 8px; }
.run-choice[aria-current=true] { background: #ffe483; }
.run-names { display: grid; min-width: 0; font-size: 12px; line-height: 1.15; gap: 2px; }
.run-names > * { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.run-project,.run-choice time { font-size: 10px; font-weight: 650; }
.run-host { padding: 0 4px; border-radius: 4px; background: #dce6ef; }
.run-meta { display: grid; gap: 2px; justify-items: end; white-space: nowrap; }
.run-status { padding: 1px 4px; border: 1px solid var(--ink); border-radius: 4px;
  font-size: 9px; font-weight: 950; background: var(--status, #dce6ef); }
.run-dismiss { min-width: 34px; border-left: 0; border-radius: 0 8px 8px 0; font-size: 18px; padding-inline: 5px; }
.run-dismiss:hover { background: #ffc9c9; }
.run-bar p { margin: 5px 0; font-size: 12px; }
.run-feedback { margin: 4px; color: #98283d; font-size: 12px; }
.devices { position: relative; }
.devices > summary { cursor: pointer; list-style: none; border: 2px solid var(--ink); border-radius: 8px;
  padding: 5px 9px; font-size: 12px; font-weight: 800; background: var(--panel); box-shadow: 2px 2px 0 var(--ink); }
.devices > summary::-webkit-details-marker { display: none; }
.devices[open] > summary { background: #ffe483; }
.device-panel { position: absolute; z-index: 4; top: calc(100% + 8px); right: 0;
  width: min(400px, calc(100vw - 36px)); padding: 12px; border: 2px solid var(--ink); border-radius: 12px;
  background: var(--panel); box-shadow: 4px 4px 0 var(--ink); }
.device-panel ul { margin: 0; padding: 0; list-style: none; }
.device-panel li { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.device-panel li + li { margin-top: 8px; }
.device-panel a { font-size: 12px; color: #123a7b; font-weight: 750; overflow-wrap: anywhere; }
.device-panel a span { display: block; font-weight: 500; }
.device-panel button { min-height: 40px; }
.warning { margin: 0; padding: 6px 18px; background: #fff2b4; border-top: 2px solid var(--ink); font-size: 12px; overflow-wrap: anywhere; }
.workspace { display: grid; grid-template-columns: minmax(0, 1fr); }
.dock { min-width: 0; background: var(--paper); }
.attention-card { --attention-color: #503785; --attention-fill: #f2eaff;
  margin: 8px 10px; padding: 8px 10px; border: 2px solid var(--attention-color); border-radius: 9px;
  background: var(--attention-fill); box-shadow: 2px 2px 0 var(--attention-color); overflow-wrap: anywhere; }
.attention-card[data-kind=stuck] { --attention-color: #98283d; --attention-fill: #fff0ef; }
.attention-card[data-kind=running] { --attention-color: #825000; --attention-fill: #fff5d7; }
.attention-card[data-kind=done] { --attention-color: #1a6542; --attention-fill: #e8f9ee; }
.attention-top { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.attention-tag { color: var(--attention-color); font-size: 11px; font-weight: 950; letter-spacing: .06em; }
.attention-card[data-kind=running] .attention-tag { animation: attention-pulse 1.45s ease-in-out infinite; }
.attention-card h2 { margin: 2px 0; font-size: 15px; line-height: 1.2; }
.attention-detail { margin: 3px 0 0; font-size: 12px; }
.attention-items { max-height: 84px; overflow: auto; overscroll-behavior: contain; margin: 5px 0 0;
  padding-left: 16px; font-size: 11px; }
.attention-items li + li { margin-top: 3px; }

#input-gate { margin-top: 7px; }
.gate-form { display: grid; gap: 8px; color: var(--ink); font-size: 12px; }
.gate-form fieldset,.gate-bugs { min-width: 0; margin: 0; padding: 4px 8px 7px; border: 1.5px solid var(--ink);
  border-radius: 8px; background: var(--panel); }
.gate-form legend { max-width: 100%; padding: 0 4px; font-weight: 850; overflow-wrap: anywhere; }
.gate-choice { display: flex; gap: 7px; align-items: center; padding: 3px 0; cursor: pointer; overflow-wrap: anywhere; }
.gate-choice input { flex: 0 0 auto; margin: 0; accent-color: #653aba; }
.gate-pill { display: inline-block; border-radius: 5px; padding: 1px 4px; font-size: 10px;
  font-weight: 850; color: #473519; background: #ffdf83; }
.gate-text { display: grid; gap: 3px; font-weight: 750; }
.gate-choice + .gate-text,.gate-bugs label:first-of-type { margin-top: 4px; }
.gate-text input,.gate-text textarea { width: 100%; font: inherit; font-weight: 500; color: var(--ink);
  background: var(--panel); border: 1.5px solid var(--ink); border-radius: 6px; padding: 5px 7px; }
.gate-text textarea { resize: vertical; }
.gate-form .primary,.inspect-subgraph { background: #ffe483; }
.gate-form .primary:hover,.inspect-subgraph:hover { background: #ffcf54; }
.gate-form p,.gate-submitted p { margin: 0; overflow-wrap: anywhere; }
.gate-status:empty { display: none; }
.gate-status[data-error] { color: #98283d; font-weight: 800; }
.gate-submitted { color: #17482e; background: #d9f6e5; border: 1.5px solid #26704d; border-radius: 8px;
  font-size: 12px; padding: 7px 9px; margin-bottom: 6px; overflow-wrap: anywhere; }
.gate-submitted p { margin-top: 3px; }
.gate-submitted + button { margin-bottom: 7px; }
.gate-review { font-size: 11px; }
.gate-bugs summary { cursor: pointer; font-weight: 800; }
@keyframes attention-pulse { 50% { opacity: .66; } }
.stage { position: relative; height: 620px; min-width: 0; overflow: hidden; isolation: isolate; background: var(--sky-bottom);
  border-block: 3px solid var(--ink); }
.embed .stage { height: 500px; }
.embed .stage[data-wrapped] { height: clamp(440px, 64vw, 560px); }
.scenery,.viewport { position: absolute; inset: 0; width: 100%; height: 100%; }
.scenery { pointer-events: none; }
.viewport { cursor: grab; touch-action: none; user-select: none; }
.viewport.dragging,.viewport.dragging .node-target { cursor: grabbing; }
.controls { position: absolute; z-index: 2; top: 12px; left: 12px; right: 12px; pointer-events: none; gap: 6px; }
.controls > * { pointer-events: auto; }
.drill-breadcrumbs { flex: 1 0 100%; display: flex; flex-wrap: wrap; gap: 6px; align-items: center; min-width: 0;
  padding: 4px 6px 6px; border: 2px solid var(--ink); border-radius: 9px; background: var(--panel); font-size: 12px; }
.drill-breadcrumbs > * { flex: none; white-space: nowrap; }
.drill-breadcrumbs strong,.drill-breadcrumbs button { min-width: 0; max-width: 100%; overflow: hidden; text-overflow: ellipsis; }
.drill-breadcrumbs .gate-pill { flex: none; white-space: nowrap; margin-left: auto; }
.layout-controls { display: flex; gap: 4px; }
.layout-controls button { white-space: nowrap; }
button { cursor: pointer; border: 2px solid var(--ink); border-radius: 8px; color: var(--ink); background: var(--panel);
  padding: 5px 9px; font: inherit; font-size: 12px; font-weight: 800; box-shadow: 2px 2px 0 var(--ink); }
button:hover { background: #ffe483; }
button:focus-visible,summary:focus-visible { outline: 3px solid #296be8; outline-offset: 3px; }
button[aria-pressed=true] { background: #ffe483; }
button:disabled { opacity: .55; cursor: default; }
.zoom { min-width: 31px; font-size: 15px; line-height: 1.14; }
.live-control { display: flex; align-items: center; gap: 6px; margin-left: auto; }
.live-label { background: #faffed; box-shadow: 2px 2px 0 var(--ink); }
.live-label[data-live=active] .dot { --status: #5fe4a6; animation: live-pulse 1.6s ease-out infinite; }
.live-label[data-live=offline] .dot { --status: #ff977d; }
.empty { position: absolute; inset: 50% auto auto 50%; max-width: 80%; translate: -50% -50%; margin: 0;
  background: var(--paper); border: 2px solid var(--ink); border-radius: 8px; padding: 8px 13px; font-weight: 800;
  box-shadow: 4px 4px 0 var(--ink); pointer-events: none; }
.inspector { position: absolute; z-index: 3; top: var(--inspector-top, 54px); right: 12px; bottom: 12px; width: min(324px, calc(100% - 24px));
  overflow: auto; padding: 0 13px 12px; background: var(--panel); border: 2px solid var(--ink); border-radius: 12px;
  box-shadow: 4px 4px 0 var(--ink); overscroll-behavior: contain; }
.inspector-head { position: sticky; top: 0; z-index: 1; display: flex; align-items: center;
  justify-content: space-between; gap: 8px; padding-block: 8px; background: var(--panel);
  border-bottom: 1px solid #dce6ef; }
.inspector-head-actions { display: flex; align-items: center; gap: 6px; flex: none; }
.inspector-head-actions .inspect-subgraph { background: #ffcf54; font-weight: 900; white-space: nowrap; }
.inspector-head-actions .inspect-subgraph:hover { background: #ffe483; }
.inspector h2 { margin: 0; min-width: 0; flex: 1 1 auto; font-size: 18px; line-height: 1.2; overflow-wrap: anywhere; }
.inspector dl { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: 5px 10px; margin: 11px 0; font-size: 12px; }
.inspector dt,.inspector h3 { font-weight: 850; }
.inspector dd { margin: 0; overflow-wrap: anywhere; white-space: pre-wrap; }
.inspector h3 { margin: 10px 0 4px; font-size: 12px; }
.inspector ul { margin: 0 0 12px; padding-left: 17px; overflow-wrap: anywhere; font-size: 12px; }
.inspector code { overflow-wrap: anywhere; font-size: 11px; }
.inspect-subtasks { margin: 0 0 11px; padding: 7px 0 9px; border-block: 1px solid currentColor; }
.inspect-subtasks progress { width: 100%; height: 9px; accent-color: #31946a; }
.inspector .inspect-subtask-list { list-style: none; max-height: 148px; padding: 1px 3px 3px 0;
  margin: 5px 0 8px; overflow: auto; overscroll-behavior: contain; }
.inspect-subtask-list li + li { margin-top: 5px; }
.inspect-subtask-list button { width: 100%; display: flex; gap: 7px; align-items: baseline; text-align: left;
  box-shadow: none; overflow-wrap: anywhere; }
.inspect-subtask-list button :is(strong,small) { display: block; }
.inspect-subtask-list button .dot { flex: 0 0 8px; }
.inspect-subtask-list button .gate-pill { margin-top: 2px; }
#inspect-input { margin: 0 0 8px; }
.copy-fallback { width: 100%; margin-top: 7px; font: 11px monospace; }
.copy-note { margin: 7px 0 0; font-size: 11px; }
.roster { padding: 10px 18px 12px; background: var(--paper); border-bottom: 2px solid var(--ink); }
.roster h2 { margin: 0 0 9px; font-size: 15px; font-weight: 900; }
.roster-filters { display: flex; gap: 5px; flex-wrap: wrap; margin-bottom: 8px; }
.roster-filters button { min-height: 30px; border-radius: 999px; }
.roster-list { max-height: 220px; overflow-y: auto; overscroll-behavior: contain; overflow-anchor: none;
  display: grid; grid-template-columns: repeat(auto-fill, minmax(min(285px, 100%), 1fr)); gap: 5px; align-content: start;
  list-style: none; margin: 0; padding: 4px; border: 2px solid var(--ink); border-radius: 9px; background: #edf4fa; }
.roster-row { display: grid; grid-template-columns: auto auto minmax(0, 1fr) auto auto; align-items: center;
  grid-template-areas: 'step status name spotlight timing' 'detail detail detail detail detail';
  gap: 4px 6px; width: 100%; min-width: 0;
  padding: 5px 7px; border-radius: 5px; box-shadow: none; text-align: start; }
.roster-row:has(.roster-detail[hidden]) { row-gap: 0; }
.roster-row:hover { background: #fff1b7; }
.roster-row[aria-current=true] { background: #e3f0ff; border-color: #075ed2; }
.roster-row:focus-visible { outline-offset: -4px; }
.roster-step { grid-area: step; font-variant-numeric: tabular-nums; color: #45516a; }
.roster-status { grid-area: status; display: inline-flex; align-items: center; gap: 4px; justify-self: start; }
.roster-badge,.roster-spotlight { border: 1px solid var(--ink); border-radius: 4px; padding: 1px 4px;
  background: var(--status, #dce6ef); font-size: 10px; line-height: 1.2; }
.roster-name { grid-area: name; min-width: 0; overflow-wrap: anywhere; }
.roster-breadcrumb { color: #45516a; font-size: 11px; font-weight: 650; }
.roster-spotlight { grid-area: spotlight; justify-self: end; font-weight: 950; box-shadow: 1px 1px 0 var(--ink); }
.roster-timing { grid-area: timing; text-align: end; font-size: 11px; font-variant-numeric: tabular-nums; white-space: nowrap; }
.roster-detail { grid-area: detail; display: -webkit-box; overflow: hidden; -webkit-box-orient: vertical;
  -webkit-line-clamp: 2; color: #45516a; font-size: 11px; font-weight: 600; overflow-wrap: anywhere; }
.roster-empty { margin: 0; padding: 5px 7px; font-size: 12px; }
.report { padding: 10px 18px 12px; background: var(--paper); }
.report summary { cursor: pointer; width: max-content; max-width: 100%; font-weight: 900; }
.report-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px; }
.report h3 { margin: 10px 0 4px; font-size: 12px; }
.report ul { margin: 0; padding-left: 17px; overflow-wrap: anywhere; font-size: 12px; }
.report li + li { margin-top: 4px; }
.report .no-data { list-style: none; margin-left: -17px; color: #555e70; }
[hidden] { display: none !important; }
#world { font-family: system-ui, sans-serif; }
#world text { fill: var(--ink); pointer-events: none; }
.node-target { cursor: pointer; outline: none; }
.hit { fill: transparent; }
.selection { fill: none; stroke: #075ed2; stroke-width: 3; stroke-dasharray: 6 4; opacity: 0; pointer-events: none; }
.node-target:hover > .selection,.node-target:focus-visible > .selection,.node-target.selected > .selection,
.node-target.hovered > .selection { opacity: 1; }
.comic-stroke { stroke: var(--ink); stroke-width: 2.5; stroke-linecap: round; stroke-linejoin: round; }
.status-passed { --status: #75e0ab; --window: #a5f6dd; }
.status-cached { --status: #a5dcff; --window: #c5eaff; }
.status-running { --status: #ffd369; --window: #ffe99c; }
.status-failed,.status-circuit_broken,.status-budget_exceeded { --status: #ff9a97; --window: #ffc4a9; }
.status-awaiting_subagents,.status-stopped { --status: #d4c4ff; --window: #e2d5ff; }
.status-pending { --status: #dce6ef; --window: #e4f4ff; }
.beacons { pointer-events: none; }
.beacons .status-pending,.now-at.status-pending,.roster-spotlight.status-pending { --status: #b9e4ff; }
.beacon-halo rect { fill: none; vector-effect: non-scaling-stroke; }
.beacon-halo .beacon-outline { stroke: var(--ink); stroke-width: 4; }
.beacon-halo .beacon-light { stroke: var(--status); stroke-width: 2.5; animation: beacon-pulse 1.5s ease-in-out infinite; }
.beacon-pin { animation: beacon-bob 1.8s ease-in-out infinite; }
.beacon-pin path { fill: var(--status); }
.node .window { fill: var(--window, #e4f4ff); }
.node .status-fill { fill: var(--status, #dce6ef); }
.node .hero-art,.node .status-fill { pointer-events: none; }
.link-outline,.link-wire,.link-pulse { fill: none; stroke-linecap: round; stroke-linejoin: round; }
.link-outline { stroke: #172033; stroke-width: 6px; }
.link-wire { stroke: #edfcff; stroke-width: 2.5px; }
.link-pulse { stroke: #ffcf42; stroke-width: 3px; stroke-dasharray: 8 19; animation: zip 1.1s linear infinite; }
.retry-outline,.retry-wire { fill: none; stroke-linecap: round; stroke-linejoin: round; }
.retry-outline { stroke: #172033; stroke-width: 6px; }
.retry-wire { stroke: #d9483b; stroke-width: 3.5px; stroke-dasharray: 8 6; }
.retry-active .retry-wire { animation: retreat 2.8s linear infinite; }
.retry-label-bg { fill: #fff1cc; stroke: #172033; stroke-width: 2.5px; }
#world .retry-label-text { fill: #7d261d; }
body[data-theme=night] .retry-label-bg { fill: #ffdc90; }
@keyframes retreat { to { stroke-dashoffset: -28; } }
.loop-track { fill: none; stroke: #6740b2; stroke-width: 2.5; stroke-dasharray: 9 7; animation: zip 2.3s linear infinite; }
.status-running .flyer { animation: hover 1.45s ease-in-out infinite; }
.status-running .window { animation: glow 1.45s ease-in-out infinite; }
body[data-theme=night] .node .window { fill: #ffe78a; }
body[data-theme=night] .node.status-failed .window,body[data-theme=night] .node.status-circuit_broken .window { fill: #ffc4a9; }
body[data-theme=day] .stars,body[data-theme=night] .sun-rays { display: none; }
@keyframes zip { to { stroke-dashoffset: -54; } }
@keyframes hover { 50% { transform: translateY(-5px); } }
@keyframes glow { 50% { fill: #fffdf1; } }
@keyframes progress-pulse { 50% { background: #ffedac; } }
@keyframes beacon-pulse { 50% { stroke-width: 5; } }
@keyframes beacon-bob { 50% { transform: translateY(-5px); } }
@keyframes live-pulse { 60%,100% { box-shadow: 0 0 0 5px #5fe4a600; } 0% { box-shadow: 0 0 0 0 #5fe4a6aa; } }
@media (min-width: 980px) {
  body.full .workspace { grid-template-columns: minmax(0, 1fr) 370px; }
  body.full .dock { display: flex; flex-direction: column; height: 620px; overflow: hidden;
    border-block: 3px solid var(--ink); border-left: 3px solid var(--ink); }
  body.full .dock .attention-card { flex: none; margin: 10px 10px 4px; }
  body.full .dock .attention-card[data-input] { max-height: 64%; overflow: auto; overscroll-behavior: contain; }
  body.full .dock .roster { display: flex; flex-direction: column; min-height: 0; padding: 9px 10px; border: 0; }
  body.full .dock .roster-list { grid-template-columns: 1fr; max-height: none; min-height: 0; }
  body.full .dock .roster-name { display: flex; white-space: nowrap; }
  body.full .dock .roster-breadcrumb,body.full .dock .roster-name strong { overflow: hidden; text-overflow: ellipsis; }
  body.full .dock .roster-breadcrumb { max-width: 46%; flex: 0 2 auto; margin-right: 2px; }
  body.full .dock .roster-breadcrumb:empty { display: none; }
  body.full .dock .inspector { position: static; width: auto; min-height: 0; margin: 9px 10px 10px; }
  body.full .dock .inspector-head { flex-direction: column; align-items: stretch; }
  body.full .dock .inspector-head-actions { order: -1; justify-content: space-between; }
  body.full .dock .inspector-head-actions #close-inspector { order: -1; }
}
@media (max-width: 720px) {
  body.full { padding: max(8px, env(safe-area-inset-top)) max(8px, env(safe-area-inset-right))
    max(8px, env(safe-area-inset-bottom)) max(8px, env(safe-area-inset-left)); }
  .hud,.roster,.report { padding-inline: 10px; }
  .hud { padding-block: 10px; }
  .hud-top { gap: 7px; }
  .identity { gap: 7px; }
  .emblem { width: 36px; height: 36px; }
  .hud-actions { margin-left: auto; }
  .hud-metrics { margin-top: 7px; gap: 4px; }
  .chip { padding-inline: 5px; }
  .budget { margin: 2px 0 0; flex-basis: 100%; }
  .progress-bar-row { grid-template-columns: auto minmax(0, 1fr); gap: 7px 9px; }
  .now-at { grid-column: 1 / -1; min-height: 36px; max-width: 100%; justify-self: start; }
  .full .stage { height: clamp(390px, 117vw, 560px); }
  .controls { top: 8px; left: 8px; right: 8px; }
  .controls button,.devices > summary { min-height: 44px; }
  .drill-breadcrumbs button { min-width: 44px; }
  .layout-controls button { padding-inline: 7px; }
  .zoom { min-width: 40px; }
  .live-control { margin-left: 0; }
  .inspector { top: auto; right: 0; bottom: 0; width: 100%; max-height: 58%; padding: 0 12px 12px;
    border-radius: 14px 14px 0 0; box-shadow: 0 -4px 0 #17203333; }
  .stage[data-located] .inspector { max-height: 40%; }
  .inspector h2 { font-size: 16px; }
  .inspector button,.attention-card button,.gate-choice,.gate-text input,.gate-text textarea,
  .gate-bugs summary { min-height: 44px; min-width: 44px; }
  .gate-text input,.gate-text textarea { font-size: 16px; }
  .gate-bugs summary { align-content: center; }
  .roster-filters button { min-height: 36px; }
  .roster-row { grid-template-columns: auto minmax(0, 1fr) auto auto;
    grid-template-areas: 'step name name name' 'step status spotlight timing' 'detail detail detail detail'; }
  .report-grid { grid-template-columns: 1fr; gap: 0; }
}
@media (prefers-reduced-motion: reduce) { *, *::before, *::after { animation: none !important; } }
"""


_SCENERY = """
<svg class="scenery" viewBox="0 0 1440 620" preserveAspectRatio="xMidYMid slice" aria-hidden="true">
  <defs>
    <linearGradient id="sky" x2="0" y2="1" gradientUnits="objectBoundingBox">
      <stop stop-color="var(--sky-top)"/><stop offset="1" stop-color="var(--sky-bottom)"/>
    </linearGradient>
    <pattern id="sky-dots" width="13" height="13" patternUnits="userSpaceOnUse">
      <circle cx="2" cy="2" r="1.4" fill="#fff" opacity=".28"/>
    </pattern>
    <pattern id="tower-windows" width="27" height="30" patternUnits="userSpaceOnUse">
      <rect x="9" y="7" width="9" height="12" rx="1.5" fill="var(--sky-bottom)" opacity=".58"/>
    </pattern>
    <path id="cloud" d="M3 49C2 35 16 26 27 30 23 7 54-7 70 12 81-3 109 2 115 22 134 11 153 28 150 42 177 38 177 64 154 64H26C10 64 3 59 3 49Z"/>
    <g id="tree">
      <path d="M25 36h9v51h-9z" fill="#ab6645" stroke="#172033" stroke-width="2.5"/>
      <path d="M14 64C1 61-2 45 6 37c-1-17 11-25 20-25 17 0 25 10 25 23 12 13 5 31-11 32Z" fill="#35bc75" stroke="#172033" stroke-width="2.5"/>
      <path d="M13 42c-4-11 5-22 13-22" fill="none" stroke="#a5e795" stroke-width="4" stroke-linecap="round"/>
    </g>
    <g id="lamp" stroke="#172033" stroke-width="2.5" stroke-linejoin="round">
      <path d="M11 88h10V21q0-10 10-10h8V4h-8Q11 4 11 21z" fill="#2ba7a2"/>
      <path d="M31 8h18l7 12H29z" fill="#defcf6"/>
      <path d="M30 20h25v5H30z" fill="#fff0a1"/>
    </g>
    <g id="crosswalk" fill="#fff4de">
      <path d="M0 0h14l-12 56h-18zm26 0h14l-9 56H13zm26 0h14l-5 56H43zm26 0h14l-2 56H72zm26 0h14l3 56h-18z"/>
    </g>
  </defs>
  <path d="M0 0h1440v620H0z" fill="url(#sky)"/>
  <path d="M0 0h1440v302H0z" fill="url(#sky-dots)"/>
  <circle cx="1121" cy="99" r="52" fill="var(--sun)" opacity=".22"/>
  <circle cx="1121" cy="99" r="39" fill="var(--sun)"/>
  <path class="sun-rays" d="M1121 33V22m0 154v-11m-66-66h-11m154 0h-11m-113-47-8-8m110 110-8-8m0-94 8-8m-110 110 8-8"
    fill="none" stroke="var(--sun)" stroke-width="4" stroke-linecap="round"/>
  <path class="stars" fill="#fff3c2" d="M314 80l3 8 8 3-8 3-3 8-3-8-8-3 8-3zm391 29 2 6 6 2-6 2-2 6-2-6-6-2 6-2zm225-57 3 8 8 3-8 3-3 8-3-8-8-3 8-3zm349 98 3 8 8 3-8 3-3 8-3-8-8-3 8-3z"/>
  <g fill="var(--cloud)">
    <use href="#cloud" transform="translate(74 100) scale(1.23)"/>
    <use href="#cloud" transform="translate(434 53) scale(.83)"/>
    <use href="#cloud" transform="translate(775 117) scale(1.25)"/>
    <use href="#cloud" transform="translate(1242 53) scale(.9)"/>
    <use href="#cloud" transform="translate(220 227) scale(.78)" opacity=".61"/>
    <use href="#cloud" transform="translate(1065 220) scale(.91)" opacity=".68"/>
  </g>
  <path d="M0 468h44V354h38v-47h46v38h43v123h44V389h61v-56h50v-43h48v49h31v129h35V402h63v-42h43v-66h39v66h38v108h48v-63h57v-63h74v-31h48v157h32V365h61v-84h46v50h41v137h41V390h35v-48h56v-48h51v76h38v98h33V360h43v-36h47v49h46v173H0z"
    fill="var(--skyline-far)"/>
  <path d="M0 500h20V427h50v-28h50v-18h57v119h52V432h52v-24h62v92h49v-67h48v-36h62v103h55v-54h51v-29h66v83h72v-96h47v-23h64v119h62v-79h90v79h38V404h57v-42h60v138h38v-62h87v62h38V417h58v-33h49v116h16v56H0z"
    fill="var(--skyline)"/>
  <path d="M0 500h20V427h50v-28h50v-18h57v119h52V432h52v-24h62v92h49v-67h48v-36h62v103h55v-54h51v-29h66v83h72v-96h47v-23h64v119h62v-79h90v79h38V404h57v-42h60v138h38v-62h87v62h38V417h58v-33h49v116h16v56H0z"
    fill="url(#tower-windows)"/>
  <path d="M0 526q80-28 169 0 90-38 180 0 100-31 187 0 107-42 213 0 90-38 180 0 85-38 170 0 85-32 172 0 85-26 169-6v35H0z"
    fill="var(--horizon)"/>
  <path d="M0 551h1440v69H0z" fill="var(--road)"/>
  <path d="M0 546h1440v14H0z" fill="#fce7bd" stroke="#172033" stroke-width="3"/>
  <path d="M0 572h1440" stroke="#8290a0" stroke-width="2"/>
  <path d="M0 592h1440" stroke="#ffdc72" stroke-width="3" stroke-dasharray="29 28"/>
  <use href="#crosswalk" x="258" y="562"/><use href="#crosswalk" x="658" y="562"/><use href="#crosswalk" x="1058" y="562"/>
  <use href="#tree" x="48" y="462"/><use href="#tree" x="163" y="462"/>
  <use href="#tree" x="421" y="462"/><use href="#tree" x="545" y="462"/>
  <use href="#tree" x="796" y="462"/><use href="#tree" x="895" y="462"/>
  <use href="#tree" x="1188" y="462"/><use href="#tree" x="1348" y="462"/>
  <use href="#lamp" x="220" y="460"/><use href="#lamp" x="624" y="460"/>
  <use href="#lamp" x="1024" y="460"/><use href="#lamp" x="1275" y="460"/>
</svg>
"""


_HEROES = """
<defs>
  <marker id="street-arrow" markerWidth="10" markerHeight="10" refX="8" refY="5" orient="auto" markerUnits="userSpaceOnUse">
    <path d="M1 1l7 4-7 4z" fill="#fff" stroke="#172033" stroke-width="1.5" stroke-linejoin="round"/>
  </marker>
  <marker id="retry-arrow" markerWidth="15" markerHeight="14" refX="12" refY="7" orient="auto" markerUnits="userSpaceOnUse">
    <path d="M2 2l10 5-10 5 2-5z" fill="#c93b2b" stroke="#172033" stroke-width="2" stroke-linejoin="round"/>
  </marker>
  <symbol id="villain-doom" viewBox="0 0 64 66">
    <g class="comic-stroke">
      <path d="M23 34Q11 37 9 49L4 61Q30 69 54 60L48 43 39 34Z" fill="#136637"/>
      <path d="M22 35 17 57h28l-6-22Z" fill="#b8c5d6"/>
      <path d="M31 3Q8 8 10 33l6 14 14-6 15 4 7-12Q51 11 31 3Z" fill="#1e8f4e"/>
      <path d="m31 12-14 12 2 13 11 9 13-8 3-14Z" fill="#b8c5d6"/>
      <path d="m19 26 9 2m7 0 8-2m-19 9h15m-12 0v4m5-4v5m4-5v4" fill="none"/>
      <path d="m31 27-3 5h6" fill="none" stroke-width="1.5"/>
      <circle cx="19" cy="46" r="3.5" fill="#f8c644"/>
      <circle cx="42" cy="46" r="3.5" fill="#f8c644"/>
      <path d="m44 55 9-8 7 4-3 7-10 3Z" fill="#b8c5d6"/>
      <path d="m54 30 2 7 6 2-6 2-2 7-2-7-6-2 6-2Z" fill="#63f59b"/>
      <circle cx="61" cy="32" r="1.5" fill="#e1ffda" stroke="none"/>
    </g>
  </symbol>
  <symbol id="villain-thanos" viewBox="0 0 64 66">
    <g class="comic-stroke">
      <path d="M11 64 9 53q3-11 20-13h12q14 5 14 24Z" fill="#29355c"/>
      <path d="m12 44 14-3 9 12 8-10 12 6-7 11-13-4-14 4-13-7Z" fill="#c88a1e"/>
      <path d="m19 20-1 14 6 12q9 7 19-2l5-10-1-14Z" fill="#8c54b8"/>
      <path d="M15 27V14Q20 4 33 4q12 1 17 10v12l-7 1-3-8H25l-3 9Z" fill="#f6c445"/>
      <path d="M23 14h19m-18 15 6 1m7 0 6-2m-15 9 10-1m-10 5 1 4m4-4v5m4-5-1 4" fill="none" stroke-width="1.7"/>
      <path d="m3 37 1-10q1-4 4-1l1-4q2-4 5 0l1 4 3-1q4 1 3 6l-2 9 1 14-13 3Z" fill="#f6c445"/>
      <path d="m6 46 13-2m-9 6 7-1" fill="none" stroke-width="1.8"/>
      <circle cx="8" cy="33" r="1.8" fill="#e03a3e" stroke="none"/>
      <circle cx="13" cy="29" r="1.8" fill="#5975ff" stroke="none"/>
      <circle cx="17" cy="33" r="1.8" fill="#2bb673" stroke="none"/>
      <circle cx="13" cy="38" r="2.15" fill="#b56aff" stroke="none"/>
      <circle cx="8" cy="38" r="1.5" fill="#f77040" stroke="none"/>
    </g>
  </symbol>
  <symbol id="hero-worker" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M21 62q-6 9 3 14 4-6 8-12m17-2q8 8 2 14-6-4-10-11" fill="#6af7ff"/>
      <path d="M21 51 11 47q-4-5 1-9l16 5m24 1 10-10q5-2 8 2 2 7-12 15" fill="#e03a3e"/>
      <path d="M25 39h24l8 14-9 10-3 6h-9l-2-9-5 8-9-4 4-10-5-6Z" fill="#e03a3e"/>
      <path d="M30 46h17l4 8-7 7H31l-6-8z" fill="#f5a623"/>
      <circle cx="38" cy="52" r="6" fill="#a3fcff" stroke="#16798f"/>
      <path d="M18 22Q17 4 37 3q23-1 25 20l-4 14-12 8H28l-12-9Z" fill="#e03a3e"/>
      <path d="M24 15h9l3 5h8l3-5h9l2 13-6 12H29l-8-12Z" fill="#ffd055"/>
      <path d="M26 27l9 2m10 0 8-3" fill="none" stroke="#172033" stroke-width="6"/>
      <path d="M26 27l9 2m10 0 8-3" fill="none" stroke="#a3fcff" stroke-width="3"/>
      <path d="M32 37h15" fill="none"/>
    </g>
  </symbol>
  <symbol id="hero-code" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M7 9 71 1" stroke="#fff" stroke-width="5"/>
      <path d="M7 9 71 1m-16 2 4 22" fill="none" stroke-width="1.5"/>
      <path d="M27 47 14 40q-4-6 2-9l11 4m15 4 12-15q5-4 9 0l-9 20Z" fill="#e03a3e"/>
      <path d="M26 42q10-7 21 2l6 12 10 1 1 6-12 5-11-9-5 10 5 3-3 5-12-4-1-6 5-15Z" fill="#2d8cff"/>
      <path d="m29 43 6 8 9 8 8-5-7-13" fill="#e03a3e"/>
      <path d="M18 24Q24 8 41 11q20 3 16 23-4 14-20 13-22-2-19-23Z" fill="#e03a3e"/>
      <path d="M23 23q14 10 32 2m-33 9q12 10 31 4M39 13l-2 32m-8-30-3 28m21-27 1 27" stroke="#932230" stroke-width="1" fill="none"/>
      <path d="M23 25q8 2 12 12-10 2-12-12Zm28 0q-9 2-11 12 11 0 11-12Z" fill="#fff"/>
    </g>
  </symbol>
  <symbol id="hero-verifier" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M27 37q10-5 22 2l11 15-8 5-5-4 6 14-12 2-5-12-6 12-12-2 7-14-8 4-6-6Z" fill="#424070"/>
      <path d="M27 43l10 8 11-9-7 13h-8Z" fill="#b599ff"/>
      <path d="M19 25l1-18 13 7q6-2 15 0l11-8 2 19q6 18-10 22H31q-16-4-12-22Z" fill="#28273f"/>
      <path d="m25 28 10 3-3 6-9-4zm30 0-11 3 3 6 10-4z" fill="#dbbbff"/>
      <path d="M35 39h9" fill="none" stroke="#9f89f8"/>
      <path d="M56 43q7 4 15 2l-1 13q-3 7-12 12-8-5-11-12V46q5 0 9-3Z" fill="#7c5cff"/>
      <path d="m52 56 4 5 9-10" fill="none" stroke="#effdff" stroke-width="3"/>
    </g>
  </symbol>
  <symbol id="hero-gate" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M64 21v53m-5 0h12" fill="none" stroke-width="4"/>
      <rect x="56" y="13" width="17" height="25" rx="4" fill="#36a9ac"/>
      <circle cx="64.5" cy="22" r="3" fill="#ff8b85"/><circle cx="64.5" cy="30" r="3" fill="#a5f6aa"/>
      <path d="M18 39Q9 38 5 49l-2 9q2 8 10 7l5-8 1 11-3 5h12l6-9 4 9h13l-4-7 1-13q5 9 12 6l4-5-4-8-13-9Z" fill="#55c97a"/>
      <path d="m19 55 13 3 15-3 2 10-11 3-5-5-5 5-11-3Z" fill="#7c5cff"/>
      <path d="M14 20q-2-13 13-13h18q10 2 11 11l-1 14q-4 12-18 12H29q-15-1-16-14Z" fill="#6ada7f"/>
      <path d="M14 21q-5-12 6-14l4-4 5 3 7-5 5 5 7-2q10 2 10 13l-8-4-5 4-6-2-8 4-7-3-7 8Z" fill="#245d48"/>
      <path d="m24 25 7 2m11 0 6-2m-18 9q6 4 12-1" fill="none"/>
    </g>
  </symbol>
  <symbol id="hero-captain" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M21 43h26l4 15-5 14h-9l-3-10-4 10h-9l-4-14Z" fill="#2266d6"/>
      <path d="M23 53h22v7H23Z" fill="#ffffff"/>
      <path d="M28 53v7m6-7v7m6-7v7" stroke="#e03a3e" stroke-width="3"/>
      <path d="M20 67h10l-1 6H19Zm18 0h10l1 6H37Z" fill="#e03a3e"/>
      <path d="M21 45 10 53q-2 4 2 7l10-6" fill="#2266d6"/>
      <path d="M10 53l-3 5q1 4 5 2l3-4Z" fill="#e03a3e"/>
      <path d="M15 24Q16 6 35 6q19 0 20 18l-2 13q-5 8-18 8-13 0-18-8Z" fill="#2266d6"/>
      <path d="M21 31q14 3 28 0l-2 9q-12 6-24 0Z" fill="#ffd2a6"/>
      <path d="M15 22 6 14l4 11Zm40 0 9-8-4 11Z" fill="#ffffff"/>
      <path d="M32 19l3-7 3 7m-4.8-2.2h3.6" fill="none" stroke="#ffffff" stroke-width="2.2"/>
      <circle cx="27" cy="26" r="2.2" fill="#ffffff" stroke="none"/>
      <circle cx="43" cy="26" r="2.2" fill="#ffffff" stroke="none"/>
      <path d="M30 37q5 3 10 0" fill="none" stroke-width="2"/>
      <circle cx="55" cy="52" r="16" fill="#e03a3e"/>
      <circle cx="55" cy="52" r="11.5" fill="#ffffff"/>
      <circle cx="55" cy="52" r="7.5" fill="#e03a3e"/>
      <circle cx="55" cy="52" r="4.8" fill="#2266d6" stroke="none"/>
      <path d="M55 47.4l1.3 3.2 3.4.3-2.6 2.2.8 3.3-2.9-1.8-2.9 1.8.8-3.3-2.6-2.2 3.4-.3Z" fill="#ffffff" stroke="none"/>
    </g>
  </symbol>
  <symbol id="hero-thor" viewBox="0 0 76 78">
    <g class="comic-stroke">
      <path d="M16 42 5 69q14 6 32 4l8-28Z" fill="#e03a3e"/>
      <path d="M22 42h24l5 16-4 14h-9l-3-10-4 10h-9l-4-14Z" fill="#2e3b59"/>
      <circle cx="29" cy="49" r="3" fill="#dce8f8" stroke-width="1.5"/>
      <circle cx="40" cy="49" r="3" fill="#dce8f8" stroke-width="1.5"/>
      <path d="M21 67h10v6H20Zm16 0h10v6H36Z" fill="#e03a3e"/>
      <path d="M44 44 57 32l4 5-12 13Z" fill="#2e3b59"/>
      <path d="M15 24Q15 8 34 8q19 0 19 16l2 14q-6 8-21 8-15 0-21-8Z" fill="#f7c948"/>
      <path d="M20 26q14 2 28 0l-2 13q-12 6-24 0Z" fill="#ffd2a6"/>
      <path d="M15 23Q16 6 34 6q18 0 19 17H15Z" fill="#cfdceb"/>
      <path d="M15 19 4 9l5 14Zm38 0 11-10-5 14Z" fill="#eef6ff"/>
      <circle cx="28" cy="30" r="2.2" fill="#172033" stroke="none"/>
      <circle cx="40" cy="30" r="2.2" fill="#172033" stroke="none"/>
      <path d="M30 37q4 3 8 0" fill="none" stroke-width="2"/>
      <path d="M58 24v16" fill="none" stroke="#9c633a" stroke-width="4.5"/>
      <rect x="49" y="10" width="20" height="14" rx="3" fill="#c7d6e8"/>
      <path d="M45 6l5 6-4 2 5 5M71 7l-4 6 4 2-5 6" fill="none" stroke="#6af7ff" stroke-width="2.5"/>
    </g>
  </symbol>
</defs>
"""


_SCRIPT = r"""
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const bootstrap = JSON.parse($('town-snapshot').textContent);
  const options = JSON.parse($('town-options').textContent);
  const viewport = $('viewport');
  const world = $('world');
  const NS = 'http://www.w3.org/2000/svg';
  const labels = {
    passed: 'Passed', cached: 'Cached', running: 'Running', failed: 'Failed', pending: 'Pending',
    circuit_broken: 'Circuit broken', awaiting_subagents: 'Awaiting subagents',
    budget_exceeded: 'Budget exceeded', stopped: 'Stopped', missing: 'Files missing'
  };
  const blockedStatuses = new Set(['failed', 'circuit_broken', 'budget_exceeded']);
  const frontierKinds = [
    {group: 'running', label: '⚡ Running: ', pin: 'RUNNING', pill: 'NOW'},
    {group: 'waiting', label: '🛡 Waiting: ', pin: 'WAITING', pill: 'WAITING'},
    {group: 'failed', label: '⚠ Stuck: ', pin: 'FAILED', pill: 'STUCK'},
    {group: 'pending', label: '▶ Next: ', pin: 'NEXT', pill: 'NEXT'}
  ];
  const roles = {
    worker: ['#e03a3e', '#ffce59', 'ϟ'],
    code: ['#e85d2a', '#ffbc70', '⌘'],
    verifier: ['#7c5cff', '#c1a7ff', '✓'],
    gate: ['#2bb673', '#a5e79a', '◈']
  };
  const districts = {
    subgraph: ['City block', '#87c7fd', '#e4f4ff', '#2d8cff'],
    loop_subgraph: ['Repair district', '#bb9bec', '#f1e5ff', '#805ad2'],
    splitter: ['Squad avenue', '#7fdbbb', '#e2faee', '#269981']
  };
  const camera = {x: 0, y: 0, scale: 1};
  const targets = new Map();
  const nodeIndex = new Map();
  const pointers = new Map();
  const rosterRows = new Map();
  const rosterList = $('roster-list');
  const rosterFilters = $('roster-filters');
  const roster = $('roster');
  const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
  const wideScreen = matchMedia('(min-width: 980px)');
  const dock = $('dock');
  const inspector = $('inspector');
  const attentionCard = $('attention-card');
  const stage = viewport.closest('.stage');
  const controls = $('fit-town').closest('.controls');
  const layoutButtons = [...controls.querySelectorAll('[data-layout]')];
  const signMetrics = document.createElement('canvas').getContext('2d');
  const signFont = 16;
  signMetrics.font = '850 ' + signFont + 'px system-ui';
  const rowGap = 78, waveGapY = 25, packedGap = 34;
  const mapInsetX = 18;
  // The ground axes recede in opposite directions. An inverse vertical
  // vector keeps extruded walls plumb in the projected town.
  const isoTilt = .27, isoSkew = -.39, isoDepth = .74;
  const isoDet = isoDepth - isoTilt * isoSkew;
  const plumbX = -isoSkew / isoDet, plumbY = 1 / isoDet;
  let layoutMode = options.layoutMode;
  let layoutSpace, townSpace;
  let snapshot, graphSignature, selected = null, gestureDragged = false, lastSkyTap;
  let fittedNodes = false, cameraMoved = false, cameraFrame = 0;
  let tasks = [], spotlights = new Map(), primarySpotlight, rosterFilter = 'all', hoveredTask = null;
  let attention, attentionSignature, inputGateSignature, preferredInputPath;
  let drillStack = [], breadcrumbSignature;
  let automatic = Boolean(options.live), timer, inFlight;
  let activeRunId = options.run, displayedRunId = options.run;

  function svg(tag, attrs, parent) {
    const el = document.createElementNS(NS, tag);
    Object.entries(attrs).forEach(([name, value]) => el.setAttribute(name, value));
    parent.append(el);
    return el;
  }
  function rect(parent, x, y, width, height, fill, attrs = {}) {
    return svg('rect', {x, y, width, height, fill, ...attrs}, parent);
  }
  function text(parent, value, x, y, size, attrs = {}) {
    const el = svg('text', {x, y, 'font-size': size, 'font-weight': 850, ...attrs}, parent);
    el.textContent = value;
    return el;
  }
  function path(parent, d, attrs) {
    return svg('path', {d, ...attrs}, parent);
  }
  function titleCase(value) {
    return String(value ?? '').replace(/_/g, ' ').replace(/^./, c => c.toUpperCase());
  }
  function statusLabel(status) {
    return Object.hasOwn(labels, status) ? labels[status] : titleCase(status);
  }
  function statusClass(status) {
    return 'status-' + (Object.hasOwn(labels, status) ? status : 'pending');
  }
  function shorten(value, max) {
    const chars = Array.from(String(value));
    return chars.length > max ? chars.slice(0, max - 1).join('') + '…' : chars.join('');
  }
  function seconds(value) {
    return value == null ? '—' : value + 's';
  }
  function collectLeafTasks(nodes, parents = [], tasks = []) {
    // Wave order respects dependencies. Stable sorting keeps the graph's
    // declaration order for siblings that can run together.
    for (const node of [...nodes].sort((a, b) => a.wave - b.wave)) {
      if (node.node_type === 'task') tasks.push({node, path: node.path, parents});
      else collectLeafTasks(node.children, [...parents, node.id], tasks);
    }
    return tasks;
  }
  function taskGroup(status) {
    if (status === 'passed' || status === 'cached') return 'done';
    if (status === 'awaiting_subagents') return 'waiting';
    if (blockedStatuses.has(status)) return 'failed';
    return status;
  }
  function docked() {
    return wideScreen.matches && document.body.classList.contains('full');
  }
  function syncInspector() {
    inspector.hidden = !targets.has(selected);
    roster.hidden = docked() && !inspector.hidden;
  }
  function layoutDock() {
    if (docked()) {
      dock.prepend(attentionCard);
      dock.append(inspector);
    } else {
      stage.before(attentionCard);
      stage.append(inspector);
    }
    const close = $('close-inspector');
    close.textContent = docked() ? '← Back to Tasks' : '×';
    close.setAttribute('aria-label', docked() ? 'Back to Tasks' : 'Close inspector');
    syncInspector();
  }
  wideScreen.addEventListener('change', layoutDock);
  layoutDock();
  function updateTaskProgress() {
    tasks = collectLeafTasks(snapshot.nodes);
    primarySpotlight = undefined;
    spotlights.clear();
    for (const kind of frontierKinds) {
      const matches = tasks.filter(({node}) => taskGroup(node.status) === kind.group);
      if (!matches.length) continue;
      primarySpotlight = matches[0];
      for (const task of kind.group === 'pending' ? matches.slice(0, 1) : matches) {
        spotlights.set(task.path, kind);
      }
      break;
    }

    const total = tasks.length;
    const done = tasks.filter(({node}) => taskGroup(node.status) === 'done').length;
    const percent = total ? Math.floor(100 * done / total) : 0;
    const count = done + ' / ' + total + ' tasks (' + percent + '%)';
    $('progress-count').textContent = count;
    $('roster-summary').textContent = 'All Tasks (' + done + '/' + total + ')';
    const bar = $('task-progress');
    bar.setAttribute('aria-valuenow', percent);
    bar.setAttribute('aria-valuetext', count);
    tasks.forEach(({node, path}, i) => {
      const segment = bar.children[i] || bar.appendChild(element('span', ''));
      segment.className = 'progress-segment ' + statusClass(node.status);
      segment.title = path + ' · ' + statusLabel(node.status);
    });
    while (bar.children.length > total) bar.lastElementChild.remove();

    const nowAt = $('now-at');
    nowAt.disabled = !primarySpotlight;
    nowAt.className = 'now-at' + (primarySpotlight ? ' ' + statusClass(primarySpotlight.node.status) : '');
    $('now-at-text').textContent = primarySpotlight ? spotlights.get(primarySpotlight.path).label + primarySpotlight.path :
      '✓ All done (' + done + '/' + total + ')';
  }
  $('now-at').addEventListener('click', () => {
    if (primarySpotlight) focusNode(primarySpotlight.path);
  });
  const attentionLabels = {
    decision: '🛡 NEEDS YOUR DECISION', waiting: '🛡 HERO NEEDED!', stuck: '⚠ STUCK',
    running: '⚡ RUNNING NOW', done: '✓ ALL DONE'
  };
  function legacyAttention() {
    const group = taskGroup(snapshot.status);
    if (group === 'done' && tasks.length && !primarySpotlight) {
      return {kind: 'done', headline: 'Workflow complete'};
    }
    const node = primarySpotlight?.node;
    if (node && ['running', 'waiting', 'failed'].includes(group) && taskGroup(node.status) === group) {
      return {kind: group === 'failed' ? 'stuck' : group, headline: titleCase(node.id),
        detail: node.detail || node.error_signature, node_path: node.path};
    }
    return null;
  }
  function renderAttention() {
    let next = snapshot.attention ?? (Object.hasOwn(snapshot, 'attention') ? null : legacyAttention());
    const preferred = nodeIndex.get(preferredInputPath);
    const gate = preferred?.input_gate;
    if (gate && !gate.submitted && preferred.path !== next?.node_path) {
      next = {kind: 'decision', headline: preferred.title || titleCase(preferred.id),
        detail: preferred.detail, node_path: preferred.path, input_gate: gate};
    } else if (!gate || gate.submitted) {
      preferredInputPath = null;
    }
    const signature = JSON.stringify(next);
    if (signature === attentionSignature) return false;
    attention = next;
    attentionSignature = signature;
    attentionCard.hidden = !attention || !Object.hasOwn(attentionLabels, attention.kind);
    renderInputGate(attention?.input_gate);
    if (attentionCard.hidden) return true;
    attentionCard.dataset.kind = attention.kind;
    $('attention-tag').textContent = attentionLabels[attention.kind];
    $('attention-headline').textContent = attention.headline;
    const detail = $('attention-detail');
    detail.textContent = attention.detail || '';
    detail.hidden = !attention.detail;
    const items = Array.isArray(attention.items) ? attention.items : [];
    const list = $('attention-items');
    list.replaceChildren(...items.map(item => element('li', item)));
    list.hidden = !items.length;
    $('attention-focus').hidden = !attention.node_path;
    return true;
  }
  function gateChoice(label, {type, name, value, checked = false, recommended = false}) {
    const row = element('label', '');
    row.className = 'gate-choice';
    const input = document.createElement('input');
    Object.assign(input, {type, name, value, checked});
    row.append(input, element('span', label));
    if (recommended) {
      const star = element('strong', '★ Recommended');
      star.className = 'gate-pill';
      row.append(star);
    }
    return {row, input};
  }

  function gateText(label, tag, limit) {
    const row = element('label', '');
    row.className = 'gate-text';
    const input = document.createElement(tag);
    input.maxLength = limit;
    if (tag === 'textarea') input.rows = 2;
    row.append(element('span', label), input);
    return {row, input};
  }

  function gateQuestion(question, index, readOnly) {
    const card = element('fieldset', '');
    const legend = element('legend', '');
    if (question.ticket) {
      const ticket = element('span', question.ticket);
      ticket.className = 'gate-pill';
      legend.append(ticket, ' ');
    }
    legend.append(question.question);
    card.append(legend);

    const choices = question.options;
    const multiple = question.multi_select && choices.length > 0;
    const name = 'gate-question-' + index;
    const controls = [];
    for (const value of choices.length ? choices : [question.recommended].filter(Boolean)) {
      const recommended = value === question.recommended;
      const label = choices.length ? value : '★ Accept Recommended · ' + value;
      const {row, input} = gateChoice(label, {name, value, type: multiple ? 'checkbox' : 'radio',
        checked: recommended && !multiple, recommended: recommended && Boolean(choices.length)});
      card.append(row);
      controls.push(input);
    }
    let custom, customChoice;
    if (!controls.length || (question.allow_note && !readOnly)) {
      custom = gateText(controls.length ? (multiple ? 'Additional answer' : 'Custom answer') : 'Answer',
        controls.length ? 'input' : 'textarea', 8000);
      if (controls.length && !multiple) {
        customChoice = gateChoice('Custom answer', {name, value: '', type: 'radio'});
        card.append(customChoice.row);
        custom.row.firstElementChild.textContent = 'Enter your answer';
        custom.input.addEventListener('focus', () => { customChoice.input.checked = true; });
        custom.input.addEventListener('input', () => { customChoice.input.checked = true; });
      }
      card.append(custom.row);
    }
    const answer = () => {
      const selected = controls.filter(input => input.checked).map(input => input.value);
      const typed = custom?.input.value.trim();
      if (multiple) return typed ? [...selected, typed] : selected;
      return !controls.length || customChoice?.input.checked ? typed || '' : selected[0] || '';
    };
    return {card, answer};
  }

  function renderInputGate(gate) {
    const container = $('input-gate');
    const submitted = gate?.submitted;
    const signature = gate ? JSON.stringify([displayedRunId, gate.node_path, gate.packet_hash,
      gate.can_respond, submitted?.answered_at]) : '';
    if (signature === inputGateSignature) return;
    inputGateSignature = signature;
    container.replaceChildren();
    container.hidden = !gate;
    attentionCard.toggleAttribute('data-input', Boolean(gate));
    if (!gate) return;

    const canRespond = Boolean(options.respond && gate.can_respond);
    const form = element('form', '');
    form.className = 'gate-form';
    const questions = (gate.questions || []).map((question, index) => ({
      id: question.id, ...gateQuestion(question, index, !canRespond)
    }));
    for (const question of questions) form.append(question.card);
    const approval = gate.gate_type === 'approval' || gate.gate_type === 'hybrid';
    const fields = [], rejected = [];
    if (approval) {
      const release = element('fieldset', '');
      release.append(element('legend', 'Release approval'));
      if (gate.review_summary) {
        const review = element('p', gate.review_summary);
        review.className = 'gate-review';
        release.append(review);
      }
      for (const field of gate.approved_fields || []) {
        const {row, input} = gateChoice(field.label, {type: 'checkbox', checked: field.default !== false});
        fields.push([field.key, input]);
        release.append(row);
      }
      form.append(release);
      if (canRespond && gate.bugs?.length) {
        const list = element('details', '');
        list.className = 'gate-bugs';
        list.append(element('summary', 'Rejected bugs'));
        for (const bug of gate.bugs) {
          const {row, input} = gateChoice(bug.title === bug.id ? bug.id : bug.title + ' · ' + bug.id,
            {type: 'checkbox', value: bug.id});
          list.append(row);
          rejected.push(input);
        }
        form.append(list);
      }
    }

    if (submitted) {
      const stamp = new Date(submitted.answered_at);
      const date = Number.isFinite(stamp.valueOf()) ? ' · ' + stamp.toLocaleString() : '';
      const banner = element('div', '');
      banner.className = 'gate-submitted';
      banner.setAttribute('role', 'status');
      banner.append(element('strong', '✓ Submitted by ' + (submitted.approved_by || 'reviewer') + date));
      if (submitted.summary) banner.append(element('p', submitted.summary));
      container.append(banner);
      if (canRespond) {
        const edit = element('button', 'Edit & Resubmit');
        edit.type = 'button';
        edit.setAttribute('aria-expanded', 'false');
        edit.setAttribute('aria-controls', 'input-gate-form');
        edit.addEventListener('click', () => {
          form.hidden = !form.hidden;
          edit.setAttribute('aria-expanded', String(!form.hidden));
          edit.textContent = form.hidden ? 'Edit & Resubmit' : 'Cancel Edit';
        });
        container.append(edit);
        form.hidden = true;
      }
    }
    form.id = 'input-gate-form';
    container.append(form);
    if (!canRespond) {
      for (const control of form.elements) control.disabled = true;
      if (!submitted) form.append(element('p', 'Read-only input preview'));
      return;
    }
    const responder = gateText('Respond as', 'input', 80);
    let savedResponder = '';
    try { savedResponder = localStorage.getItem('kgraph.reviewer') || ''; } catch {}
    responder.input.value = savedResponder || 'Reviewer';
    responder.input.placeholder = 'Your name';
    responder.input.required = true;
    const notes = gateText('Notes (optional)', 'textarea', 8000);
    form.append(responder.row, notes.row);
    const actions = element('div', '');
    actions.className = 'actions';
    const action = (label, value, primary) => {
      const button = element('button', label);
      Object.assign(button, {type: 'submit', value});
      if (primary) button.className = 'primary';
      actions.append(button);
    };
    if (approval) {
      action('✓ Approve', 'approve', true);
      action('✕ Request Fixes', 'reject', false);
    } else {
      action('Submit Decision', 'submit', true);
    }
    const status = element('p', '');
    status.className = 'gate-status';
    status.setAttribute('role', 'status');
    form.append(actions, status);
    form.addEventListener('submit', async event => {
      event.preventDefault();
      if (options.runs && activeRunId !== displayedRunId) {
        status.textContent = 'Wait for the selected run to load.';
        return;
      }
      const payload = {node_path: gate.node_path, packet_hash: gate.packet_hash,
        submitted_at: submitted?.answered_at ?? null, approved_by: responder.input.value, notes: notes.input.value};
      if (questions.length) payload.decisions = Object.fromEntries(questions.map(q => [q.id, q.answer()]));
      if (approval) {
        payload.approved = event.submitter?.value !== 'reject';
        payload.approved_fields = Object.fromEntries(fields.map(([key, input]) => [key, input.checked]));
        if (!payload.approved) payload.rejected_bugs = rejected.filter(input => input.checked).map(input => input.value);
      }
      const url = options.runs ? options.respond + '/' + encodeURIComponent(displayedRunId) + '/respond' : options.respond;
      for (const button of actions.children) button.disabled = true;
      status.removeAttribute('data-error');
      status.textContent = 'Submitting…';
      try {
        const response = await fetch(url, {method: 'POST', headers: {'Content-Type': 'application/json'},
          body: JSON.stringify(payload)});
        const data = await response.json();
        if (!response.ok) throw new Error(data.error || 'HTTP ' + response.status);
        try {
          const name = responder.input.value.trim();
          if (name) localStorage.setItem('kgraph.reviewer', name);
        } catch {}
        if (inputGateSignature === signature && activeRunId === displayedRunId) {
          renderInputGate({...gate, submitted: {answered_at: data.answered_at,
            approved_by: data.decision.approved_by, summary: data.summary}});
          refresh();
        }
      } catch (error) {
        status.dataset.error = '';
        status.textContent = error.message || 'Could not submit the decision.';
      } finally {
        for (const button of actions.children) button.disabled = false;
      }
    });
  }

  $('attention-focus').addEventListener('click', () => focusNode(attention?.node_path));

  function indexNodes(nodes) {
    for (const node of nodes) {
      nodeIndex.set(node.path, node);
      indexNodes(node.children || []);
      indexNodes(node.sub_graph?.nodes || []);
    }
  }

  function subProgress(node, graph) {
    if (node.sub_progress) return node.sub_progress;
    const children = graph.nodes;
    return {total: children.length, done: children.filter(child => taskGroup(child.status) === 'done').length,
      running: children.filter(child => child.status === 'running').length};
  }

  function stageGraph() {
    const path = drillStack.at(-1);
    return path ? subGraph(nodeIndex.get(path)) || {nodes: [], edges: []} : snapshot;
  }

  function renderBreadcrumbs() {
    const bar = $('drill-breadcrumbs');
    const node = nodeIndex.get(drillStack.at(-1));
    const graph = subGraph(node);
    const progress = graph ? subProgress(node, graph) : null;
    const names = ['Town', ...drillStack.map(path => nodeIndex.get(path)?.title || nodeIndex.get(path)?.id)];
    const signature = JSON.stringify([drillStack, names, progress]);
    if (signature === breadcrumbSignature) return;
    breadcrumbSignature = signature;
    const focused = bar.contains(document.activeElement) && document.activeElement.dataset.depth;
    bar.hidden = !drillStack.length;
    bar.replaceChildren();
    if (bar.hidden) return;
    const crumb = (label, depth) => {
      const button = element('button', label);
      button.type = 'button';
      button.dataset.depth = depth;
      button.addEventListener('click', () => drillTo(drillStack.slice(0, depth)));
      return button;
    };
    const back = crumb('← Back', drillStack.length - 1);
    back.id = 'drill-back';
    bar.append(back);
    for (const [depth, name] of names.entries()) {
      if (depth > 0) {
        const arrow = element('span', '›');
        arrow.setAttribute('aria-hidden', 'true');
        bar.append(arrow);
      }
      const current = depth === drillStack.length;
      const item = current ? element('strong', name) : crumb(name, depth);
      if (current) item.setAttribute('aria-current', 'location');
      bar.append(item);
    }
    if (progress) {
      const counts = element('span', progress.done + '/' + progress.total + ' done');
      counts.className = 'gate-pill';
      bar.append(counts);
    }
    if (focused !== false) [...bar.querySelectorAll('button')].find(button => button.dataset.depth === focused)?.focus();
  }

  function drillTo(stack, focusPath = null, focusOptions = {scroll: false}) {
    const previous = drillStack.at(-1);
    select(null);
    drillStack = stack;
    redrawTown();
    fitTown();
    $('empty').hidden = Boolean(stageGraph().nodes.length);
    if (focusPath) {
      focusNode(focusPath, focusOptions);
      targets.get(focusPath)?.target.focus({preventScroll: true});
    } else if (drillStack.length) {
      $('drill-back')?.focus({preventScroll: true});
    } else {
      targets.get(previous)?.target.focus({preventScroll: true});
    }
  }

  function openSubGraph(node, focusPath, focusOptions) {
    if (!subGraph(node)) return;
    const ancestors = drillStack.filter(path => node.path.startsWith(path + '/'));
    drillTo([...ancestors, node.path], focusPath, focusOptions);
  }

  // Measure inside-out so wide districts count at their actual width.
  function measureNode(node) {
    if (node.node_type === 'task') return {node, w: 202, h: 152};
    const parallel = node.node_type === 'splitter';
    const inside = measureLevel(node.children, node.edges, {parallel});
    const inset = layoutMode === 'iso' ? 91 : 74;
    return {node, inside, w: Math.max(258, inside.w + 36), h: Math.max(135, inside.h + inset)};
  }

  function subGraph(node) {
    if (node?.sub_graph?.nodes?.length) return node.sub_graph;
    if (node?.children?.length) return {nodes: node.children, edges: node.edges || [], parallel: node.node_type === 'splitter'};
    return null;
  }

  function buildingStatus(node) {
    if (node.status !== 'awaiting_subagents') return statusLabel(node.status).toUpperCase();
    if ((node.input_gate && !node.input_gate.submitted) ||
        (attention?.kind === 'decision' && attention.node_path === node.path)) return 'INPUT NEEDED';
    const progress = node.sub_progress;
    return progress?.running > 0 ? 'ACTIVE ' + progress.done + '/' + progress.total : 'WAITING';
  }

  function splitterRows(branches, gapX, gapY) {
    if (!branches.length) return [[]];
    const maxCols = layoutMode === 'strip' ? branches.length : Math.min(branches.length, 4);
    const minCols = layoutMode === 'strip' ? maxCols : 1;
    let bestRows, bestSize = Infinity;
    for (let cols = minCols; cols <= maxCols; cols++) {
      const widths = Array(cols).fill(0);
      branches.forEach((item, i) => {
        widths[i % cols] = Math.max(widths[i % cols], item.w);
      });
      const rows = [];
      let height = 0;
      for (let i = 0; i < branches.length; i += cols) {
        const row = branches.slice(i, i + cols).map((item, col) => ({group: [item], w: widths[col], h: item.h}));
        rows.push(row);
        height += Math.max(...row.map(wave => wave.h));
      }
      height += gapY * (rows.length - 1);
      const width = widths.reduce((sum, w) => sum + w, gapX * (cols - 1));
      const size = Math.max(width / 2.1, height);
      if (size < bestSize) {
        bestSize = size;
        bestRows = rows;
      }
    }
    return bestRows;
  }

  function wrapWaves(waves, gap, root, place) {
    if (layoutMode === 'strip' || waves.length < (root ? 2 : 3)) return place([waves]);
    const widths = [0];
    for (const wave of waves) widths.push(widths.at(-1) + wave.w + gap);

    // For each row count the total width is fixed, so minimizing squared
    // widths also minimizes variance. Reuse the partitions across counts.
    const starts = [[], Array(waves.length + 1).fill(0)];
    let costs = widths.map(width => (width - gap) ** 2);
    let bestRows = [waves];
    const initial = place(bestRows);
    const {single, wrapped} = townSpace;
    const target = root ? single : wrapped;
    let bestScale = Math.min(target.w / initial.w, target.h / initial.h);
    let bestShape = Math.abs(Math.log(initial.w / initial.h / target.aspect));
    const minHeight = Math.min(...waves.map(wave => wave.h));
    for (let count = 2; count <= waves.length; count++) {
      if (wrapped.h / (count * minHeight + (count - 1) * rowGap) < bestScale) break;
      const nextCosts = [], rowStarts = [];
      for (let end = count; end <= waves.length; end++) {
        let cost = Infinity;
        for (let start = count - 1; start < end; start++) {
          const width = widths[end] - widths[start] - gap;
          const next = costs[start] + width ** 2;
          if (next < cost) {
            cost = next;
            rowStarts[end] = start;
          }
        }
        nextCosts[end] = cost;
      }
      costs = nextCosts;
      starts.push(rowStarts);

      const rows = [];
      let end = waves.length;
      for (let row = count; row > 0; row--) {
        const start = starts[row][end];
        rows.push(waves.slice(start, end));
        end = start;
      }
      rows.reverse();
      const candidate = place(rows);
      const scale = Math.min(wrapped.w / candidate.w, wrapped.h / candidate.h);
      const shape = Math.abs(Math.log(candidate.w / candidate.h / wrapped.aspect));
      if (scale > bestScale || (scale === bestScale && shape < bestShape)) {
        bestRows = rows;
        bestScale = scale;
        bestShape = shape;
      }
    }
    // Restore the winning coordinates after scoring the other arrangements.
    return starts.length === 2 ? initial : place(bestRows);
  }

  function measureLevel(nodes, edges, {parallel = false, root = false} = {}) {
    const items = new Map(nodes.map(node => [node.path, measureNode(node)]));
    const gapX = parallel ? 24 : 74;
    const rowSpacing = parallel ? 20 : rowGap;
    const place = rows => placeRows(rows, edges, items, gapX, rowSpacing, parallel);
    if (parallel) {
      for (const item of items.values()) item.column = 0;
      return place(splitterRows([...items.values()], gapX, rowSpacing));
    }
    const columns = new Map();
    for (const item of items.values()) {
      const wave = item.node.wave;
      if (!columns.has(wave)) columns.set(wave, []);
      columns.get(wave).push(item);
    }
    const waves = [...columns.entries()].sort(([a], [b]) => a - b).map(([, group], index) => {
      const packed = layoutMode !== 'strip' && group.length >= 4 && group.every(item => item.node.node_type === 'task');
      group.forEach((item, i) => {
        item.column = index;
        item.innerEntry = packed && i % 2 === 1;
        item.innerExit = packed && i % 2 === 0;
      });
      const cellWidth = Math.max(...group.map(item => item.w));
      const line = packed ? group.filter((_, i) => i % 2 === 0) : group;
      return {
        group, cellWidth, packed,
        w: packed ? cellWidth * 2 + packedGap : cellWidth,
        h: line.reduce((sum, item) => sum + item.h, 0) + waveGapY * (line.length - 1)
      };
    });
    return wrapWaves(waves, gapX, root, place);
  }

  function placeRows(rowGroups, edges, items, gapX, rowSpacing, parallel) {
    const rows = rowGroups.map((rowWaves, row) => {
      rowWaves.forEach((wave, col) => {
        for (const item of wave.group) Object.assign(item, {row, col});
      });
      return {waves: rowWaves, bypasses: 0, avenues: 0};
    });
    let express = 0;
    const connections = [];
    const retryConnections = [];
    for (const edge of edges) {
      const from = items.get(edge.from), to = items.get(edge.to);
      if (!from || !to) continue;
      if (edge.kind === 'retry') {
        retryConnections.push({from, to, label: edge.label, lane: retryConnections.length});
        continue;
      }
      if (from.column >= to.column) continue;
      const connection = {from, to};
      if (to.row > from.row) {
        connection.exit = rows[from.row].avenues++;
        if (to.row > from.row + 1) {
          connection.entry = rows[to.row - 1].avenues++;
          connection.express = express++;
        }
      } else if (to.col > from.col + 1 || from.innerExit || to.innerEntry) {
        connection.bypass = rows[from.row].bypasses++;
      }
      connections.push(connection);
    }
    const wrapped = !parallel && rows.length > 1;
    const retryGutter = retryConnections.length ? 34 + (retryConnections.length - 1) * 10 : 0;
    const left = (express ? 57 + 7 * (express - 1) : wrapped ? 32 : 0) + retryGutter;
    const right = (wrapped ? 28 : 0) + retryGutter;
    let w = 0, y = 0;
    for (const row of rows) {
      row.top = y;
      const overhead = row.bypasses ? 16 + row.bypasses * 11 : 0;
      const tallest = Math.max(0, ...row.waves.map(wave => wave.h));
      let x = left;
      for (const wave of row.waves) {
        let laneY = y + overhead + (tallest - wave.h) / 2;
        wave.group.forEach((item, i) => {
          const itemX = item.innerEntry ? x + wave.cellWidth + packedGap : x;
          Object.assign(item, {x: itemX, y: laneY, columnWidth: wave.cellWidth ?? wave.w});
          if (!wave.packed || i % 2 === 1) laneY += item.h + waveGapY;
        });
        x += wave.w + gapX;
      }
      w = Math.max(w, x - gapX + right);
      row.bottom = y + tallest + overhead;
      y = row.bottom + rowSpacing;
    }
    const contentBottom = y - rowSpacing;
    const retryClearance = retryConnections.length ? 72 + (retryConnections.length - 1) * 44 : 0;
    return {items, connections, retryConnections, rows, left, w,
      contentBottom, h: contentBottom + retryClearance, parallel};
  }

  function anchor(item, output) {
    const task = item.node.node_type === 'task';
    return {
      x: item.x + (output ? item.w - (task ? 0 : 1) : (task ? 6 : 1)),
      y: item.y + (task ? 100 : item.h / 2)
    };
  }
  function roundedRoute(points) {
    const corners = points.filter(([x, y], i) => i === 0 || x !== points[i - 1][0] || y !== points[i - 1][1]);
    const toward = ([x, y], [nextX, nextY], radius) => {
      const distance = Math.abs(nextX - x) + Math.abs(nextY - y);
      const ratio = Math.min(radius / distance, .5);
      return (x + (nextX - x) * ratio) + ' ' + (y + (nextY - y) * ratio);
    };
    let d = 'M' + corners[0].join(' ');
    for (let i = 1; i < corners.length - 1; i++) {
      const corner = corners[i];
      d += 'L' + toward(corner, corners[i - 1], 10) +
        'Q' + corner.join(' ') + ' ' + toward(corner, corners[i + 1], 10);
    }
    return d + 'L' + corners.at(-1).join(' ');
  }

  function avenueY(row, lane) {
    const spacing = Math.min(9, 46 / Math.max(1, row.avenues - 1));
    return row.bottom + rowGap / 2 + (lane - (row.avenues - 1) / 2) * spacing;
  }

  function drawRetryConnections(group, level, projection) {
    const active = [...level.items.values()].some(({node}) =>
      node.status === 'awaiting_subagents' || blockedStatuses.has(node.status));
    const iso = layoutMode === 'iso';
    for (const {from, to, label, lane} of level.retryConnections) {
      const track = svg('g', {class: active ? 'retry-active' : 'retry-route', transform: projection}, group);
      const left = 12 + 9 * lane, right = level.w - left;
      const bottom = level.contentBottom + 44 + 44 * lane;
      const fromTask = from.node.node_type === 'task', toTask = to.node.node_type === 'task';
      const a = [from.x + from.w - (fromTask && !iso ? 2 : 5), from.y + (fromTask ? 132 : from.h - 40)];
      const b = [to.x + (toTask && iso ? 8 : 0), to.y + (toTask ? 132 : to.h - 40)];
      const depart = from.x + from.columnWidth + 12, approach = to.x - 12;
      const lower = level.rows[from.row].bottom + 16, upper = level.rows[to.row].bottom + 16;
      const route = roundedRoute([a, [depart, a[1]], [depart, lower], [right, lower], [right, bottom],
        [left, bottom], [left, upper], [approach, upper], [approach, b[1]], b]);
      path(track, route, {class: 'retry-outline'});
      path(track, route, {class: 'retry-wire', 'marker-end': 'url(#retry-arrow)'});
      const caption = '↻ ' + titleCase(label || 'If fails → retry');
      const review = from.node.kind === 'gate' || /reject|review|approval/i.test(label || '');
      const size = review ? 26 : 21;
      const height = size + 12;
      const captionWidth = Math.ceil(signMetrics.measureText(caption).width * size / signFont);
      const width = Math.min(captionWidth + 24, right - left - 104);
      const middle = (left + right) / 2;
      const fit = width < captionWidth + 24 ? {textLength: width - 24, lengthAdjust: 'spacingAndGlyphs'} : {};
      rect(track, middle - width / 2, bottom - height / 2, width, height, '#fff1cc',
        {rx: height / 2, class: 'retry-label-bg'});
      text(track, caption, middle, bottom + size / 3, size,
        {class: 'retry-label-text', 'text-anchor': 'middle', ...fit});
      const villain = review ? 'villain-thanos' : 'villain-doom';
      drawHero(track, villain, middle + width / 2 + 6, bottom - 18, 34, 36);
    }
  }

  function drawLevel(group, level, projection = '') {
    const routeTransform = projection ? {transform: projection} : {};
    for (const {from, to, bypass, exit, entry, express} of level.connections) {
      const a = anchor(from, true);
      const b = anchor(to, false);
      const outside = from.x + from.columnWidth + (from.innerExit ? 17 : 22);
      const approach = to.x - (to.innerEntry ? 17 : 26);
      let d;
      if (to.row > from.row) {
        const corridorY = avenueY(level.rows[from.row], exit);
        const route = [[a.x, a.y], [outside, a.y], [outside, corridorY]];
        let arrivalY = corridorY;
        if (entry !== undefined) {
          const through = level.left - 48 - express * 7;
          arrivalY = avenueY(level.rows[to.row - 1], entry);
          route.push([through, corridorY], [through, arrivalY]);
        }
        route.push([approach, arrivalY], [approach, b.y], [b.x - 8, b.y]);
        d = roundedRoute(route);
      } else if (bypass === undefined) {
        const middle = (outside + b.x) / 2;
        d = 'M' + a.x + ' ' + a.y + 'H' + outside + 'C' + middle + ' ' + a.y + ' ' +
          middle + ' ' + b.y + ' ' + (b.x - 8) + ' ' + b.y;
      } else {
        const lane = level.rows[from.row].top + 9 + 11 * bypass;
        d = roundedRoute([[a.x, a.y], [outside, a.y], [outside, lane],
          [approach, lane], [approach, b.y], [b.x - 8, b.y]]);
      }
      path(group, d, {class: 'link-outline', ...routeTransform});
      path(group, d, {class: 'link-wire', 'marker-end': 'url(#street-arrow)', ...routeTransform});
      if (from.node.status === 'running' || to.node.status === 'running') {
        path(group, d, {class: 'link-pulse', ...routeTransform});
      }
    }
    if (level.retryConnections.length) drawRetryConnections(group, level, projection);
    for (const item of level.items.values()) {
      const wrapper = svg('g', {class: 'node ' + statusClass(item.node.status),
        transform: projection + 'translate(' + item.x + ' ' + item.y + ')'}, group);
      const target = svg('g', {class: 'node-target', tabindex: 0, role: 'button',
        'aria-label': item.node.path + ', ' + statusLabel(item.node.status)}, wrapper);
      const mission = attention?.node_path === item.node.path ? attention.headline : '';
      const nested = subGraph(item.node);
      const detail = [mission, item.node.detail || item.node.error_signature,
        nested ? 'Open the Inspector for sub-tasks or double-click to zoom in' : ''].filter(Boolean).join(' · ');
      if (detail) svg('title', {}, target).textContent = shorten(detail, 200);
      targets.set(item.node.path, {target, node: item.node, w: item.w, h: item.h});
      rect(target, 0, 0, item.w, item.h, 'transparent', {class: 'hit'});
      if (item.inside) drawDistrict(wrapper, target, item);
      else if (layoutMode === 'iso') drawIsoBuilding(target, item.node);
      else drawBuilding(target, item.node);
      rect(target, 2, 2, item.w - 4, item.h - 4, 'none', {rx: 10, class: 'selection'});
      const inspect = () => item.node.node_type === 'task' ? focusNode(item.node.path) : select(item.node.path);
      target.addEventListener('click', event => { if (!gestureDragged || !event.detail) inspect(); });
      if (nested) target.addEventListener('dblclick', event => {
        event.preventDefault();
        if (!gestureDragged) openSubGraph(item.node);
      });
      target.addEventListener('keydown', event => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          inspect();
        }
      });
    }
  }

  function point(x, y, z = 0) {
    return [x - z * plumbX, y - z * plumbY];
  }

  function polygon(group, vertices, fill, attrs = {}) {
    return svg('polygon', {points: vertices.map(vertex => vertex.join(',')).join(' '),
      fill, ...attrs}, group);
  }

  function drawIsoBlock(group, x, y, w, d, base, height, colors) {
    const right = x + w, front = y + d, roof = base + height;
    const [topColor, rightColor, frontColor] = colors;
    const line = {class: 'comic-stroke'};
    polygon(group, [point(right, y, roof), point(right, front, roof),
      point(right, front, base), point(right, y, base)], rightColor, line);
    polygon(group, [point(x, front, roof), point(right, front, roof),
      point(right, front, base), point(x, front, base)], frontColor, line);
    polygon(group, [point(x, y, roof), point(right, y, roof),
      point(right, front, roof), point(x, front, roof)], topColor, line);
  }

  function drawDistrict(wrapper, target, item) {
    const {node, inside, w, h} = item;
    const [label, bright, paper, accent] = districts[node.node_type] || districts.subgraph;
    const iso = layoutMode === 'iso';
    if (iso) {
      rect(target, 8, 19, w - 9, h - 20, '#172033', {rx: 6, opacity: .27});
      drawIsoBlock(target, 7, 19, w - 14, h - 25, 0, 12, [paper, accent, bright]);
      rect(target, 3.5, 5.2, w - 14, 50, bright, {class: 'comic-stroke'});
    } else {
      rect(target, 5, 5, w - 5, h - 5, '#172033', {rx: 13});
      rect(target, 1.5, 1.5, w - 7, h - 7, paper, {rx: 13, class: 'comic-stroke'});
      path(target, 'M14 2H' + (w - 18) + 'q12 0 12 12v40H2V14Q2 2 14 2Z',
        {fill: bright, class: 'comic-stroke'});
    }
    text(target, shorten(node.id, Math.max(13, Math.floor((w - 106) / 10))), 16, 25, 21);
    text(target, label.toUpperCase() + (subGraph(node) ? ' ↗' : ''), 17, 43, 10, {'letter-spacing': 1.4});
    const status = buildingStatus(node);
    const badgeWidth = Math.max(66, status.length * 6 + 14);
    rect(target, w - badgeWidth - 15, 14, badgeWidth, 22, 'white', {rx: 7, class: 'comic-stroke status-fill'});
    text(target, status, w - badgeWidth / 2 - 15, 29, 9, {'text-anchor': 'middle'});
    if (node.node_type === 'loop_subgraph') {
      rect(target, 10, 59, w - 25, h - (iso ? 87 : 70), 'none', {rx: 15, class: 'loop-track'});
      const badge = '↻ max ' + (node.max_retries ?? '—');
      rect(target, w - 80, 39, 64, 20, '#ffefd1', {rx: 6, class: 'comic-stroke'});
      text(target, badge, w - 48, 53, 10, {'text-anchor': 'middle'});
    }
    const offsetX = (w - inside.w) / 2;
    if (inside.parallel) {
      for (const branch of inside.items.values()) {
        const x = offsetX + branch.x - 5, y = 63 + branch.y - 4;
        rect(target, x, y, branch.w + 10, branch.h + 8, '#c4f0e1', {rx: 8});
        path(target, 'M' + (x + 2) + ' ' + (y + branch.h) + 'V' + (y + 8) +
          'Q' + (x + 2) + ' ' + (y + 2) + ' ' + (x + 8) + ' ' + (y + 2) + 'H' + (x + branch.w + 2), {
          fill: 'none', stroke: accent, 'stroke-width': 2
        });
      }
    }
    if (inside.items.size) {
      drawLevel(svg('g', {transform: 'translate(' + offsetX + ' 63)'}, wrapper), inside);
    } else {
      text(target, 'No stages yet', w / 2, 100, 13, {'text-anchor': 'middle'});
    }
  }

  function heroFor(node, kind) {
    const id = String(node.id || '').toLowerCase();
    if (node.status === 'budget_exceeded' || String(node.error_signature || '').includes('rejected')) {
      return 'villain-thanos';
    }
    if (node.status === 'failed' || node.status === 'circuit_broken') return 'villain-doom';
    if (kind === 'gate') return 'gate';
    if (kind === 'verifier') {
      if (/(build|validate|batch|review|shield|cap|check)/.test(id)) return 'captain';
      if (/(test|run|qa|runtime)/.test(id)) return 'thor';
      return 'verifier';
    }
    if (kind === 'worker') {
      if (/(fix|commit|upload|publish|thor|bolt|repair|rig)/.test(id)) return 'thor';
      return 'worker';
    }
    if (kind === 'code') {
      if (/(prepare|release|package|shield|cap)/.test(id)) return 'captain';
      return 'code';
    }
    return kind;
  }

  function drawHero(group, heroId, x, y, width, height) {
    const hero = svg('g', {transform: 'translate(' + x + ' ' + y + ')', class: 'hero-art'}, group);
    const href = heroId.startsWith('villain-') ? '#' + heroId : '#hero-' + heroId;
    svg('use', {href, x: 0, y: 0, width, height,
      class: heroId === 'worker' || heroId === 'thor' ? 'flyer' : ''}, hero);
  }

  function drawBuildingName(group, id, x, y, w) {
    const name = id.replace(/[_-]+/g, ' ').trim() || id;
    const available = w - 10;
    const widthOf = line => signMetrics.measureText(line).width;
    let lines = [name];
    if (widthOf(name) > available) {
      const words = name.split(/\s+/);
      let widest = Infinity;
      for (let split = 1; split < words.length; split++) {
        const pair = [words.slice(0, split).join(' '), words.slice(split).join(' ')];
        const width = Math.max(...pair.map(widthOf));
        if (width < widest) {
          widest = width;
          lines = pair;
        }
      }
    }
    rect(group, x, y, w, 35, '#fff6e4', {rx: 5, class: 'comic-stroke'});
    lines.forEach((line, i) => {
      const fitLine = widthOf(line) > available ? {textLength: available, lengthAdjust: 'spacingAndGlyphs'} : {};
      text(group, line, x + w / 2, y + (lines.length === 1 ? 23 : 15 + i * 15),
        signFont, {'text-anchor': 'middle', ...fitLine});
    });
  }

  function drawIsoBuilding(group, node) {
    const kind = Object.hasOwn(roles, node.kind) ? node.kind : 'code';
    const [paint, trim] = roles[kind];
    const line = {class: 'comic-stroke'};
    const facade = 'matrix(1 0 ' + plumbX + ' ' + plumbY + ' ';
    rect(group, 9, 119, 187, 30, '#172033', {rx: 5, opacity: .28});
    drawIsoBlock(group, 9, 118, 183, 26, 0, 6, ['#ffefcf', '#b37c62', '#e1b695']);
    drawIsoBlock(group, 33, 84, 122, 37, 6, 62, [trim, '#45516a', paint]);
    const [x, y] = point(33, 121, 68);
    const face = svg('g', {transform: facade + x + ' ' + y + ')'}, group);
    drawBuildingName(face, node.title || node.id, 4, 4, 114);
    for (const wx of [10, 47, 84]) {
      rect(face, wx, 44, 28, 13, '#fff', {rx: 2, ...line});
      rect(face, wx + 3, 47, 22, 7, '#e4f4ff', {class: 'window'});
      path(face, 'M' + (wx + 14) + ' 46v9', {stroke: '#fff', 'stroke-width': 2});
    }
    if (node.resource) {
      rect(group, 28, 31, 59, 16, '#ffe18a', {rx: 4, ...line});
      text(group, shorten(node.resource, 9), 57, 43, 9, {'text-anchor': 'middle'});
    }
    const heroFacing = svg('g', {transform: facade + '131 42)'}, group);
    drawHero(heroFacing, heroFor(node, kind), 0, 0, 46, 49);
    drawBuildingStatus(group, node);
  }

  function drawBuilding(group, node) {
    const kind = Object.hasOwn(roles, node.kind) ? node.kind : 'code';
    const [paint, trim, icon] = roles[kind];
    const line = {class: 'comic-stroke'};
    // Pavement and bold offset shadows keep the silhouette clear against the town.
    rect(group, 1, 127, 199, 17, '#172033', {rx: 6});
    rect(group, 1, 123, 196, 17, '#ffefcf', {rx: 6, ...line});
    path(group, 'M6 134h188', {fill: 'none', stroke: '#d09c75', 'stroke-width': 1.5});
    rect(group, 13, 34, 151, 98, '#172033', {rx: 5});
    rect(group, 9, 30, 151, 98, paint, {rx: 5, ...line});
    path(group, 'M9 37 20 20h128l13 17Z', {fill: trim, ...line});
    const strip = layoutMode === 'strip';
    if (strip) {
      rect(group, 19, 39, 128, 25, '#fff6e4', {rx: 5, ...line});
      text(group, icon, 30, 57, 15, {'text-anchor': 'middle'});
      text(group, shorten(node.title || node.id, 14), 42, 56, 13);
    } else {
      drawBuildingName(group, node.title || node.id, 16, 39, 137);
    }
    const sill = strip ? 71 : 78;
    for (const x of [22, 63, 104]) {
      rect(group, x, sill, 31, 90 - sill, '#fff', {rx: 3, ...line});
      rect(group, x + 3, sill + 3, 25, 84 - sill, '#e4f4ff', {rx: 1.5, class: 'window'});
      path(group, 'M' + (x + 15) + ' ' + (sill + 2) + 'V88', {stroke: '#fff', 'stroke-width': 2});
    }
    path(group, 'M19 91h129l9 16H10Z', {fill: '#fff6e4', ...line});
    for (const x of [19, 50, 81, 112]) {
      path(group, 'M' + x + ' 92h15l-3 14h-19Z', {fill: trim});
    }
    path(group, 'M10 107h147', {stroke: '#172033', 'stroke-width': 2.5});
    rect(group, 56, 108, 43, 20, '#465671', {rx: 2, ...line});
    path(group, 'M78 110v16', {stroke: '#fff4e0', 'stroke-width': 2});
    if (node.resource) {
      path(group, 'M16 19q12 1 21-4 9 5 21 4v15q-12 6-21 7-9-1-21-7Z',
        {fill: '#ffe18a', ...line});
      text(group, shorten(node.resource, 7), 37, 31, 8, {'text-anchor': 'middle'});
    }
    const heroId = heroFor(node, kind);
    drawHero(group, heroId, heroId === 'gate' ? 142 : 132, heroId === 'gate' ? 64 : -2, 59, 61);
    drawBuildingStatus(group, node);
  }

  function drawBuildingStatus(group, node) {
    const status = buildingStatus(node);
    path(group, 'M104 109h83l-4 5 11 4-9 5 4 6h-84l3-5-8-4 7-5Z',
      {fill: 'white', class: 'comic-stroke status-fill'});
    text(group, status, 147, 122, 8.7, {'text-anchor': 'middle',
      ...(status.length > 13 ? {textLength: 70, lengthAdjust: 'spacingAndGlyphs'} : {})});
    const attempts = node.attempts || 0;
    rect(group, 17, 134, 115, 16, '#fff', {rx: 6, class: 'comic-stroke'});
    text(group, seconds(node.duration_s) + '  •  ' + attempts + (attempts === 1 ? ' attempt' : ' attempts'), 74, 145, 10,
      {'text-anchor': 'middle'});
    const progress = node.sub_progress;
    if (progress?.total > 0) {
      const ratio = progress.done / progress.total;
      const mark = ratio === 1 ? ' ✓' : progress.running > 0 ? ' ⚡' : '';
      rect(group, 15, 108, 81, 20, '#fff6e4', {rx: 5, class: 'comic-stroke'});
      text(group, progress.done + '/' + progress.total + mark + ' ↗', 55, 119, 9.5, {'text-anchor': 'middle'});
      rect(group, 22, 122, 67, 3, '#dccdb4', {rx: 1.5});
      if (ratio > 0) rect(group, 22, 122, 67 * ratio, 3, '#2f915b', {rx: 1.5});
    }
  }

  function drawBeacons() {
    if (!spotlights.size) return;
    const layer = svg('g', {class: 'beacons', 'aria-hidden': 'true'}, world);
    const toWorld = world.getCTM().inverse();
    const iso = layoutMode === 'iso';
    for (const [taskPath, kind] of spotlights) {
      const entry = targets.get(taskPath);
      if (!entry) continue;
      const {target, node} = entry;
      const transform = toWorld.multiply(target.getCTM());
      const {a, b, c, d, e, f} = transform;
      const marker = svg('g', {class: statusClass(node.status)}, layer);
      const halo = svg('g', {class: 'beacon-halo', transform: 'matrix(' + [a, b, c, d, e, f].join(' ') + ')'}, marker);
      const y = iso ? 124 : 129;
      rect(halo, 2, y, 197, 151 - y, 'none', {rx: 8, class: 'beacon-outline'});
      rect(halo, 2, y, 197, 151 - y, 'none', {rx: 8, class: 'beacon-light'});

      // Project the roof anchor and keep the pin and its lettering upright.
      const roof = iso ? point(94, 102, 68) : [84, 20];
      const anchor = new DOMPoint(...roof).matrixTransform(transform);
      const placement = svg('g', {transform: 'translate(' + anchor.x + ' ' + anchor.y + ')'}, marker);
      const pin = svg('g', {class: 'beacon-pin'}, placement);
      path(pin, 'M-43-35H43Q49-35 49-29V-16Q49-10 43-10H8L0-2-8-10H-43Q-49-10-49-16V-29Q-49-35-43-35Z',
        {class: 'comic-stroke'});
      text(pin, kind.pin, 0, -18, 12, {'text-anchor': 'middle', 'letter-spacing': .8});
    }
  }

  function applyCamera() {
    world.setAttribute('transform', 'translate(' + camera.x + ' ' + camera.y + ') scale(' + camera.scale + ')');
  }
  function toolbarInset() {
    const inset = controls.offsetTop + controls.offsetHeight + 10;
    stage.style.setProperty('--inspector-top', inset + 'px');
    return inset;
  }
  function viewportSpace(wrapped) {
    const top = toolbarInset() + 8;
    const bottom = layoutMode === 'iso' || wrapped ? 24 : 60;
    const {clientWidth, clientHeight} = viewport;
    if (!clientWidth || !clientHeight) {
      const aspect = document.body.classList.contains('full') ? 1.8 : 1.45;
      return {w: aspect, h: 1, aspect, top, bottom};
    }
    const w = Math.max(1, clientWidth - 2 * mapInsetX);
    const h = Math.max(1, clientHeight - top - bottom);
    return {w, h, aspect: w / h, top, bottom};
  }
  function measureTownSpace() {
    const wrapped = stage.hasAttribute('data-wrapped');
    const current = viewportSpace(wrapped);
    if (layoutMode === 'strip') return {single: current, wrapped: current};
    // Wrapped embeds have a different CSS stage height. Score both layouts
    // against the space they will have once rendered.
    stage.toggleAttribute('data-wrapped', !wrapped);
    const alternate = viewportSpace(!wrapped);
    stage.toggleAttribute('data-wrapped', wrapped);
    return wrapped ? {single: alternate, wrapped: current} : {single: current, wrapped: alternate};
  }
  function fit() {
    cancelAnimationFrame(cameraFrame);
    const space = viewportSpace(stage.hasAttribute('data-wrapped'));
    if (!targets.size) return;
    const {clientWidth: w, clientHeight: h} = viewport;
    const bounds = world.getBBox();
    camera.scale = Math.min(1.5, space.w / Math.max(1, bounds.width), space.h / Math.max(1, bounds.height));
    camera.x = w / 2 - (bounds.x + bounds.width / 2) * camera.scale;
    camera.y = (space.top + h - space.bottom) / 2 - (bounds.y + bounds.height / 2) * camera.scale;
    applyCamera();
  }
  function focusNode(path, {scroll = true} = {}) {
    if (!targets.has(path) && path?.includes('::')) {
      const parent = nodeIndex.get(path.split('::', 1)[0]);
      if (parent && nodeIndex.has(path)) openSubGraph(parent, path, {scroll});
      return;
    }
    if (!targets.has(path) && drillStack.length && nodeIndex.has(path)) drillTo([]);
    const entry = targets.get(path);
    if (!entry) return;
    select(path, {located: true});
    cameraMoved = true;
    cancelAnimationFrame(cameraFrame);

    const toWorld = world.getCTM().inverse().multiply(entry.target.getCTM());
    const center = new DOMPoint(entry.w / 2, entry.h / 2).matrixTransform(toWorld);
    const {clientWidth: w, clientHeight: h} = viewport;
    const sheet = !docked() && inspector.offsetLeft === 0;
    const right = docked() || sheet ? w : inspector.offsetLeft - 8;
    const bottom = sheet ? inspector.offsetTop - 8 : h;
    const scale = sheet ? .95 : 1.1;
    const destination = {x: right / 2 - center.x * scale,
      y: (toolbarInset() + bottom) / 2 - center.y * scale, scale};
    if (scroll && !docked()) {
      stage.scrollIntoView({block: 'nearest', behavior: reducedMotion.matches ? 'auto' : 'smooth'});
    }
    if (reducedMotion.matches) {
      Object.assign(camera, destination);
      applyCamera();
      return;
    }

    const start = {...camera};
    const started = performance.now();
    function move(now) {
      const time = Math.min(1, Math.max(0, (now - started) / 360));
      const progress = 1 - (1 - time) ** 3;
      for (const key of ['x', 'y', 'scale']) camera[key] = start[key] + (destination[key] - start[key]) * progress;
      applyCamera();
      if (time < 1) cameraFrame = requestAnimationFrame(move);
    }
    cameraFrame = requestAnimationFrame(move);
  }
  function zoomAt(ratio, x = viewport.clientWidth / 2, y = viewport.clientHeight / 2, toX = x, toY = y) {
    cancelAnimationFrame(cameraFrame);
    cameraMoved = true;
    const next = Math.min(5, Math.max(Math.min(.06, camera.scale), camera.scale * ratio));
    camera.x = toX - (x - camera.x) * next / camera.scale;
    camera.y = toY - (y - camera.y) * next / camera.scale;
    camera.scale = next;
    applyCamera();
  }
  function pinch() {
    const [first, second] = pointers.values();
    if (!second) return null;
    const bounds = viewport.getBoundingClientRect();
    return {
      x: (first.x + second.x) / 2 - bounds.left,
      y: (first.y + second.y) / 2 - bounds.top,
      distance: Math.max(1, Math.hypot(first.x - second.x, first.y - second.y))
    };
  }
  function startDrag() {
    cancelAnimationFrame(cameraFrame);
    gestureDragged = true;
    cameraMoved = true;
    lastSkyTap = null;
    viewport.classList.add('dragging');
    for (const id of pointers.keys()) viewport.setPointerCapture(id);
  }
  viewport.addEventListener('pointerdown', event => {
    if (event.button !== 0 || (pointers.size && event.pointerType !== 'touch')) return;
    if (!pointers.size) gestureDragged = false;
    pointers.set(event.pointerId, {
      x: event.clientX, y: event.clientY, startX: event.clientX, startY: event.clientY,
      sky: !event.target.closest('.node-target')
    });
    if (pointers.size > 1) startDrag();
  });
  viewport.addEventListener('pointermove', event => {
    const pointer = pointers.get(event.pointerId);
    if (!pointer) return;
    const before = pinch();
    const dx = event.clientX - (gestureDragged ? pointer.x : pointer.startX);
    const dy = event.clientY - (gestureDragged ? pointer.y : pointer.startY);
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    if (before) {
      const after = pinch();
      zoomAt(after.distance / before.distance, before.x, before.y, after.x, after.y);
    } else {
      if (!gestureDragged && Math.hypot(dx, dy) < 5) return;
      if (!gestureDragged) startDrag();
      camera.x += dx;
      camera.y += dy;
      applyCamera();
    }
  });
  function endPointer(event) {
    const pointer = pointers.get(event.pointerId);
    if (!pointer) return;
    pointers.delete(event.pointerId);
    if (pointers.size) return;
    viewport.classList.remove('dragging');
    if (event.type === 'pointerup' && event.pointerType === 'touch' && !gestureDragged && pointer.sky) {
      const tap = {time: performance.now(), x: event.clientX, y: event.clientY};
      if (lastSkyTap && tap.time - lastSkyTap.time < 350 &&
          Math.hypot(tap.x - lastSkyTap.x, tap.y - lastSkyTap.y) < 32) {
        fitTown();
        lastSkyTap = null;
      } else {
        lastSkyTap = tap;
      }
    } else {
      lastSkyTap = null;
    }
  }
  viewport.addEventListener('pointerup', endPointer);
  viewport.addEventListener('pointercancel', endPointer);
  viewport.addEventListener('pointerleave', event => {
    if (!viewport.hasPointerCapture(event.pointerId)) endPointer(event);
  });
  viewport.addEventListener('lostpointercapture', event => {
    if (event.target === viewport) endPointer(event);
  });
  viewport.addEventListener('wheel', event => {
    if (!event.deltaY) return;
    event.preventDefault();
    const bounds = viewport.getBoundingClientRect();
    zoomAt(event.deltaY < 0 ? 1.14 : 1 / 1.14, event.clientX - bounds.left, event.clientY - bounds.top);
  }, {passive: false});
  $('zoom-in').addEventListener('click', () => zoomAt(1.25));
  $('zoom-out').addEventListener('click', () => zoomAt(.8));
  function fitTown() {
    cameraMoved = false;
    fit();
  }
  $('fit-town').addEventListener('click', fitTown);
  window.addEventListener('resize', () => {
    // Reflow on aspect changes large enough to affect the street layout.
    const {aspect} = viewportSpace(stage.hasAttribute('data-wrapped'));
    if (layoutMode !== 'strip' && Math.abs(Math.log(aspect / layoutSpace.aspect)) > .14) {
      const previousSpace = layoutSpace;
      const entry = cameraMoved && targets.get(selected);
      const anchor = entry && new DOMPoint(entry.w / 2, entry.h / 2);
      const previous = anchor && anchor.matrixTransform(entry.target.getCTM());
      redrawTown();
      if (previous) {
        const current = anchor.matrixTransform(targets.get(selected).target.getCTM());
        const x = mapInsetX + (previous.x - mapInsetX) * layoutSpace.w / previousSpace.w;
        const y = layoutSpace.top + (previous.y - previousSpace.top) * layoutSpace.h / previousSpace.h;
        cancelAnimationFrame(cameraFrame);
        camera.x += x - current.x;
        camera.y += y - current.y;
        applyCamera();
      }
    }
    if (!cameraMoved) fit();
  });
  for (const button of layoutButtons) {
    button.addEventListener('click', () => {
      if (layoutMode !== button.dataset.layout) {
        layoutMode = button.dataset.layout;
        for (const choice of layoutButtons) {
          choice.setAttribute('aria-pressed', String(choice === button));
        }
        redrawTown();
      }
      fitTown();
    });
  }
  $('theme').addEventListener('click', event => {
    const night = document.body.dataset.theme === 'day';
    document.body.dataset.theme = night ? 'night' : 'day';
    event.currentTarget.setAttribute('aria-pressed', String(night));
    event.currentTarget.textContent = night ? '☀ Daytime' : '☾ Night Patrol';
  });

  function element(tag, value) {
    const el = document.createElement(tag);
    el.textContent = value;
    return el;
  }
  function hoverTask(path) {
    targets.get(hoveredTask)?.target.classList.remove('hovered');
    hoveredTask = path;
    targets.get(path)?.target.classList.add('hovered');
  }
  function createRosterRow(path) {
    const row = document.createElement('li');
    const button = element('button', '');
    button.type = 'button';
    button.className = 'roster-row';
    for (const field of ['step', 'status', 'name', 'spotlight', 'timing', 'detail']) {
      const span = element('span', '');
      span.className = 'roster-' + field;
      button.append(span);
    }
    const [, status, name] = button.children;
    const badge = element('span', '');
    badge.className = 'roster-badge';
    status.append(badge);
    const breadcrumb = element('span', '');
    breadcrumb.className = 'roster-breadcrumb';
    name.append(breadcrumb, element('strong', ''));
    button.addEventListener('click', () => focusNode(path));
    button.addEventListener('pointerenter', event => { if (event.pointerType !== 'touch') hoverTask(path); });
    button.addEventListener('pointerleave', () => { if (hoveredTask === path) hoverTask(null); });
    row.append(button);
    return row;
  }
  function applyRosterFilter() {
    let visible = 0;
    for (const [path, row] of rosterRows) {
      const group = row.dataset.group;
      row.hidden = rosterFilter !== 'all' && (rosterFilter === 'active' ?
        group !== 'running' && group !== 'waiting' : group !== rosterFilter);
      if (!row.hidden) visible++;
      else if (hoveredTask === path) hoverTask(null);
    }
    rosterList.hidden = !visible;
    const empty = $('roster-empty');
    empty.hidden = Boolean(visible);
    empty.textContent = tasks.length ? 'No tasks match this filter.' : 'No tasks to display.';
  }
  function setRosterFilter(filter) {
    rosterFilter = filter;
    for (const button of rosterFilters.children) button.setAttribute('aria-pressed', String(button.dataset.filter === filter));
    applyRosterFilter();
  }
  rosterFilters.addEventListener('click', event => {
    const filter = event.target.closest('button')?.dataset.filter;
    if (filter) setRosterFilter(filter);
  });

  function renderRoster() {
    const paths = new Set(tasks.map(task => task.path));
    for (const [path, row] of rosterRows) {
      if (paths.has(path)) continue;
      if (hoveredTask === path) hoverTask(null);
      row.remove();
      rosterRows.delete(path);
    }
    tasks.forEach(({node, path, parents}, i) => {
      const row = rosterRows.get(path) || createRosterRow(path);
      rosterRows.set(path, row);
      if (rosterList.children[i] !== row) rosterList.insertBefore(row, rosterList.children[i] || null);
      row.dataset.group = taskGroup(node.status);
      const button = row.firstElementChild;
      if (path === selected) button.setAttribute('aria-current', 'true');
      const [step, status, name, pill, timing, subtitle] = button.children;
      const [breadcrumb, id] = name.children;
      const color = statusClass(node.status);
      step.textContent = '#' + (i + 1);
      status.className = 'roster-status ' + color;
      const label = statusLabel(node.status);
      status.title = label;
      status.lastElementChild.textContent = {'Awaiting subagents': 'Paused', 'Circuit broken': 'Failed',
        'Budget exceeded': 'Budget'}[label] || label;
      breadcrumb.textContent = parents.length ? parents.map(titleCase).join(' › ') + ' › ' : '';
      id.textContent = titleCase(node.id);
      name.title = breadcrumb.textContent + id.textContent;
      const kind = frontierKinds.find(kind => kind.group !== 'pending' && kind.group === row.dataset.group);
      pill.hidden = !kind;
      pill.className = 'roster-spotlight ' + color;
      pill.textContent = kind?.pill || '';
      timing.textContent = (node.attempts ?? 0) + '× · ' + seconds(node.duration_s);
      const context = attention?.node_path === path ? attention : null;
      const taskDetail = node.detail || node.error_signature || '';
      const mission = context?.headline !== id.textContent ? context?.headline : null;
      subtitle.textContent = [mission, context?.detail || taskDetail].filter(Boolean).join(' · ');
      subtitle.title = subtitle.textContent;
      subtitle.hidden = !subtitle.textContent;
    });
    applyRosterFilter();
  }

  function revealRosterRow(path, current = true) {
    const row = rosterRows.get(path);
    if (!row) return;
    if (current) row.firstElementChild.setAttribute('aria-current', 'true');
    if (roster.hidden || row.hidden) return;
    const list = rosterList.getBoundingClientRect();
    const bounds = row.getBoundingClientRect();
    const inset = 6;
    const offset = bounds.top < list.top + inset ? bounds.top - list.top - inset :
      Math.max(0, bounds.bottom - list.bottom + inset);
    if (offset) rosterList.scrollBy({top: offset, behavior: reducedMotion.matches ? 'auto' : 'smooth'});
  }

  function shellQuote(value) {
    return /^[\w@%+=:,./-]+$/.test(value) && !value.startsWith('-') ?
      value : "'" + String(value).replace(/'/g, "'\"'\"'") + "'";
  }
  function actionCommands(node) {
    if (!snapshot.workflow_ref || node.path.includes('::')) return null;
    // The CLI can retry splitter descendants. Checkpointing stops at the outer splitter.
    const parts = node.path.split('/');
    let checkpoint = node.path;
    for (let n = 1; n < parts.length; n++) {
      const parent = parts.slice(0, n).join('/');
      if (targets.get(parent)?.node.node_type === 'splitter') {
        checkpoint = parent;
        break;
      }
    }
    let base = 'kgraph-town run ' + shellQuote(snapshot.workflow_ref);
    if (snapshot.project_dir) {
      base = 'cd -- ' + shellQuote(snapshot.project_dir) + ' && ' + base;
      for (const [flag, value] of [['--state', snapshot.state_file], ['--report', snapshot.report_file],
        ['--factory', snapshot.factory === 'build_graph' ? null : snapshot.factory]]) {
        if (value) base += ' ' + flag + ' ' + shellQuote(value);
      }
    }
    return {
      retry: base + ' --retry ' + shellQuote(node.path),
      until: base + ' --until ' + shellQuote(checkpoint), checkpoint
    };
  }
  function select(value, {located = false} = {}) {
    const restoreFocus = docked() && roster.contains(document.activeElement);
    targets.get(selected)?.target.classList.remove('selected');
    rosterRows.get(selected)?.firstElementChild.removeAttribute('aria-current');
    selected = value;
    stage.toggleAttribute('data-located', located);
    targets.get(selected)?.target.classList.add('selected');
    drawInspector();
    revealRosterRow(selected);
    if (restoreFocus && !inspector.hidden) $('close-inspector').focus({preventScroll: true});
  }
  function drawInspector() {
    const node = targets.get(selected)?.node;
    syncInspector();
    if (!node) return;
    $('inspect-title').textContent = node.title || titleCase(node.id);
    const fields = $('inspect-fields');
    fields.replaceChildren();
    const facts = [
      ['Path', node.path], ['Status', statusLabel(node.status)], ['Kind', titleCase(node.kind)],
      ['Attempts', node.attempts ?? 0], ['Duration', seconds(node.duration_s)],
      ['Resource lock', node.resource], ['Input hash', node.input_hash],
      ['Error signature', node.error_signature], ['Detail', node.detail],
      ['Started', node.started_at], ['Updated', node.updated_at]
    ];
    if (node.path.includes('::')) {
      facts.push(['Agent', node.agent], ['Progress', node.progress == null ? null : node.progress + '%']);
    }
    if (attention?.node_path === selected) facts.unshift(['Mission', attention.headline]);
    $('inspect-input').hidden = attention?.input_gate?.node_path !== selected &&
      (!node.input_gate || Boolean(node.input_gate.submitted));
    renderInspectorSubtasks(node);
    const parent = nodeIndex.get(node.path.slice(0, node.path.lastIndexOf('/')));
    const stageTitle = path => titleCase(nodeIndex.get(path)?.id ?? path.split('/').at(-1));
    for (const edge of parent?.edges || []) {
      if (edge.kind === 'retry' && edge.from === node.path) {
        facts.push(['On failure', 'Routes back to ' + stageTitle(edge.to) + ' (' + edge.label + ')']);
      }
    }
    if (node.node_type === 'loop_subgraph') {
      for (const edge of node.edges.filter(edge => edge.kind === 'retry')) {
        const limit = node.max_retries == null ? '' : ' (max ' + node.max_retries + ')';
        facts.push(['Retry loop', stageTitle(edge.from) + ' → ' + stageTitle(edge.to) + limit]);
      }
    }
    for (const [name, value] of facts) fields.append(element('dt', name), element('dd', value ?? '—'));
    const outputs = $('inspect-outputs');
    outputs.replaceChildren();
    const digests = node.output_hashes || {};
    const names = new Set([...(node.outputs || []), ...Object.keys(digests)]);
    for (const name of names) {
      const line = element('li', name);
      if (Object.hasOwn(digests, name)) line.append(document.createElement('br'), element('code', digests[name]));
      outputs.append(line);
    }
    if (!names.size) outputs.append(element('li', 'No recorded outputs'));
    const commands = actionCommands(node);
    $('copy-retry').disabled = $('copy-until').disabled = !commands;
    $('copy-until').textContent = commands && commands.checkpoint !== node.path ? 'Copy until splitter' : 'Copy until';
    $('copy-note').textContent = !commands ? (node.path.includes('::') ? '' : 'A workflow path is required to run commands.') :
      (commands.checkpoint === node.path ? '' : 'Checkpoint: ' + commands.checkpoint);
    $('copy-result').textContent = '';
    $('copy-fallback').hidden = true;
  }
  function renderInspectorSubtasks(node) {
    const section = $('inspect-subtasks');
    const openButton = $('open-subgraph');
    const graph = subGraph(node);
    section.hidden = !graph;
    openButton.hidden = !graph;
    if (!graph) {
      openButton.onclick = null;
      return;
    }
    const progress = subProgress(node, graph);
    openButton.textContent = 'Open Sub-graph ↗';
    $('inspect-subtasks-title').textContent = 'Sub-tasks (' + progress.done + '/' + progress.total + ')';
    const bar = $('inspect-subtask-progress');
    bar.max = progress.total;
    bar.value = progress.done;
    const list = $('inspect-subtask-list');
    const scroll = list.scrollTop;
    const focused = list.contains(document.activeElement) && document.activeElement.dataset.path;
    list.replaceChildren(...graph.nodes.map(child => {
      const item = element('li', '');
      const button = element('button', '');
      button.type = 'button';
      button.dataset.path = child.path;
      const dot = element('span', '');
      dot.className = 'dot ' + statusClass(child.status);
      dot.title = statusLabel(child.status);
      dot.setAttribute('aria-label', dot.title);
      const label = element('span', '');
      label.append(element('strong', child.title || titleCase(child.id)));
      if (child.detail) label.append(element('small', child.detail));
      if (child.agent) {
        const agent = element('small', child.agent);
        agent.className = 'gate-pill';
        label.append(agent);
      }
      button.append(dot, label);
      button.addEventListener('click', () => openSubGraph(node, child.path));
      item.append(button);
      return item;
    }));
    list.scrollTop = scroll;
    if (focused !== false) [...list.querySelectorAll('button')].find(button => button.dataset.path === focused)?.focus();
    openButton.onclick = () => openSubGraph(node);
  }

  async function copyValue(value, field, status, button, label) {
    try {
      await navigator.clipboard.writeText(value);
      field.hidden = true;
      status.textContent = 'Copied';
    } catch {
      field.value = value;
      field.hidden = false;
      field.focus();
      field.select();
      if (document.execCommand?.('copy')) {
        field.hidden = true;
        status.textContent = 'Copied';
        button.focus();
      } else {
        status.textContent = label + ' selected for copying';
      }
    }
  }
  function copyCommand(key) {
    const commands = actionCommands(targets.get(selected).node);
    return copyValue(commands[key], $('copy-fallback'), $('copy-result'), $('copy-' + key), 'Command');
  }
  function closeInspector() {
    const path = selected;
    const target = targets.get(path)?.target;
    const row = rosterRows.get(path);
    select(null);
    if (docked()) {
      revealRosterRow(path, false);
      const control = row && !row.hidden ? row.firstElementChild : rosterFilters.querySelector('[aria-pressed=true]');
      control?.focus({preventScroll: true});
    } else {
      target?.focus({preventScroll: true});
    }
  }
  $('close-inspector').addEventListener('click', closeInspector);
  $('inspect-input').addEventListener('click', () => {
    if (attention?.input_gate?.node_path !== selected) {
      preferredInputPath = selected;
      renderAttention();
    }
    const form = $('input-gate');
    form.scrollIntoView({block: 'nearest'});
    form.focus({preventScroll: true});
  });
  document.addEventListener('keydown', event => {
    if (event.key !== 'Escape') return;
    if (selected !== null) closeInspector();
    else if (drillStack.length) drillTo(drillStack.slice(0, -1));
  });
  $('copy-retry').addEventListener('click', () => copyCommand('retry'));
  $('copy-until').addEventListener('click', () => copyCommand('until'));
  $('device-list')?.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    const url = button.closest('li').querySelector('a').href;
    copyValue(url, $('device-fallback'), $('device-copy-result'), button, 'Address');
  });

  function renderHud() {
    $('workflow-title').textContent = snapshot.title;
    document.title = snapshot.title + ' | Hero Town';
    const status = $('overall');
    status.className = 'overall ' + statusClass(snapshot.status);
    status.textContent = statusLabel(snapshot.status);
    const counts = snapshot.counts || {};
    $('count-passed').textContent = counts.passed || 0;
    $('count-cached').textContent = counts.cached || 0;
    $('count-running').textContent = counts.running || 0;
    $('count-failed').textContent = (counts.failed || 0) + (counts.circuit_broken || 0) + (counts.budget_exceeded || 0);
    $('count-pending').textContent = (counts.pending || 0) + (counts.awaiting_subagents || 0);
    const {worker_calls_used: used, max_worker_calls: max} = snapshot.summary || {};
    $('budget-label').textContent = (used ?? '—') + ' / ' + (max ?? (used == null ? '—' : '∞'));
    const bar = $('budget-meter');
    bar.max = Math.max(1, max ?? 1);
    bar.value = used ?? 0;
    bar.hidden = max == null;
    $('warning').textContent = snapshot.warning || '';
    $('warning').hidden = !snapshot.warning;
  }
  function renderReport() {
    const rows = {
      'slowest-nodes': (snapshot.slowest_nodes || []).map(row => row.node + ' · ' + seconds(row.duration_s) + ' · ' +
        row.attempts + (row.attempts === 1 ? ' attempt' : ' attempts')),
      'failure-signatures': (snapshot.failure_signatures || []).map(row => row.node + ' · ' +
        (row.error_signature || statusLabel(row.status)) + (row.detail ? ' · ' + row.detail : '')),
      'evolution-hints': snapshot.evolution_hints || []
    };
    for (const [id, entries] of Object.entries(rows)) {
      const list = $(id);
      list.replaceChildren(...entries.map(value => element('li', value)));
      if (!entries.length) {
        const empty = element('li', 'None recorded');
        empty.className = 'no-data';
        list.append(empty);
      }
    }
  }
  function redrawTown() {
    const focused = [...targets.entries()].find(([, entry]) => entry.target === document.activeElement)?.[0];
    targets.clear();
    world.replaceChildren();
    renderBreadcrumbs();
    townSpace = measureTownSpace();
    const graph = stageGraph();
    const level = measureLevel(graph.nodes, graph.edges, {root: true, parallel: Boolean(graph.parallel)});
    stage.dataset.layout = layoutMode;
    const wrapped = level.rows.length > 1;
    stage.toggleAttribute('data-wrapped', wrapped);
    layoutSpace = wrapped ? townSpace.wrapped : townSpace.single;
    // Project each district and street before measuring so empty corners
    // of the full grid don't count toward the isometric camera bounds.
    const projection = layoutMode === 'iso' ? 'matrix(1 ' + isoTilt + ' ' + isoSkew + ' ' + isoDepth + ' 0 0) ' : '';
    drawLevel(world, level, projection);
    drawBeacons();
    targets.get(selected)?.target.classList.add('selected');
    targets.get(hoveredTask)?.target.classList.add('hovered');
    targets.get(focused)?.target.focus({preventScroll: true});
  }

  function update(next, initial = false) {
    next = {title: 'Hero Town', status: 'pending', ...next,
      nodes: Array.isArray(next.nodes) ? next.nodes : [], edges: Array.isArray(next.edges) ? next.edges : []};
    const previousRef = snapshot?.workflow_ref;
    const previousMission = attention?.kind + ":" + attention?.node_path;
    const previousSelection = targets.get(selected)?.node;
    snapshot = next;
    renderHud();
    renderReport();
    const graph = JSON.stringify([next.nodes, next.edges]);
    const newGraph = graph !== graphSignature;
    const drillDepth = drillStack.length;
    if (newGraph) {
      nodeIndex.clear();
      indexNodes(next.nodes);
      const missing = drillStack.findIndex(path => !subGraph(nodeIndex.get(path)));
      if (missing !== -1) drillStack = drillStack.slice(0, missing);
      updateTaskProgress();
    }
    const newAttention = renderAttention();
    const missionChanged = previousMission !== attention?.kind + ":" + attention?.node_path;
    if (newGraph || missionChanged || initial) {
      graphSignature = graph;
      redrawTown();
      renderRoster();
      if (initial || drillDepth !== drillStack.length || (!fittedNodes && targets.size && !cameraMoved)) fit();
      fittedNodes ||= targets.size > 0;
    } else if (newAttention) {
      renderRoster();
    }
    $('empty').hidden = Boolean(stageGraph().nodes.length);
    $('empty').textContent = options.runs && !displayedRunId ? 'Run a workflow to add it to Hero Town' : 'No stages to display';
    if (selected !== null) {
      const current = targets.get(selected);
      if (!current) select(null);
      else if (newAttention || JSON.stringify(previousSelection) !== JSON.stringify(current?.node) ||
          previousRef !== snapshot.workflow_ref) {
        drawInspector();
      }
    }
  }

  function showLive(status, text) {
    $('live-badge').dataset.live = status;
    $('live-text').textContent = text;
  }
  function setPolling(active) {
    automatic = active;
    $('auto-refresh').setAttribute('aria-pressed', String(active));
    clearTimeout(timer);
    if (!active) inFlight?.abort();
    showLive(active ? 'active' : 'paused', active ? 'Live' : 'Paused');
    if (active) timer = setTimeout(refresh, options.pollMs);
  }
  function setRunLocation(id) {
    const url = new URL(location.href);
    if (id) url.searchParams.set('run', id);
    else url.searchParams.delete('run');
    if (url.href !== location.href) history.replaceState(null, '', url);
  }
  function highlightRun() {
    for (const button of $('run-bar').querySelectorAll('.run-choice')) {
      button.setAttribute('aria-current', String(button.dataset.run === activeRunId));
    }
  }
  function runStatus(status) {
    if (status === 'missing' || blockedStatuses.has(status)) return 'FAILED';
    if (taskGroup(status) === 'done') return 'PASSED';
    return status === 'running' ? 'RUNNING' : 'WAITING';
  }
  const relativeTime = new Intl.RelativeTimeFormat(undefined, {numeric: 'auto', style: 'narrow'});
  function runTime(value) {
    const stamp = Date.parse(value);
    if (Number.isNaN(stamp)) return '';
    const elapsed = Math.max(0, (Date.now() - stamp) / 1000);
    if (elapsed < 10) return 'Just now';
    for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]]) {
      if (elapsed >= size) return relativeTime.format(-Math.floor(elapsed / size), unit);
    }
    return relativeTime.format(-Math.floor(elapsed), 'second');
  }
  function drawRuns(runs) {
    const bar = $('run-bar');
    const cards = new Map([...bar.querySelectorAll('.run-card')].map(card => [card.dataset.run, card]));
    const focused = document.activeElement;
    const next = runs.map(run => {
      let card = cards.get(run.id);
      if (!card) {
        card = element('div', '');
        card.className = 'run-card';
        card.dataset.run = run.id;
        const button = element('button', '');
        button.type = 'button';
        button.className = 'run-choice';
        button.dataset.run = run.id;
        const names = element('span', '');
        names.className = 'run-names';
        const project = element('span', '');
        project.className = 'run-project';
        names.append(element('strong', ''), project);
        const metadata = element('span', '');
        metadata.className = 'run-meta';
        metadata.append(element('span', ''), element('time', ''));
        button.append(names, metadata);
        const dismiss = element('button', '×');
        dismiss.type = 'button';
        dismiss.className = 'run-dismiss';
        card.append(button, dismiss);
      }
      const [button, dismiss] = card.children;
      const [names, metadata] = button.children;
      const [status, time] = metadata.children;
      status.className = 'run-status ' + statusClass(run.status === 'missing' ? 'failed' : run.status);
      status.textContent = runStatus(run.status);
      names.firstElementChild.textContent = run.name;
      const project = names.lastElementChild;
      project.replaceChildren();
      if (run.source_host) {
        const host = element('span', run.source_host);
        host.className = 'run-host';
        project.append(host, ' · ');
      }
      project.append(run.project_name);
      time.dateTime = run.updated_at || '';
      time.textContent = runTime(run.updated_at);
      button.setAttribute('aria-label', project.textContent + ' / ' + run.name + ', ' + statusLabel(run.status));
      dismiss.setAttribute('aria-label', 'Dismiss ' + run.name);
      dismiss.title = 'Dismiss run';
      return card;
    });
    if (!next.length) next.push(element('p', 'No workflow runs yet'));
    if (next.length !== bar.children.length || next.some((item, index) => item !== bar.children[index])) {
      bar.replaceChildren(...next);
      if (bar.contains(focused)) focused.focus({preventScroll: true});
    }
    highlightRun();
  }
  let runsTimer, runsRequest;
  async function refreshRuns() {
    clearTimeout(runsTimer);
    runsRequest?.abort();
    const request = new AbortController();
    runsRequest = request;
    try {
      const response = await fetch(options.runs, {cache: 'no-store', signal: request.signal});
      if (!response.ok) throw new Error('HTTP ' + response.status);
      const {runs} = await response.json();
      if (!request.signal.aborted) drawRuns(runs);
    } catch {
      // A transient registry or network error leaves the current choices visible.
    } finally {
      if (runsRequest === request) runsTimer = setTimeout(refreshRuns, Math.max(3000, options.pollMs));
    }
  }
  async function dismissRun(button) {
    const card = button.closest('.run-card');
    const id = card.dataset.run;
    const feedback = $('run-feedback');
    button.disabled = true;
    feedback.hidden = true;
    try {
      const url = new URL(options.runs, location.href);
      url.pathname += '/' + encodeURIComponent(id);
      const response = await fetch(url, {method: 'DELETE'});
      if (!response.ok && response.status !== 404) throw new Error('HTTP ' + response.status);
      const fallback = (card.nextElementSibling || card.previousElementSibling)?.querySelector('.run-choice');
      if (document.activeElement === button || document.activeElement === document.body) {
        fallback?.focus({preventScroll: true});
      }
      card.remove();
      if (activeRunId === id) {
        activeRunId = null;
        setRunLocation(null);
        refresh();
      }
      await refreshRuns();
    } catch {
      button.disabled = false;
      feedback.textContent = 'Could not dismiss the run. Try again.';
      feedback.hidden = false;
    }
  }
  $('run-bar')?.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    if (button.classList.contains('run-dismiss')) {
      event.stopPropagation();
      dismissRun(button);
      return;
    }
    if (button.dataset.run === activeRunId && displayedRunId === activeRunId) return;
    activeRunId = button.dataset.run;
    setRunLocation(activeRunId);
    highlightRun();
    refresh();
  });

  async function refresh() {
    clearTimeout(timer);
    inFlight?.abort();
    const request = new AbortController();
    inFlight = request;
    try {
      const url = new URL(options.live, location.href);
      if (options.runs && activeRunId) url.searchParams.set('run', activeRunId);
      const response = await fetch(url, {cache: 'no-store', signal: request.signal});
      if (!response.ok) throw new Error('HTTP ' + response.status);
      const next = await response.json();
      if (!request.signal.aborted) {
        const runId = response.headers.get('X-KGraph-Run-ID') || next.run_id;
        const switched = options.runs && runId !== displayedRunId;
        if (options.runs) {
          displayedRunId = activeRunId = runId;
          setRunLocation(activeRunId);
          highlightRun();
        }
        if (switched) {
          select(null);
          drillStack = [];
          preferredInputPath = null;
          attentionSignature = undefined;
          fittedNodes = false;
          cameraMoved = false;
        }
        update(next, switched);
        showLive(automatic ? 'active' : 'paused', automatic ? 'Live' : 'Paused');
      }
    } catch (error) {
      if (!request.signal.aborted) showLive('offline', 'Reconnecting');
    } finally {
      if (inFlight === request && automatic && !request.signal.aborted) {
        timer = setTimeout(refresh, options.pollMs);
      }
    }
  }
  $('auto-refresh').addEventListener('click', () => setPolling(!automatic));
  if (options.live) {
    $('live-controls').hidden = false;
    setPolling(true);
  }
  update(bootstrap, true);
  if (options.runs) {
    setRunLocation(activeRunId);
    refreshRuns();
  }
})();
"""


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
                     layout_mode: str = "grid", respond_url: Optional[str] = None) -> str:
    """Return an interactive HTML document with no external assets."""
    if poll_ms < 1:
        raise ValueError("poll_ms must be at least 1")
    if layout_mode not in tuple(LAYOUT_MODES):
        raise ValueError("layout_mode must be grid, iso, or strip")
    initial = _json_script(snapshot)
    options = _json_script({"live": live_url, "runs": runs_url, "run": active_run_id, "pollMs": poll_ms,
                            "layoutMode": layout_mode, "respond": respond_url})
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
</main>
<script type="application/json" id="town-snapshot">{initial}</script>
<script type="application/json" id="town-options">{options}</script>
<script>{_SCRIPT}</script>
</body>
</html>
"""
