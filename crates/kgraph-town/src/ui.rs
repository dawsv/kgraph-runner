//! Render Hero Town as a self-contained HTML page.

use crate::network::AccessUrl;
use serde::Serialize;
use serde_json::{ser::Formatter, Value};
use std::io::{self, Write};

pub const LAYOUT_MODES: [(&str, &str); 3] =
    [("grid", "2D Grid"), ("iso", "3D Iso"), ("strip", "1-Row")];
pub const PWA_META: &str = r##"  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <meta name="apple-mobile-web-app-capable" content="yes">
  <meta name="theme-color" content="#49b7fb">
  <link rel="manifest" href="/manifest.webmanifest">
  <link rel="icon" type="image/svg+xml" href="/icon.svg">"##;
pub const EMBLEM: &str = include_str!("../assets/emblem.svg");
pub const STYLES: &str = include_str!("../assets/town.css");
const SCENERY: &str = include_str!("../assets/scenery.svg");
const HEROES: &str = include_str!("../assets/heroes.svg");
pub const SCRIPT: &str = include_str!("../assets/town.js");
pub const APP_ICON: &str = concat!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 64 64">
  <rect x="1" y="1" width="62" height="62" rx="13" fill="#49b7fb" stroke="#172033" stroke-width="2"/>
  <g transform="translate(4 4)">"##,
    include_str!("../assets/emblem.svg"),
    "</g>\n</svg>"
);

#[derive(Debug, Clone, Serialize)]
pub struct RenderTownOptions<'a> {
    #[serde(rename = "live")]
    pub live_url: Option<&'a str>,
    #[serde(rename = "runs")]
    pub runs_url: Option<&'a str>,
    #[serde(rename = "run")]
    pub active_run_id: Option<&'a str>,
    #[serde(rename = "pollMs")]
    pub poll_ms: u64,
    #[serde(skip)]
    pub embed: bool,
    #[serde(skip)]
    pub access_urls: &'a [AccessUrl],
    #[serde(rename = "layoutMode")]
    pub layout_mode: &'a str,
    #[serde(rename = "respond")]
    pub respond_url: Option<&'a str>,
    #[serde(rename = "events")]
    pub events_url: Option<&'a str>,
    #[serde(rename = "logs")]
    pub logs_url: Option<&'a str>,
    #[serde(rename = "qa")]
    pub qa_url: Option<&'a str>,
    #[serde(rename = "readOnly")]
    pub read_only: bool,
}

impl Default for RenderTownOptions<'_> {
    fn default() -> Self {
        Self {
            live_url: None,
            runs_url: None,
            active_run_id: None,
            poll_ms: 1200,
            embed: false,
            access_urls: &[],
            layout_mode: "grid",
            respond_url: None,
            events_url: None,
            logs_url: None,
            qa_url: None,
            read_only: false,
        }
    }
}

// Python json.dumps uses spaces after separators, ASCII escapes, and signed,
// two-digit exponents. Preserve those spellings in the bootstrap scripts.
struct TownJson;

fn float_repr(number: &serde_json::Number) -> String {
    // Reformat shortest round-trip digits using Python repr's exponent thresholds.
    let raw = number.to_string();
    let sign = if raw.starts_with('-') { "-" } else { "" };
    let unsigned = raw.trim_start_matches('-');
    let (mantissa, exponent) = unsigned.split_once('e').unwrap_or((unsigned, "0"));
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.replace('.', "");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return format!("{sign}0.0");
    }
    let exp =
        exponent.parse::<i32>().unwrap() + point - (digits.len() - significant.len()) as i32 - 1;
    let digits = significant.trim_end_matches('0');
    if !(-4..16).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let dot = if rest.is_empty() { "" } else { "." };
        format!("{sign}{first}{dot}{rest}e{exp:+03}")
    } else if exp < 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize))
    } else {
        let point = exp as usize + 1;
        if digits.len() > point {
            format!("{sign}{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("{sign}{digits}{}.0", "0".repeat(point - digits.len()))
        }
    }
}

impl Formatter for TownJson {
    fn begin_array_value<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }
    fn begin_object_key<W: Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.begin_array_value(w, first)
    }
    fn begin_object_value<W: Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }
    fn write_f64<W: Write + ?Sized>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        w.write_all(
            float_repr(&serde_json::Number::from_f64(value).expect("finite JSON float")).as_bytes(),
        )
    }
    fn write_string_fragment<W: Write + ?Sized>(&mut self, w: &mut W, s: &str) -> io::Result<()> {
        for unit in s.encode_utf16() {
            if unit >= 127 {
                write!(w, "\\u{unit:04x}")?;
            } else {
                w.write_all(&[unit as u8])?;
            }
        }
        Ok(())
    }
}

pub(crate) fn python_json(data: &impl Serialize) -> Result<String, String> {
    let mut bytes = Vec::new();
    data.serialize(&mut serde_json::Serializer::with_formatter(
        &mut bytes, TownJson,
    ))
    .map_err(|err| err.to_string())?;
    Ok(String::from_utf8(bytes).expect("JSON is UTF-8"))
}

fn json_script(data: &impl Serialize) -> Result<String, String> {
    Ok(python_json(data)?
        .replace("</", r"<\/")
        .replace("<!--", r"\u003c!--"))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// Python repr escapes Unicode separators and non-printing categories, except space.
static NON_PRINTING: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"[\p{C}\p{Z}]").unwrap());

fn quoted_string(value: &str) -> String {
    use std::fmt::Write;
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = String::from(quote);
    for c in value.chars() {
        match c {
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            c if c == quote || c == '\\' => {
                repr.push('\\');
                repr.push(c);
            }
            c if c != ' ' && NON_PRINTING.is_match(c.encode_utf8(&mut [0; 4])) => {
                let n = c as u32;
                if n <= 255 {
                    write!(repr, "\\x{n:02x}").unwrap();
                } else if n <= 65535 {
                    write!(repr, "\\u{n:04x}").unwrap();
                } else {
                    write!(repr, "\\U{n:08x}").unwrap();
                }
            }
            c => repr.push(c),
        }
    }
    repr.push(quote);
    repr
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => quoted_string(s),
        Value::Number(n) if n.is_f64() => float_repr(n),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", quoted_string(k), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn device_menu(entries: &[AccessUrl]) -> String {
    let mut links = String::new();
    for entry in entries {
        let url = &entry.url;
        let parsed = url
            .trim_start_matches(|c: char| c <= ' ')
            .replace(['\r', '\n', '\t'], "");
        let scheme = parsed.split_once(':').map(|(s, _)| s).unwrap_or("");
        if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
            continue;
        }
        let label = escape(&entry.label);
        let href = escape(url);
        let address = escape(url.split('?').next().unwrap());
        links.push_str(&format!(r#"<li><a href="{href}" target="_blank" rel="noreferrer">{label}<span>{address}</span></a><button type="button" aria-label="Copy {label} address">Copy</button></li>"#));
    }
    if links.is_empty() {
        return links;
    }
    format!(
        r#"<details class="devices">
        <summary>Devices</summary>
        <div class="device-panel">
          <ul id="device-list">{links}</ul>
          <input class="copy-fallback" id="device-fallback" aria-label="Device address" readonly hidden>
          <p class="copy-note" id="device-copy-result" role="status"></p>
        </div>
      </details>"#
    )
}

pub fn render_town_html(snapshot: &Value, opts: &RenderTownOptions<'_>) -> Result<String, String> {
    if opts.poll_ms == 0 {
        return Err("poll_ms must be at least 1".into());
    }
    if !LAYOUT_MODES
        .iter()
        .any(|(mode, _)| *mode == opts.layout_mode)
    {
        return Err("layout_mode must be grid, iso, or strip".into());
    }
    let initial = json_script(snapshot)?;
    let options = json_script(opts)?;
    let layouts: String = LAYOUT_MODES
        .iter()
        .map(|(mode, label)| {
            let pressed = *mode == opts.layout_mode;
            format!(r#"<button data-layout="{mode}" aria-pressed="{pressed}">{label}</button>"#)
        })
        .collect();
    let title = match snapshot.get("title") {
        None => "Hero Town".to_owned(),
        Some(Value::String(s)) => escape(s),
        Some(v) => escape(&python_repr(v)),
    };
    let shell = if opts.embed { "embed" } else { "full" };
    let meta = if opts.embed {
        r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#
    } else {
        PWA_META
    };
    let switcher = if opts.runs_url.is_some_and(|url| !url.is_empty()) {
        concat!(
            r#"<nav class="run-bar" id="run-bar" aria-label="Workflow runs"></nav>"#,
            r#"<p class="run-feedback" id="run-feedback" role="status" hidden></p>"#
        )
    } else {
        ""
    };
    let devices = device_menu(opts.access_urls);
    Ok(format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  {meta}
  <title>{title} | Hero Town</title>
  <style>{STYLES}</style>
</head>
<body class="{shell}" data-theme="day">
<main class="town">
  <header class="hud">
    <div class="hud-top">
      <div class="identity">
        <svg class="emblem" viewBox="0 0 56 56" aria-hidden="true">
          {EMBLEM}
        </svg>
        <div><p class="wordmark">Hero Town</p><h1 id="workflow-title">{title}</h1></div>
      </div>
      <div class="hud-actions">
        <span id="overall" class="overall">Pending</span>
        {devices}
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
    {SCENERY}
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
      {HEROES}
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
<script>{SCRIPT}</script>
</body>
</html>
"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn preserves_python_float_spelling_and_container_titles() {
        let tie = 1e15 + 0.25;
        let data = json!([tie, -0.0, 1e-4, 1e-5, 1e16]);
        assert_eq!(
            json_script(&data).unwrap(),
            "[1000000000000000.2, -0.0, 0.0001, 1e-05, 1e+16]"
        );
        let title = json!([null, false, "it's", "\u{200b}", "\u{f0010}"]);
        assert_eq!(
            python_repr(&title),
            r#"[None, False, "it's", '\u200b', '\U000f0010']"#
        );
    }

    #[test]
    fn rejects_invalid_options() {
        for (opts, error) in [
            (
                RenderTownOptions {
                    poll_ms: 0,
                    ..Default::default()
                },
                "poll_ms must be at least 1",
            ),
            (
                RenderTownOptions {
                    layout_mode: "list",
                    ..Default::default()
                },
                "layout_mode must be grid, iso, or strip",
            ),
        ] {
            assert_eq!(render_town_html(&json!({}), &opts).unwrap_err(), error);
        }
    }

    fn script_data<'a>(html: &'a str, id: &str) -> &'a str {
        html.split_once(&format!(r#"<script type="application/json" id="{id}">"#))
            .unwrap()
            .1
            .split_once("</script>")
            .unwrap()
            .0
    }

    #[test]
    fn static_export_matches_python_and_embeds_all_assets() {
        let html = render_town_html(&json!({}), &Default::default()).unwrap();
        assert_eq!(
            script_data(&html, "town-options"),
            r#"{"live": null, "runs": null, "run": null, "pollMs": 1200, "layoutMode": "grid", "respond": null, "events": null, "logs": null, "qa": null, "readOnly": false}"#
        );
        assert!(html.contains(r#"<section id="inspect-log" class="inspect-log" hidden"#));
        assert!(html.contains(r#"<details class="qa-panel" id="qa-panel" hidden>"#));
        let mut remainder = &html[..];
        for marker in [
            "class=\"inspector-head\"",
            "id=\"open-subgraph\"",
            "id=\"inspect-subtasks\"",
            "id=\"inspect-fields\"",
            "id=\"inspect-outputs\"",
            "id=\"inspect-log\"",
            "</aside>",
        ] {
            remainder = remainder.split_once(marker).unwrap().1;
        }
        for asset in [STYLES, SCRIPT, SCENERY, HEROES, EMBLEM] {
            assert!(html.contains(asset));
        }
        assert!(!html.contains("/artifacts/"));
        // Hashes from the v0.2.0 Python renderer (including whitespace).
        assert_eq!(
            hex::encode(Sha256::digest(html)),
            "95e6c6d4c9563e588666fdfe619165c730f5b4b4cab7456f399b22f769308ea9"
        );
        assert_eq!(
            hex::encode(Sha256::digest(APP_ICON)),
            "aa510787199773fff60a423bdef8f4eef54b8f10307f39b79f3c5790e6998a6d"
        );
    }

    #[test]
    fn live_embed_matches_python_and_escapes_untrusted_content() {
        let snapshot = json!({"edges":[], "nodes":[1, 1e-7, "é😀\u{7f}"], "title": "<Gate & \"log\" 'stage'> </script><!--"});
        let access_urls = [
            AccessUrl {
                kind: "lan".into(),
                url: "HTTPS://example.test/?token=t&x=1".into(),
                label: "Phone & \"tablet\"".into(),
            },
            AccessUrl {
                kind: "unused".into(),
                url: "javascript:alert(1)".into(),
                label: "Skip".into(),
            },
        ];
        let options = RenderTownOptions {
            live_url: Some("/api/snapshot"),
            runs_url: Some("/api/runs"),
            active_run_id: Some("a"),
            poll_ms: 40,
            embed: true,
            access_urls: &access_urls,
            layout_mode: "iso",
            respond_url: Some("/api/respond"),
            events_url: Some("/api/events"),
            logs_url: Some("/api/logs"),
            qa_url: Some("/api/qa"),
            read_only: true,
        };
        let html = render_town_html(&snapshot, &options).unwrap();
        assert!(html.contains(
            "&lt;Gate &amp; &quot;log&quot; &#x27;stage&#x27;&gt; &lt;/script&gt;&lt;!--"
        ));
        assert!(!html.contains("javascript:alert"));
        let data = script_data(&html, "town-snapshot");
        assert!(data.contains(r"<\/script>\u003c!--"));
        assert_eq!(serde_json::from_str::<Value>(data).unwrap(), snapshot);
        assert_eq!(
            serde_json::from_str::<Value>(script_data(&html, "town-options")).unwrap(),
            json!({
                "live":"/api/snapshot","runs":"/api/runs","run":"a","pollMs":40,"layoutMode":"iso",
                "respond":"/api/respond","events":"/api/events","logs":"/api/logs","qa":"/api/qa","readOnly":true
            })
        );
        assert_eq!(
            hex::encode(Sha256::digest(html)),
            "f12105defd56e68fab291cb88592b69e151269f56955cf1f80f8b032ac766113"
        );
    }
}
