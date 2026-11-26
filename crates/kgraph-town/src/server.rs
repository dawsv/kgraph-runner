//! Hero Town's single-workflow and registry-backed HTTP servers.
use crate::interactions;
use crate::network::{
    discover_access_urls, generate_token, with_token, AccessUrl, CloudflareTunnel,
};
pub use crate::snapshot::SnapshotProvider;
use crate::snapshot::{build_town_snapshot_from_graph, disable_responses};
use crate::ui::{self, RenderTownOptions};
use crate::{registry, workboard};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{sse::Event, IntoResponse, Response, Sse};
use axum::Router;
use futures_util::{stream, StreamExt};
use kgraph_core::telemetry;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{spawn_blocking, JoinHandle};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use url::form_urlencoded;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub poll_ms: u64,
    pub token: Option<String>,
    pub layout_mode: String,
    pub read_only: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8765,
            poll_ms: 1200,
            token: None,
            layout_mode: "grid".into(),
            read_only: false,
        }
    }
}

pub struct RunningTownServer {
    pub port: u16,
    pub access_urls: Vec<AccessUrl>,
    pub shutdown: CancellationToken,
    task: JoinHandle<Result<(), String>>,
}

impl RunningTownServer {
    pub async fn wait(self) -> Result<(), String> {
        self.task.await.map_err(|e| e.to_string())?
    }
    pub fn stop(&self) {
        self.shutdown.cancel();
    }
}

enum Source {
    Town(SnapshotProvider),
    Hub(Vec<PathBuf>),
}

struct Town {
    source: Source,
    config: ServerConfig,
    access: Vec<AccessUrl>,
    shutdown: CancellationToken,
    heartbeat: Duration,
    event_feeds: Mutex<HashMap<String, watch::Sender<Option<Event>>>>,
}

impl Town {
    fn hub(&self) -> bool {
        matches!(self.source, Source::Hub(_))
    }
    fn enabled(&self) -> bool {
        self.config.token.is_some() && !self.config.read_only
    }
    fn runs(&self) -> Result<Vec<Value>, String> {
        match &self.source {
            Source::Town(provider) => Ok(vec![registry::single_run_entry(&provider()?)]),
            Source::Hub(dirs) => Ok(registry::list_runs(dirs)),
        }
    }
    fn snapshot(&self, selected: &str) -> Result<(Value, Option<Value>), String> {
        let (snapshot, active) = match &self.source {
            Source::Town(provider) => (provider()?, None),
            Source::Hub(dirs) => {
                let mut runs = registry::list_runs(dirs);
                let index = runs.iter().position(|r| r["id"] == selected).unwrap_or(0);
                let active = (!runs.is_empty()).then(|| runs.swap_remove(index));
                (hub_snapshot(active.as_ref())?, active)
            }
        };
        if !snapshot.is_object() {
            return Err("Snapshot must be a JSON object".into());
        }
        Ok((
            if self.enabled() {
                snapshot
            } else {
                disable_responses(&snapshot)
            },
            active,
        ))
    }
    fn run(&self, id: Option<&str>, require_state: bool) -> Result<Option<Value>, String> {
        match &self.source {
            Source::Hub(dirs) => Ok(id.and_then(|id| registry::get_run(id, dirs))),
            Source::Town(provider) => {
                let snapshot = provider()?;
                Ok(Some(
                    if require_state && snapshot["state_file"].as_str().unwrap_or("").is_empty() {
                        json!({})
                    } else {
                        registry::single_run_entry(&snapshot)
                    },
                ))
            }
        }
    }
}

fn hub_snapshot(active: Option<&Value>) -> Result<Value, String> {
    let Some(run) = active else {
        return Ok(
            json!({"version":1,"title":"Workflow Hub","status":"pending","nodes":[],"edges":[],"run_id":null}),
        );
    };
    let path = |key: &str| run[key].as_str().map(Path::new);
    if run["source"] == "remote" {
        let remote = path("remote_snapshot_path").ok_or("Missing remote snapshot path")?;
        let snapshot = Value::Object(kgraph_core::files::read_object(remote)?);
        return Ok(interactions::apply_remote_response(
            &snapshot,
            run["id"].as_str().ok_or("Missing run ID")?,
        ));
    }
    let mut snapshot = build_town_snapshot_from_graph(
        None,
        path("state_path"),
        path("report_path"),
        None,
        run["name"].as_str(),
        run["status"].as_str(),
        path("project_dir"),
    );
    for (key, from) in [
        ("workflow_ref", "workflow_path"),
        ("run_id", "id"),
        ("project_dir", "project_dir"),
        ("factory", "factory"),
    ] {
        snapshot[key] = run[from].clone();
    }
    Ok(snapshot)
}

const JSON: &str = "application/json; charset=utf-8";
const HTML: &str = "text/html; charset=utf-8";
const MANIFEST: &str = r##"{"id": "/", "name": "Hero Town — KGraph", "short_name": "Hero Town", "start_url": "/", "display": "standalone", "theme_color": "#49b7fb", "background_color": "#fff8e8", "icons": [{"src": "/icon.svg", "type": "image/svg+xml", "sizes": "any", "purpose": "any"}]}"##;
const KEEPALIVE: Duration = Duration::from_secs(15);
const READ_ONLY: &str = "This shared Hero Town view is read-only";
const CROSS_ORIGIN: &str = "Cross-origin submissions are forbidden";

fn reply(status: StatusCode, content_type: &'static str, body: impl Into<Body>) -> Response {
    (status, [(header::CONTENT_TYPE, content_type)], body.into()).into_response()
}
fn json_reply(status: StatusCode, value: Value) -> Response {
    reply(status, JSON, value.to_string())
}
fn error(status: StatusCode, message: &str) -> Response {
    json_reply(status, json!({"error":message}))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
fn http_error(status: StatusCode, message: &str) -> Response {
    reply(status, HTML, format!("<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        <p>Error code: {}</p>\n        <p>Message: {}.</p>\n    </body>\n</html>\n", status.as_u16(), escape(message)))
}
fn unauthorized(path: &str, run: &str) -> Response {
    if path.starts_with("/api/") {
        error(StatusCode::UNAUTHORIZED, "unauthorized")
    } else {
        reply(StatusCode::UNAUTHORIZED, HTML, unlock_page(run))
    }
}
fn unlock_page(run: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
{meta}
<title>Unlock Hero Town</title>
<style>
* {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 64px 16px 24px; font: 16px/1.5 system-ui, sans-serif;
  color: #172033; background: #d8f7ff; }}
main {{ max-width: 390px; margin: auto; border: 3px solid; border-radius: 17px;
  background: #fff8e8; padding: 24px; box-shadow: 5px 5px #172033; }}
img {{ display: block; width: 64px; }}
h1 {{ margin: 8px 0; line-height: 1.1; }}
label {{ display: block; margin: 16px 0 4px; font-weight: 700; }}
input,button {{ width: 100%; min-height: 44px; padding: 8px 12px; font: inherit;
  color: inherit; border: 2px solid; border-radius: 8px; }}
button {{ margin-top: 12px; background: #ffcf54; font-weight: 800; cursor: pointer;
  box-shadow: 2px 2px #172033; }}
</style>
</head>
<body>
<main>
  <img src="/icon.svg" width="64" height="64" alt="">
  <h1>Unlock Hero Town</h1>
  <form method="post" action="/auth">
    <input type="hidden" name="run" value="{run}">
    <label for="token">Access token</label>
    <input type="password" id="token" name="token" autocomplete="current-password" required>
    <button type="submit">Unlock</button>
  </form>
</main>
</body>
</html>"##,
        meta = ui::PWA_META,
        run = escape(run)
    )
}
type Fields = Vec<(String, String)>;

fn fields(data: &[u8], keep_blank: bool) -> Fields {
    form_urlencoded::parse(data)
        .into_owned()
        .filter(|(_, v)| keep_blank || !v.is_empty())
        .collect()
}
fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a str> {
    fields
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}
fn matches_token(expected: Option<&str>, value: Option<&str>) -> bool {
    expected
        .zip(value)
        .is_some_and(|(a, b)| bool::from(a.as_bytes().ct_eq(b.as_bytes())))
}
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
fn authenticated(expected: Option<&str>, query: Option<&str>, headers: &HeaderMap) -> bool {
    if expected.is_none() || matches_token(expected, query) {
        return true;
    }
    if let Some((scheme, value)) =
        header_str(headers, "authorization").and_then(|s| s.split_once(' '))
    {
        if scheme.eq_ignore_ascii_case("bearer") && matches_token(expected, Some(value)) {
            return true;
        }
    }
    let cookie = header_str(headers, "cookie")
        .unwrap_or("")
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .rfind(|(k, _)| *k == "kgraph_token")
        .and_then(|(_, v)| {
            urlencoding::decode(
                v.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v),
            )
            .ok()
        });
    matches_token(expected, cookie.as_deref())
}
fn redirect(expected: Option<&str>, pairs: &Fields, headers: &HeaderMap) -> Response {
    let query = pairs
        .iter()
        .map(|(k, v)| {
            let quote = |s: &str| urlencoding::encode(s).replace("%20", "+");
            format!("{}={}", quote(k), quote(v))
        })
        .collect::<Vec<_>>()
        .join("&");
    let location = if query.is_empty() {
        "/".into()
    } else {
        format!("/?{query}")
    };
    let mut response = (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
    if let Some(token) = expected {
        let secure = header_str(headers, "x-forwarded-proto")
            .is_some_and(|v| v.eq_ignore_ascii_case("https"));
        let cookie = format!(
            "kgraph_token={}; HttpOnly; SameSite=Lax; Path=/{}",
            urlencoding::encode(token),
            if secure { "; Secure" } else { "" }
        );
        response.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).expect("encoded cookie"),
        );
    }
    response
}
fn same_origin(headers: &HeaderMap) -> bool {
    if ["sec-fetch-site", "origin", "host"]
        .iter()
        .any(|key| headers.get_all(*key).iter().count() > 1)
    {
        return false;
    }
    if headers.contains_key("sec-fetch-site")
        && !matches!(
            header_str(headers, "sec-fetch-site"),
            Some("same-origin" | "none")
        )
    {
        return false;
    }
    if !headers.contains_key("origin") {
        return true;
    }
    let scheme = if headers.contains_key("x-forwarded-proto") {
        header_str(headers, "x-forwarded-proto").unwrap_or("")
    } else {
        "http"
    };
    let host = header_str(headers, "host").unwrap_or("");
    !host.is_empty()
        && (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
        && header_str(headers, "origin")
            .is_some_and(|s| s.eq_ignore_ascii_case(&format!("{scheme}://{host}")))
}

type BodyError = (StatusCode, &'static str);
async fn bounded_body(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
    excess: &'static str,
) -> Result<Bytes, BodyError> {
    let length = header_str(headers, "content-length").unwrap_or("").trim();
    if headers.contains_key(header::TRANSFER_ENCODING)
        || headers.get_all(header::CONTENT_LENGTH).iter().count() != 1
        || length.is_empty()
        || !length.bytes().all(|b| b.is_ascii_digit())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "A valid Content-Length is required",
        ));
    }
    let size = length.trim_start_matches('0');
    let size = if size.is_empty() {
        0
    } else {
        size.parse::<usize>().unwrap_or(usize::MAX)
    };
    if size > limit {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, excess));
    }
    let bytes = timeout(Duration::from_secs(5), to_bytes(body, size))
        .await
        .map_err(|_| (StatusCode::BAD_REQUEST, "Could not read request body"))?
        .map_err(|_| (StatusCode::BAD_REQUEST, "Incomplete request body"))?;
    if bytes.len() != size {
        return Err((StatusCode::BAD_REQUEST, "Incomplete request body"));
    }
    Ok(bytes)
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Endpoint {
    Home,
    Snapshot,
    Runs,
    Events,
    Response,
    Logs,
    Telemetry,
    Workboard,
    Qa,
    Respond,
    Ingest,
    Delete,
    Missing,
}

fn endpoint(hub: bool, method: &Method, path: &str) -> (Endpoint, Option<String>) {
    use Endpoint::*;
    match (method, path) {
        (&Method::GET, "/") => (Home, None),
        (&Method::GET, "/api/snapshot") => (Snapshot, None),
        (&Method::GET, "/api/runs") => (Runs, None),
        (&Method::GET, "/api/events") => (Events, None),
        (&Method::GET, "/api/telemetry") if !hub => (Telemetry, None),
        (&Method::GET, "/api/workboard") if !hub => (Workboard, None),
        (&Method::GET, "/api/logs") if !hub => (Logs, None),
        (&Method::GET | &Method::POST, "/api/qa") if !hub => (Qa, None),
        (&Method::POST, "/api/respond") if !hub => (Respond, None),
        (&Method::POST, "/api/ingest") if hub => (Ingest, None),
        _ if hub && path.starts_with("/api/runs/") => {
            let rest = &path["/api/runs/".len()..];
            let (id, op) = if *method == Method::DELETE {
                (rest, Delete)
            } else {
                let Some((id, suffix)) = rest.rsplit_once('/') else {
                    return (Missing, None);
                };
                let op = match (method, suffix) {
                    (&Method::POST, "respond") => Respond,
                    (&Method::POST, "delete") => Delete,
                    (&Method::GET, "response") => Response,
                    (&Method::GET, "logs") => Logs,
                    (&Method::GET, "telemetry") => Telemetry,
                    (&Method::GET, "workboard") => Workboard,
                    (&Method::GET | &Method::POST, "qa") => Qa,
                    _ => Missing,
                };
                (id, op)
            };
            (
                op,
                Some(
                    String::from_utf8_lossy(&urlencoding::decode_binary(id.as_bytes()))
                        .into_owned(),
                ),
            )
        }
        _ => (Missing, None),
    }
}

async fn handle(State(town): State<Arc<Town>>, req: Request) -> Response {
    let mut response = dispatch(town, req).await;
    let headers = response.headers_mut();
    headers
        .entry(header::CONNECTION)
        .or_insert(HeaderValue::from_static("close"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}
async fn dispatch(town: Arc<Town>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    let headers = &parts.headers;
    let method = &parts.method;
    let expected = town.config.token.as_deref();
    if *method == Method::GET {
        match path {
            "/api/health" => {
                return json_reply(
                    StatusCode::OK,
                    json!({"ok":true,"mode":if town.hub() {"hub"} else {"single"}}),
                )
            }
            "/icon.svg" => {
                return reply(StatusCode::OK, "image/svg+xml; charset=utf-8", ui::APP_ICON)
            }
            "/manifest.webmanifest" => {
                return reply(
                    StatusCode::OK,
                    "application/manifest+json; charset=utf-8",
                    MANIFEST,
                )
            }
            _ => {}
        }
    }
    if *method == Method::POST && path == "/auth" {
        if !same_origin(headers) {
            return error(StatusCode::FORBIDDEN, CROSS_ORIGIN);
        }
        let data = match bounded_body(headers, body, 4096, "Auth payload exceeds 4 KiB").await {
            Ok(v) => v,
            Err((status, msg)) => return http_error(status, msg),
        };
        if std::str::from_utf8(&data).is_err() {
            return http_error(StatusCode::BAD_REQUEST, "Bad Request");
        }
        let form = fields(&data, false);
        let run = field(&form, "run").unwrap_or("");
        return if expected.is_none() || matches_token(expected, field(&form, "token")) {
            let params = if run.is_empty() {
                vec![]
            } else {
                vec![("run".into(), run.into())]
            };
            redirect(expected, &params, headers)
        } else {
            unauthorized("/", run)
        };
    }
    let query = fields(
        parts.uri.query().unwrap_or("").as_bytes(),
        *method == Method::GET,
    );
    let token = field(&query, "token");
    if path == "/" && *method == Method::GET && matches_token(expected, token) {
        return redirect(
            expected,
            &query.into_iter().filter(|(k, _)| k != "token").collect(),
            headers,
        );
    }
    let (op, id) = endpoint(town.hub(), method, path);
    let mutation = *method != Method::GET
        && matches!(
            op,
            Endpoint::Qa | Endpoint::Respond | Endpoint::Ingest | Endpoint::Delete
        );
    if mutation && op != Endpoint::Ingest && town.config.read_only {
        return error(StatusCode::FORBIDDEN, READ_ONLY);
    }
    if !authenticated(expected, token, headers)
        || (expected.is_none()
            && ((mutation && op != Endpoint::Delete) || op == Endpoint::Response))
    {
        return unauthorized(path, field(&query, "run").unwrap_or(""));
    }
    if mutation && !same_origin(headers) {
        return error(StatusCode::FORBIDDEN, CROSS_ORIGIN);
    }
    if op == Endpoint::Missing {
        return http_error(StatusCode::NOT_FOUND, "Not Found");
    }
    if op == Endpoint::Events {
        return events(town, field(&query, "run").unwrap_or("").into()).await;
    }
    let payload = if mutation && op != Endpoint::Delete {
        let (limit, msg) = match op {
            Endpoint::Ingest => (2 * 1024 * 1024, "Snapshot payload exceeds 2 MiB"),
            Endpoint::Qa => (256 * 1024, "QA payload exceeds 256 KiB"),
            _ => (256 * 1024, "Response payload exceeds 256 KiB"),
        };
        let bytes = match bounded_body(headers, body, limit, msg).await {
            Ok(v) => v,
            Err((s, msg)) => return error(s, msg),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) if v.is_object() => Some(v),
            _ => return error(StatusCode::BAD_REQUEST, "Invalid JSON request body"),
        }
    } else {
        None
    };
    spawn_blocking(move || execute(&town, op, id.as_deref(), query, payload))
        .await
        .map_err(|e| e.to_string())
        .and_then(|result| result)
        .unwrap_or_else(|reason| {
            tracing::error!(%reason, "Hero Town request failed");
            failure(op, mutation)
        })
}

fn failure(op: Endpoint, mutation: bool) -> Response {
    let status = StatusCode::INTERNAL_SERVER_ERROR;
    match op {
        Endpoint::Respond => error(status, "Could not deliver gate response"),
        Endpoint::Qa if mutation => error(status, "Could not record manual QA"),
        Endpoint::Telemetry => error(status, "Could not read run telemetry"),
        Endpoint::Workboard => error(status, "Could not read workboard"),
        Endpoint::Delete => error(status, "Could not dismiss run"),
        Endpoint::Ingest => error(status, "Could not store remote snapshot"),
        _ => http_error(status, "Could not build town snapshot"),
    }
}
fn execute(
    town: &Town,
    op: Endpoint,
    id: Option<&str>,
    query: Fields,
    payload: Option<Value>,
) -> Result<Response, String> {
    let selected = field(&query, "run").unwrap_or("");
    let not_found = || error(StatusCode::NOT_FOUND, "Run not found");
    Ok(match op {
        Endpoint::Home | Endpoint::Snapshot => {
            let (snap, active) = town.snapshot(selected)?;
            let active_id = active.as_ref().and_then(|r| r["id"].as_str());
            if op == Endpoint::Snapshot {
                let mut r = json_reply(StatusCode::OK, snap);
                if active.as_ref().is_some_and(|r| r["source"] == "remote") {
                    r.headers_mut().insert(
                        "x-kgraph-run-id",
                        HeaderValue::from_str(active_id.ok_or("Missing run ID")?)
                            .map_err(|e| e.to_string())?,
                    );
                }
                r
            } else {
                let hub = town.hub();
                let opts = RenderTownOptions {
                    live_url: Some("/api/snapshot"),
                    runs_url: hub.then_some("/api/runs"),
                    active_run_id: active_id.or_else(|| snap["run_id"].as_str()),
                    poll_ms: town.config.poll_ms,
                    embed: false,
                    access_urls: &town.access,
                    layout_mode: &town.config.layout_mode,
                    respond_url: town.enabled().then_some(if hub {
                        "/api/runs"
                    } else {
                        "/api/respond"
                    }),
                    events_url: Some("/api/events"),
                    logs_url: Some(if hub { "/api/runs" } else { "/api/logs" }),
                    qa_url: Some(if hub { "/api/runs" } else { "/api/qa" }),
                    read_only: town.config.read_only,
                };
                reply(StatusCode::OK, HTML, ui::render_town_html(&snap, &opts)?)
            }
        }
        Endpoint::Runs => json_reply(StatusCode::OK, json!({"runs":town.runs()?})),
        Endpoint::Response => match id.and_then(interactions::read_remote_response) {
            Some(v) => json_reply(StatusCode::OK, v),
            None => error(StatusCode::NOT_FOUND, "No queued response for this run"),
        },
        Endpoint::Telemetry | Endpoint::Workboard => {
            let Some(run) = town.run(id, true)? else {
                return Ok(not_found());
            };
            local_work_data(op, &run, &query)?
        }
        Endpoint::Logs | Endpoint::Qa | Endpoint::Respond => {
            let Some(run) = town.run(id, op != Endpoint::Respond)? else {
                return Ok(not_found());
            };
            let run = run.as_object().ok_or("Invalid registry entry")?;
            let (status, data) = match (&op, payload.as_ref()) {
                (Endpoint::Respond, Some(v)) => interactions::submit_gate_response(run, v),
                (Endpoint::Qa, Some(v)) => interactions::submit_run_qa(run, v),
                (Endpoint::Qa, None) => interactions::read_run_qa(run, town.enabled()),
                _ => interactions::read_node_log_tail(
                    run,
                    field(&query, "path"),
                    field(&query, "max_bytes"),
                ),
            };
            json_reply(
                StatusCode::from_u16(status).map_err(|e| e.to_string())?,
                data,
            )
        }
        Endpoint::Delete => {
            let Source::Hub(dirs) = &town.source else {
                return Ok(not_found());
            };
            let id = id.ok_or("Missing run ID")?;
            if registry::remove_run(id, dirs)? {
                json_reply(StatusCode::OK, json!({"ok":true,"removed":id}))
            } else {
                not_found()
            }
        }
        Endpoint::Ingest => {
            let v = payload.ok_or("Missing ingest payload")?;
            match registry::ingest_remote_run(&v["run"], &v["snapshot"]) {
                Ok(run) => json_reply(StatusCode::OK, json!({"ok":true,"run":run})),
                Err(msg)
                    if matches!(
                        msg.as_str(),
                        "run and snapshot must be JSON objects"
                            | "run ID must be a safe slug of at most 128 characters"
                            | "run ID conflicts with an existing run"
                    ) || msg.starts_with("run.") =>
                {
                    error(StatusCode::BAD_REQUEST, &msg)
                }
                Err(msg) => return Err(msg),
            }
        }
        Endpoint::Events | Endpoint::Missing => unreachable!(),
    })
}

fn local_work_data(op: Endpoint, run: &Value, query: &Fields) -> Result<Response, String> {
    let path = |key: &str| run[key].as_str().filter(|s| !s.is_empty()).map(Path::new);
    let denied = || {
        error(
            StatusCode::FORBIDDEN,
            "This operation requires a local run inside .kgraph",
        )
    };
    let Some(state) = path("state_path") else {
        return Ok(denied());
    };
    // A remote entry can retain local metadata when ingestion deduplicates
    // against a run registered on this host. A foreign state path is untrusted.
    if run["source"] == "remote"
        && (path("report_path").is_none()
            || !state.is_file()
            || run["source_host"]
                .as_str()
                .is_some_and(|h| h.to_lowercase() != registry::hostname()))
    {
        return Ok(denied());
    }
    let Some(root) = state
        .ancestors()
        .skip(1)
        .find(|p| p.file_name().is_some_and(|n| n == ".kgraph"))
    else {
        return Ok(denied());
    };
    if interactions::reject_path_traversal(state).is_err() || root.is_symlink() {
        return Ok(denied());
    }
    let root = interactions::resolve(root).map_err(|e| e.to_string())?;
    let Ok(state) = interactions::ensure_inside(state, &root) else {
        return Ok(denied());
    };
    if op == Endpoint::Workboard {
        let items = workboard::list_workboard(&state, path("report_path"), path("project_dir"))?;
        return Ok(json_reply(StatusCode::OK, json!({"items":items})));
    }
    let last = match field(query, "last") {
        None => None,
        Some(s) => match s.parse::<usize>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                return Ok(error(
                    StatusCode::BAD_REQUEST,
                    "last must be a positive integer",
                ))
            }
        },
    };
    let history = state.with_file_name("telemetry.jsonl");
    if interactions::ensure_inside(&history, &root).as_ref() != Ok(&history) {
        return Ok(denied());
    }
    // Anchor the actual read as well as the preflight containment check.
    let file = match interactions::regular(&history, true) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json_reply(StatusCode::OK, json!({"available":false})));
        }
        Err(_) => return Ok(denied()),
    };
    let lock = history.with_extension("jsonl.lock");
    let runs = {
        let _guard = crate::artifact::lock(&root, &lock).map_err(|e| e.to_string())?;
        use std::io::{BufRead, BufReader};
        let mut runs = std::collections::VecDeque::new();
        let workflow = field(query, "workflow");
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            let run: telemetry::RunTelemetry =
                serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if workflow.is_some() && workflow != run.workflow_ref.as_deref() {
                continue;
            }
            runs.push_back(run);
            if last.is_some_and(|limit| runs.len() > limit) {
                runs.pop_front();
            }
        }
        Vec::from(runs)
    };
    let Some(latest) = runs.last() else {
        return Ok(json_reply(StatusCode::OK, json!({"available":false})));
    };
    let result = if field(query, "chrome_trace") == Some("1") {
        telemetry::export_chrome_trace(latest)
    } else {
        json!({"available":true, "runs_analyzed":runs.len(), "latest_run":latest,
               "analysis":telemetry::analyze_telemetry(&runs)})
    };
    Ok(json_reply(StatusCode::OK, result))
}

fn subscribe(town: &Arc<Town>, selected: String) -> watch::Receiver<Option<Event>> {
    let key = if town.hub() { selected } else { String::new() };
    let mut feeds = town.event_feeds.lock().unwrap();
    if let Some(sender) = feeds.get(&key) {
        return sender.subscribe();
    }
    let (sender, receiver) = watch::channel(None);
    feeds.insert(key.clone(), sender.clone());
    tokio::spawn(poll_events(town.clone(), key, sender));
    receiver
}

async fn poll_events(town: Arc<Town>, key: String, sender: watch::Sender<Option<Event>>) {
    let mut previous = None;
    let interval = Duration::from_millis(town.config.poll_ms);
    loop {
        // Serialize retirement with subscription so a new viewer cannot attach
        // to a feed that has already decided to go idle.
        {
            let mut feeds = town.event_feeds.lock().unwrap();
            if sender.receiver_count() == 0 {
                feeds.remove(&key);
                return;
            }
        }
        let source = town.clone();
        let selected = key.clone();
        let result = tokio::select! {
            biased;
            _ = town.shutdown.cancelled() => break,
            result = spawn_blocking(move || -> Result<_, String> {
                let (mut snap, active) = source.snapshot(&selected)?;
                if source.hub() {
                    snap["run_id"] = active.map(|r| r["id"].clone()).unwrap_or(Value::Null);
                }
                let mut comparable = snap.clone();
                if let Some(map) = comparable.as_object_mut() {
                    map.shift_remove("generated_at");
                }
                comparable.sort_all_objects();
                let event = (previous.as_ref() != Some(&comparable))
                    .then(|| Event::default().event("snapshot").data(snap.to_string()));
                Ok((comparable, event))
            }) => result.map_err(|e| e.to_string()).and_then(|r| r),
        };
        match result {
            Ok((current, event)) => {
                previous = Some(current);
                if let Some(event) = event {
                    sender.send_replace(Some(event));
                }
            }
            Err(e) => {
                tracing::error!(%e, "Could not build town event snapshot");
                break;
            }
        }
        tokio::select! {
            _ = town.shutdown.cancelled() => break,
            _ = sender.closed() => {},
            _ = sleep(interval) => {},
        }
    }
    town.event_feeds.lock().unwrap().remove(&key);
}

async fn events(town: Arc<Town>, selected: String) -> Response {
    let mut receiver = subscribe(&town, selected);
    let stop = town.shutdown.clone();
    let first = tokio::select! {
        biased;
        _ = stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        event = receiver.wait_for(Option::is_some) => match event {
            Ok(snapshot) => snapshot.as_ref().unwrap().clone(),
            Err(_) => return failure(Endpoint::Events, false),
        },
    };
    let stream_stop = stop.child_token();
    let end_on_error = stream_stop.clone();
    let updates = stream::unfold(receiver, move |mut receiver| {
        let end_on_error = end_on_error.clone();
        async move {
            if receiver.changed().await.is_err() {
                end_on_error.cancel();
                return None;
            }
            let event = receiver.borrow_and_update().clone()?;
            Some((event, receiver))
        }
    });
    let heartbeat = town.heartbeat;
    let heartbeats = stream::unfold((), move |_| async move {
        sleep(heartbeat).await;
        Some((Event::default().comment("keepalive"), ()))
    });
    let stream = tokio_stream::once(first)
        .chain(stream::select(updates, heartbeats))
        .map(Ok::<_, Infallible>)
        .take_until(stream_stop.cancelled_owned());
    let mut response = Sse::new(stream).into_response();
    // Python streams are close-delimited. HTTP/1.0 preserves the advertised
    // keep-alive header while ending the connection when the body finishes.
    *response.version_mut() = axum::http::Version::HTTP_10;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

// Hyper coalesces identical Content-Length fields. Inspect the original HTTP/1
// head before replaying it to Hyper, which still owns all body parsing and
// response encoding. One request per connection matches the Python server.
async fn read_head(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    while bytes.len() < LIMIT {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let start = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&buf[..n]);
        if bytes[start..].windows(4).any(|w| w == b"\r\n\r\n")
            || bytes[start..].windows(2).any(|w| w == b"\n\n")
        {
            return Ok(bytes);
        }
    }
    Err(std::io::ErrorKind::InvalidData.into())
}

async fn head_error(bytes: &[u8], app: &Router) -> Option<Response> {
    use hyper::service::Service;

    let bad = || http_error(StatusCode::BAD_REQUEST, "Bad Request");
    let mut storage = [httparse::EMPTY_HEADER; 100];
    let mut req = httparse::Request::new(&mut storage);
    if !matches!(req.parse(bytes), Ok(httparse::Status::Complete(_))) {
        return Some(bad());
    }
    let mut lengths = req
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
        .map(|h| std::str::from_utf8(h.value).unwrap_or("").trim());
    let first = lengths.next();
    let invalid = lengths.next().is_some()
        || first.is_some_and(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
        || req
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"));
    let too_large = first.is_some_and(|s| s.parse::<u64>().unwrap_or(u64::MAX) > 2 * 1024 * 1024);
    if !invalid && !too_large {
        return None;
    }

    // Dispatch body-bearing routes with the original headers intact. Their
    // authorization and read-only checks take precedence over framing errors.
    let target = req.path.unwrap_or("");
    let path = target.split('?').next().unwrap_or("");
    if req.method != Some("POST")
        || !(matches!(path, "/auth" | "/api/ingest" | "/api/qa" | "/api/respond")
            || (path.starts_with("/api/runs/")
                && (path.ends_with("/qa") || path.ends_with("/respond"))))
    {
        return Some(bad());
    }
    let mut builder = Request::builder().method(Method::POST).uri(target);
    for h in req.headers.iter() {
        builder = builder.header(h.name, h.value);
    }
    let Ok(request) = builder.body(Body::empty()) else {
        return Some(bad());
    };
    let service = hyper_util::service::TowerToHyperService::new(app.clone());
    let Ok(response) = service.call(request).await;
    Some(response)
}
async fn send_head_error(socket: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let status = response.status();
    let content_type = response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap_or(HTML);
    let head=format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n");
    let body = to_bytes(response.into_body(), 8192)
        .await
        .map_err(std::io::Error::other)?;
    socket
        .write_all(format!("{head}Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    socket.write_all(&body).await?;
    socket.shutdown().await
}
async fn connection(mut socket: TcpStream, app: Router, shutdown: CancellationToken) {
    let head = tokio::select! {
        biased;
        _=shutdown.cancelled()=>return,
        result=timeout(Duration::from_secs(5),read_head(&mut socket))=>match result{
            Ok(Ok(head))=>head,
            Ok(Err(e)) if e.kind()==std::io::ErrorKind::InvalidData=>{
                let _=send_head_error(&mut socket,http_error(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,"Request Header Fields Too Large")).await;
                return;
            }
            _=>return
        }
    };
    if let Some(response) = head_error(&head, &app).await {
        let _ = send_head_error(&mut socket, response).await;
        return;
    }
    let (reader, writer) = socket.into_split();
    let io = tokio::io::join(std::io::Cursor::new(head).chain(reader), writer);
    let conn = hyper::server::conn::http1::Builder::new().serve_connection(
        hyper_util::rt::TokioIo::new(io),
        hyper_util::service::TowerToHyperService::new(app),
    );
    tokio::pin!(conn);
    tokio::select! {
        _=&mut conn=>{},
        _=shutdown.cancelled()=>{
            conn.as_mut().graceful_shutdown();
            // Bound draining when a peer has stopped reading its event stream.
            let _=timeout(Duration::from_millis(500),conn).await;
        }
    }
}
async fn serve_connections(
    listener: TcpListener,
    app: Router,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let mut clients = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _=shutdown.cancelled()=>break,
            Some(_)=clients.join_next(), if !clients.is_empty()=>{},
            accepted=listener.accept()=>{
                let (socket,_)=accepted.map_err(|e|e.to_string())?;
                clients.spawn(connection(socket,app.clone(),shutdown.clone()));
            }
        }
    }
    drop(listener);
    while clients.join_next().await.is_some() {}
    Ok(())
}

async fn start(
    source: Source,
    mut config: ServerConfig,
    tunnel: bool,
    heartbeat: Duration,
) -> Result<(RunningTownServer, Option<CloudflareTunnel>), String> {
    if config.poll_ms == 0 {
        return Err("--poll-ms must be at least 1".into());
    }
    config.token = config.token.filter(|s| !s.is_empty());
    let host = match config.host.as_str() {
        "" => "0.0.0.0",
        "localhost" => "127.0.0.1",
        host => host,
    };
    let listener = TcpListener::bind((host, config.port))
        .await
        .map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let host = config.host.clone();
    let token = config.token.clone();
    let (access, tunnel) = spawn_blocking(move || -> Result<_, String> {
        let mut access = discover_access_urls(port, &host, token.as_deref());
        let tunnel = tunnel
            .then(|| CloudflareTunnel::start(port, 20))
            .transpose()?;
        if let Some(t) = &tunnel {
            access.push(AccessUrl {
                kind: "tunnel".into(),
                label: "Public HTTPS Tunnel".into(),
                url: with_token(&format!("{}/", t.url), token.as_deref()),
            });
        }
        Ok((access, tunnel))
    })
    .await
    .map_err(|e| e.to_string())??;
    let shutdown = CancellationToken::new();
    let town = Arc::new(Town {
        source,
        config,
        access: access.clone(),
        shutdown: shutdown.clone(),
        heartbeat,
        event_feeds: Mutex::new(HashMap::new()),
    });
    let app = Router::new().fallback(handle).with_state(town);
    let signal = shutdown.clone();
    let task = tokio::spawn(serve_connections(listener, app, signal));
    Ok((
        RunningTownServer {
            port,
            access_urls: access,
            shutdown,
            task,
        },
        tunnel,
    ))
}
pub async fn start_town_server(
    snapshot_fn: SnapshotProvider,
    config: ServerConfig,
) -> Result<RunningTownServer, String> {
    Ok(start(Source::Town(snapshot_fn), config, false, KEEPALIVE)
        .await?
        .0)
}
pub async fn start_hub_server(
    watch_dirs: Vec<PathBuf>,
    config: ServerConfig,
) -> Result<RunningTownServer, String> {
    Ok(start(Source::Hub(watch_dirs), config, false, KEEPALIVE)
        .await?
        .0)
}
fn open_url(url: &str) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    let mut opener = std::process::Command::new("open");
    #[cfg(windows)]
    let mut opener = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut opener = std::process::Command::new("xdg-open");
    opener.arg(url).status()
}
fn serve_blocking(
    source: Source,
    mut config: ServerConfig,
    open_browser: bool,
    tunnel: bool,
    no_auth: bool,
) -> Result<(), String> {
    if tunnel
        && !matches!(
            config.host.as_str(),
            "" | "127.0.0.1" | "localhost" | "0.0.0.0" | "::"
        )
    {
        return Err("--tunnel requires a bind that accepts IPv4 connections on 127.0.0.1.".into());
    }
    config.token = if no_auth {
        None
    } else {
        config
            .token
            .filter(|s| !s.is_empty())
            .or_else(|| tunnel.then(generate_token))
    };
    tokio::runtime::Runtime::new().map_err(|e|e.to_string())?.block_on(async move{
        let (server,_tunnel)=start(source,config,tunnel,KEEPALIVE).await?;
        let local=&server.access_urls[0];
        println!("Hero Town is live. {}: {}",local.label,local.url);
        for entry in &server.access_urls[1..]{println!("  {}: {}",entry.label,entry.url);}
        if open_browser {
            let url=local.url.clone();
            tokio::spawn(async move{
                if !matches!(spawn_blocking(move||open_url(&url)).await,Ok(Ok(s)) if s.success()){
                    tracing::warn!("Could not open a browser.");
                }
            });
        }
        let shutdown=server.shutdown.clone();
        let wait=server.wait();
        tokio::pin!(wait);
        tokio::select! {
            result=&mut wait=>result,
            signal=tokio::signal::ctrl_c()=>{
                shutdown.cancel();
                wait.await?;
                signal.map_err(|e|e.to_string())
            }
        }
    })
}
pub fn serve_town_blocking(
    snapshot_fn: SnapshotProvider,
    config: ServerConfig,
    open_browser: bool,
    tunnel: bool,
    no_auth: bool,
) -> Result<(), String> {
    serve_blocking(
        Source::Town(snapshot_fn),
        config,
        open_browser,
        tunnel,
        no_auth,
    )
}
pub fn serve_hub_blocking(
    watch_dirs: Vec<PathBuf>,
    config: ServerConfig,
    open_browser: bool,
    tunnel: bool,
    no_auth: bool,
) -> Result<(), String> {
    serve_blocking(
        Source::Hub(watch_dirs),
        config,
        open_browser,
        tunnel,
        no_auth,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::{files::write_json, qa_metrics};
    use reqwest::{Client, RequestBuilder};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn config(token: Option<&str>, read_only: bool) -> ServerConfig {
        ServerConfig {
            port: 0,
            poll_ms: 10,
            token: token.map(str::to_owned),
            read_only,
            ..Default::default()
        }
    }
    fn provider(snap: Value) -> SnapshotProvider {
        Arc::new(move || Ok(snap.clone()))
    }
    fn base(s: &RunningTownServer) -> String {
        format!("http://127.0.0.1:{}", s.port)
    }
    fn client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }
    async fn checked(req: RequestBuilder, expected: StatusCode) -> reqwest::Response {
        let res = req.send().await.unwrap();
        assert_eq!(res.status(), expected, "{} {}", res.url(), res.status());
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(res.headers()[header::REFERRER_POLICY], "no-referrer");
        res
    }
    async fn document(req: RequestBuilder) -> Value {
        checked(req, StatusCode::OK).await.json().await.unwrap()
    }
    async fn stop(server: RunningTownServer) {
        server.stop();
        timeout(Duration::from_secs(1), server.wait())
            .await
            .expect("server shutdown stalled")
            .unwrap();
    }
    #[tokio::test]
    async fn telemetry_and_workboard_reads() {
        use StatusCode as S;
        let local = Local::new();
        let packet = local.state.with_file_name("packet.json");
        write_json(&packet, &json!({"output_path":"result.json"}), false).unwrap();
        write_json(
            &local.state,
            &json!({"nodes":{"work":{"status":"awaiting_subagents","packet_path":packet}}}),
            false,
        )
        .unwrap();
        let town = start_town_server(local.provider(), config(Some("test"), true))
            .await
            .unwrap();
        let url = base(&town);
        let c = client();
        let get = |p: &str| c.get(format!("{url}/api/{p}")).bearer_auth("test");
        for endpoint in ["telemetry", "workboard"] {
            checked(c.get(format!("{url}/api/{endpoint}")), S::UNAUTHORIZED).await;
        }
        assert_eq!(document(get("telemetry")).await, json!({"available":false}));
        let history = local.state.with_file_name("telemetry.jsonl");
        for (id, workflow) in [("one", "alpha"), ("two", "alpha"), ("three", "beta")] {
            let run = telemetry::RunTelemetry {
                version: 1,
                run_id: id.into(),
                workflow_ref: Some(workflow.into()),
                ..Default::default()
            };
            telemetry::append_run_telemetry(&history, &run).unwrap();
        }
        let all = document(get("telemetry")).await;
        assert_eq!(all["available"], true);
        assert_eq!(all["runs_analyzed"], 3);
        assert_eq!(all["latest_run"]["run_id"], "three");
        assert!(all["analysis"].is_object());
        let filtered = document(get("telemetry?last=1&workflow=alpha")).await;
        assert_eq!(filtered["runs_analyzed"], 1);
        assert_eq!(filtered["latest_run"]["run_id"], "two");
        assert_eq!(
            document(get("telemetry?workflow=missing")).await,
            json!({"available":false})
        );
        let trace = document(get("telemetry?chrome_trace=1")).await;
        assert!(trace["traceEvents"].is_array());
        assert!(trace.get("analysis").is_none());
        for query in ["last=0", "last=no"] {
            checked(get(&format!("telemetry?{query}")), S::BAD_REQUEST).await;
        }
        assert!(document(get("workboard")).await["items"][0]["lease"].is_null());
        workboard::claim_work(
            &local.state,
            Some(&local.report),
            None,
            "agent",
            60,
            None,
            None,
        )
        .unwrap();
        let board = document(get("workboard")).await;
        assert_eq!(board["items"][0]["lease"]["worker_id"], "agent");
        assert_eq!(
            board["items"][0]["expected_outputs"],
            json!(["result.json"])
        );
        #[cfg(unix)]
        {
            let outside = local._temp.path().join("private.jsonl");
            std::fs::rename(&history, &outside).unwrap();
            std::os::unix::fs::symlink(&outside, &history).unwrap();
            checked(get("telemetry"), S::FORBIDDEN).await;
        }
        stop(town).await;
    }

    #[tokio::test]
    async fn hub_work_data_requires_registered_local_run() {
        // Confine ingestion and registry writes to an isolated test process.
        const CHILD: &str = "KGRAPH_WORK_DATA_HUB_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = spawn_blocking(|| {
                let home = tempdir().unwrap();
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "server::tests::hub_work_data_requires_registered_local_run",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("KGRAPH_HOME", home.path())
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let local = Local::new();
        let project = local.state.parent().unwrap().parent().unwrap().to_owned();
        let hub = start_hub_server(vec![project], config(Some("test"), true))
            .await
            .unwrap();
        let c = client();
        let url = base(&hub);
        let get = |p: &str| c.get(format!("{url}/api/runs{p}")).bearer_auth("test");
        let runs = document(get("")).await;
        let id = runs["runs"][0]["id"].as_str().unwrap();
        let remote = json!({"run":{"run_id":"foreign","source_host":"other-host"},"snapshot":local.provider()().unwrap()});
        document(
            c.post(format!("{url}/api/ingest"))
                .bearer_auth("test")
                .json(&remote),
        )
        .await;
        for op in ["telemetry", "workboard"] {
            checked(get(&format!("/missing/{op}")), StatusCode::NOT_FOUND).await;
            checked(get(&format!("/foreign/{op}")), StatusCode::FORBIDDEN).await;
        }
        assert_eq!(
            document(get(&format!("/{id}/telemetry"))).await,
            json!({"available":false})
        );
        assert_eq!(
            document(get(&format!("/{id}/workboard"))).await,
            json!({"items":[]})
        );
        stop(hub).await;
    }

    async fn raw(port: u16, method: &str, path: &str, headers: &str, body: &[u8]) -> String {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n"
        )
        .into_bytes();
        request.extend_from_slice(body);
        sock.write_all(&request).await.unwrap();
        let mut result = Vec::new();
        timeout(Duration::from_secs(2), sock.read_to_end(&mut result))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8(result).unwrap()
    }
    #[tokio::test]
    async fn public_assets_authentication_cookie_and_redirects() {
        use StatusCode as S;
        let token = "a /+";
        let s = start_town_server(
            provider(json!({"title":"Demo","nodes":[],"edges":[]})),
            config(Some(token), false),
        )
        .await
        .unwrap();
        let base = base(&s);
        let c = client();
        let get = |p: &str| c.get(format!("{base}{p}"));
        assert_eq!(
            document(get("/api/health")).await,
            json!({"ok":true,"mode":"single"})
        );
        for (path, content_type, body) in [
            (
                "/manifest.webmanifest",
                "application/manifest+json; charset=utf-8",
                MANIFEST,
            ),
            ("/icon.svg", "image/svg+xml; charset=utf-8", ui::APP_ICON),
        ] {
            let r = checked(get(path), S::OK).await;
            assert_eq!(r.headers()[header::CONTENT_TYPE], content_type);
            assert_eq!(r.text().await.unwrap(), body);
        }
        let locked = checked(get("/?run=%22%3C"), S::UNAUTHORIZED)
            .await
            .text()
            .await
            .unwrap();
        assert!(locked.contains("value=\"&quot;&lt;\""));
        assert_eq!(
            checked(get("/api/runs"), S::UNAUTHORIZED)
                .await
                .json::<Value>()
                .await
                .unwrap(),
            json!({"error":"unauthorized"})
        );
        let redirect = checked(
            get("/?q=a&q=b&token=a+%2F%2B&empty=").header("x-forwarded-proto", "HTTPS"),
            S::FOUND,
        )
        .await;
        assert_eq!(redirect.headers()[header::LOCATION], "/?q=a&q=b&empty=");
        assert_eq!(
            redirect.headers()[header::SET_COOKIE],
            "kgraph_token=a%20%2F%2B; HttpOnly; SameSite=Lax; Path=/; Secure"
        );
        assert_eq!(
            document(get("/api/snapshot").header(header::COOKIE, "x=y; kgraph_token=a%20%2F%2B"))
                .await["title"],
            "Demo"
        );
        assert_eq!(
            document(get("/api/snapshot?token=a+%2F%2B")).await["title"],
            "Demo"
        );
        assert_eq!(
            document(get("/api/snapshot").header(header::AUTHORIZATION, "bEaReR a /+")).await
                ["title"],
            "Demo"
        );
        let auth = |body: &str| c.post(format!("{base}/auth")).body(body.to_owned());
        checked(auth("token=no&run=remote"), S::UNAUTHORIZED).await;
        let ok = checked(auth("token=a+%2F%2B&run=two+words"), S::FOUND).await;
        assert_eq!(ok.headers()[header::LOCATION], "/?run=two+words");
        assert!(!ok.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Secure"));
        for (frame, expected) in [
            ("Content-Length: bad\r\n", 400),
            ("Content-Length: -1\r\n", 400),
            ("Content-Length: 12\r\nContent-Length: 12\r\n", 400),
            ("Content-Length: 0\r\nTransfer-Encoding: chunked\r\n", 400),
            ("Content-Length: 4097\r\n", 413),
            ("", 400),
        ] {
            let response = raw(s.port, "POST", "/auth", frame, b"token=secret").await;
            assert!(
                response.starts_with(&format!("HTTP/1.1 {expected}")),
                "{response}"
            );
            assert!(response.to_lowercase().contains("cache-control: no-store"));
        }
        checked(
            c.post(format!("{base}/auth")).body(vec![255]),
            S::BAD_REQUEST,
        )
        .await;
        stop(s).await;
    }

    #[tokio::test]
    async fn origin_framing_and_tokenless_permissions() {
        use StatusCode as S;
        let snap = json!({"title":"Demo","attention":{"input_gate":{"can_respond":true}}});
        let c = client();
        let s = start_town_server(provider(snap.clone()), config(None, false))
            .await
            .unwrap();
        let url = base(&s);
        assert_eq!(
            document(c.get(format!("{url}/api/snapshot"))).await["attention"]["input_gate"]
                ["can_respond"],
            false
        );
        for path in ["/api/respond", "/api/qa"] {
            checked(
                c.post(format!("{url}{path}")).json(&json!({})),
                S::UNAUTHORIZED,
            )
            .await;
        }
        assert!(checked(c.get(format!("{url}/")), S::OK)
            .await
            .text()
            .await
            .unwrap()
            .contains("\"respond\": null"));
        stop(s).await;
        let s = start_town_server(provider(snap), config(Some("secret"), false))
            .await
            .unwrap();
        let url = base(&s);
        let submit = || {
            c.post(format!("{url}/api/qa"))
                .bearer_auth("secret")
                .json(&json!({}))
        };
        for (key, value) in [
            ("origin", "null"),
            ("origin", "https://foreign.invalid"),
            ("sec-fetch-site", "same-site"),
            ("sec-fetch-site", "cross-site"),
        ] {
            checked(submit().header(key, value), S::FORBIDDEN).await;
        }
        for (key, value) in [
            ("Origin", "http://localhost"),
            ("Sec-Fetch-Site", "same-origin"),
            ("Host", "localhost"),
        ] {
            let mut headers =
                format!("Authorization: Bearer secret\r\nContent-Length: 2\r\n{key}: {value}\r\n");
            if key != "Host" {
                headers.push_str(&format!("{key}: {value}\r\n"));
            }
            assert!(raw(s.port, "POST", "/api/qa", &headers, b"{}")
                .await
                .starts_with("HTTP/1.1 403"));
        }
        checked(
            submit()
                .header("origin", &url)
                .header("sec-fetch-site", "same-origin"),
            S::NOT_FOUND,
        )
        .await;
        checked(
            submit()
                .header("x-forwarded-proto", "https")
                .header("origin", url.replace("http:", "https:")),
            S::NOT_FOUND,
        )
        .await;
        let duplicates = "Content-Length: 2\r\nContent-Length: 2\r\n";
        assert!(raw(s.port, "POST", "/api/respond", duplicates, b"{}")
            .await
            .starts_with("HTTP/1.1 401"));
        let duplicates = format!("Authorization: Bearer secret\r\n{duplicates}");
        assert!(raw(s.port, "POST", "/api/respond", &duplicates, b"{}")
            .await
            .starts_with("HTTP/1.1 400"));
        for body in ["[]", "null", "{\"x\": NaN}", "{"] {
            checked(
                c.post(format!("{url}/api/respond"))
                    .bearer_auth("secret")
                    .body(body),
                S::BAD_REQUEST,
            )
            .await;
        }
        let headers = "Authorization: Bearer secret\r\nContent-Length: 262145\r\n";
        assert!(raw(s.port, "POST", "/api/respond", headers, b"")
            .await
            .starts_with("HTTP/1.1 413"));
        stop(s).await;
        let bad = ServerConfig {
            poll_ms: 0,
            ..Default::default()
        };
        assert_eq!(
            start_town_server(provider(json!({})), bad)
                .await
                .err()
                .unwrap(),
            "--poll-ms must be at least 1"
        );
    }

    // SSE records can span transport chunks or share a chunk.
    struct Events {
        response: reqwest::Response,
        buffer: Vec<u8>,
    }
    impl Events {
        async fn open(req: RequestBuilder) -> Self {
            let response = checked(req, StatusCode::OK).await;
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/event-stream; charset=utf-8"
            );
            assert_eq!(response.headers()[header::CONNECTION], "keep-alive");
            assert_eq!(response.headers()["x-accel-buffering"], "no");
            Self {
                response,
                buffer: vec![],
            }
        }
        async fn record(&mut self) -> String {
            timeout(Duration::from_secs(2), async {
                while !self.buffer.windows(2).any(|w| w == b"\n\n") {
                    self.buffer.extend_from_slice(
                        &self.response.chunk().await.unwrap().expect("missing event"),
                    );
                }
                let end = self.buffer.windows(2).position(|w| w == b"\n\n").unwrap() + 2;
                String::from_utf8(self.buffer.drain(..end).collect()).unwrap()
            })
            .await
            .unwrap()
        }
        async fn snapshot(&mut self) -> Value {
            loop {
                let event = self.record().await;
                if event == ": keepalive\n\n" {
                    continue;
                }
                let data = event
                    .strip_prefix("event: snapshot\ndata: ")
                    .unwrap()
                    .trim_end();
                return serde_json::from_str(data).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn event_changes_ignore_timestamps_and_key_order_and_shutdown_open_stream() {
        let calls = Arc::new(AtomicUsize::new(0));
        let revision = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let changed = revision.clone();
        let provider: SnapshotProvider = Arc::new(move || {
            let i = counter.fetch_add(1, Ordering::SeqCst);
            let nested = if i.is_multiple_of(2) {
                json!({"a":1,"b":2})
            } else {
                json!({"b":2,"a":1})
            };
            Ok(json!({"revision":changed.load(Ordering::SeqCst),"generated_at":i,"nested":nested}))
        });
        let (s, _) = start(
            Source::Town(provider),
            config(Some("secret"), false),
            false,
            Duration::from_millis(90),
        )
        .await
        .unwrap();
        let c = client();
        let req = || c.get(format!("{}/api/events", base(&s)));
        checked(req(), StatusCode::UNAUTHORIZED).await;
        let mut stream = Events::open(req().bearer_auth("secret")).await;
        let mut other = Events::open(
            c.get(format!("{}/api/events?run=ignored", base(&s)))
                .bearer_auth("secret"),
        )
        .await;
        let first = stream.snapshot().await;
        assert_eq!(first["revision"], 0);
        assert_eq!(other.snapshot().await, first);
        assert_eq!(stream.record().await, ": keepalive\n\n");
        assert!(calls.load(Ordering::SeqCst) > 1);
        revision.store(1, Ordering::SeqCst);
        let updated = stream.snapshot().await;
        assert_eq!(updated["revision"], 1);
        assert_eq!(other.snapshot().await, updated);
        stop(s).await;
        for mut events in [stream, other] {
            timeout(Duration::from_secs(1), async {
                while events.response.chunk().await.unwrap().is_some() {}
            })
            .await
            .expect("event stream did not close");
        }
    }

    struct Local {
        _temp: tempfile::TempDir,
        state: PathBuf,
        report: PathBuf,
        log: PathBuf,
    }
    impl Local {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let state = temp
                .path()
                .canonicalize()
                .unwrap()
                .join(".kgraph/demo_state.json");
            let report = state.with_file_name("demo_report.json");
            let log = state.with_file_name("work.log");
            let missing = state.with_file_name("missing.log");
            write_json(
                &state,
                &json!({"nodes":{"stage":{"status":"running","outputs":[log,missing]}}}),
                true,
            )
            .unwrap();
            write_json(&report, &json!({}), false).unwrap();
            std::fs::write(
                &log,
                format!("{}\npassword=hidden\nfinished\n", "x".repeat(300)),
            )
            .unwrap();
            interactions::ask_node_question(
                &state,
                "stage",
                "pick",
                &json!({"question":"Proceed?"}),
            )
            .unwrap();
            qa_metrics::init_qa_checklist(
                &qa_metrics::run_qa_checklist_path(&state),
                "batch",
                &[json!({"id":"check","automated_status":"pass"})],
                "review",
                None,
                Some("Tester"),
                None,
                None,
                false,
            )
            .unwrap();
            Self {
                _temp: temp,
                state,
                report,
                log,
            }
        }
        fn provider(&self) -> SnapshotProvider {
            let state = self.state.clone();
            let report = self.report.clone();
            Arc::new(move || {
                Ok(build_town_snapshot_from_graph(
                    None,
                    Some(&state),
                    Some(&report),
                    None,
                    None,
                    None,
                    None,
                ))
            })
        }
        async fn verify_logs_qa(&self, url: &str, read_only: bool) {
            use StatusCode as S;
            let c = client();
            let qa = || c.get(format!("{url}/qa")).bearer_auth("secret");
            let log = |p: &Path| {
                c.get(format!("{url}/logs"))
                    .bearer_auth("secret")
                    .query(&[("path", p.to_str().unwrap()), ("max_bytes", "256")])
            };
            let tail = document(log(&self.log)).await;
            assert_eq!(tail["truncated"], true);
            let content = tail["content"].as_str().unwrap();
            assert!(content.contains("[redacted]") && !content.contains("hidden"));
            checked(log(&self.log.with_file_name("missing.log")), S::NOT_FOUND).await;
            checked(log(&self.log.with_file_name("unrecorded")), S::FORBIDDEN).await;
            let current = document(qa()).await;
            assert_eq!(current["available"], true);
            assert_eq!(current["can_edit"], !read_only);
            let payload = json!({"case_id":"check","manual_qa":"fail","gap_type":"ui_regression","notes":"overlaps","tester":"Tester"});
            let submit = c
                .post(format!("{url}/qa"))
                .bearer_auth("secret")
                .json(&payload);
            if read_only {
                checked(submit, S::FORBIDDEN).await;
            } else {
                assert_eq!(document(submit).await["ok"], true);
                let saved = document(qa()).await;
                assert_eq!(saved["scorecard"]["cases"][0]["manual_qa"], "fail");
                assert_eq!(saved["scorecard"]["cases"][0]["notes"], "overlaps");
            }
        }
    }
    fn answer(snap: &Value) -> Value {
        let gate = &snap["attention"]["input_gate"];
        json!({"node_path":gate["node_path"],"packet_hash":gate["packet_hash"],"decisions":{"q1":"Yes"}})
    }
    #[tokio::test]
    async fn local_log_qa_and_gate_round_trip() {
        let local = Local::new();
        let c = client();
        for ro in [true, false] {
            let s = start_town_server(local.provider(), config(Some("secret"), ro))
                .await
                .unwrap();
            let url = base(&s);
            local.verify_logs_qa(&format!("{url}/api"), ro).await;
            let runs = document(c.get(format!("{url}/api/runs")).bearer_auth("secret")).await;
            assert_eq!(runs["runs"][0]["state_path"], json!(local.state));
            let snap = document(c.get(format!("{url}/api/snapshot")).bearer_auth("secret")).await;
            assert_eq!(snap["attention"]["input_gate"]["can_respond"], !ro);
            let req = c
                .post(format!("{url}/api/respond"))
                .bearer_auth("secret")
                .json(&answer(&snap));
            if ro {
                checked(req, StatusCode::FORBIDDEN).await;
            } else {
                assert_eq!(document(req).await["delivery"], "local_subtask");
            }
            stop(s).await;
        }
    }
    #[tokio::test]
    async fn hub_selection_remote_queue_read_only_and_dismissal() {
        // Registry paths depend on process-wide KGRAPH_HOME. Child isolation
        // avoids touching the real registry or changing other tests' environment.
        const CHILD: &str = "KGRAPH_TOWN_HUB_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = spawn_blocking(|| {
                let home = tempdir().unwrap();
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "server::tests::hub_selection_remote_queue_read_only_and_dismissal",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("KGRAPH_HOME", home.path())
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        use StatusCode as S;
        let c = client();
        let s = start_hub_server(vec![], config(None, false)).await.unwrap();
        let url = base(&s);
        assert_eq!(
            document(c.get(format!("{url}/api/health"))).await["mode"],
            "hub"
        );
        assert_eq!(
            document(c.get(format!("{url}/api/runs"))).await["runs"],
            json!([])
        );
        let mut events = Events::open(c.get(format!("{url}/api/events"))).await;
        let empty = events.snapshot().await;
        assert_eq!(empty["title"], "Workflow Hub");
        assert_eq!(empty.get("run_id"), Some(&Value::Null));
        for route in ["/api/ingest", "/api/runs/origin/respond"] {
            checked(
                c.post(format!("{url}{route}")).json(&json!({})),
                S::UNAUTHORIZED,
            )
            .await;
        }
        checked(
            c.get(format!("{url}/api/runs/origin/response")),
            S::UNAUTHORIZED,
        )
        .await;
        stop(s).await;

        let local = Local::new();
        let project = local.state.parent().unwrap().parent().unwrap().to_owned();
        let mut upstream = local.provider()().unwrap();
        upstream["run_id"] = json!("upstream-id");
        let ingest =
            json!({"run":{"run_id":"origin","source_host":"foreign-worker"},"snapshot":upstream});
        for ro in [true, false] {
            let s = start_hub_server(vec![project.clone()], config(Some("secret"), ro))
                .await
                .unwrap();
            let url = base(&s);
            let get = |p: &str| c.get(format!("{url}{p}")).bearer_auth("secret");
            let post = |p: &str| c.post(format!("{url}{p}")).bearer_auth("secret");
            checked(
                c.post(format!("{url}/api/ingest")).json(&ingest),
                S::UNAUTHORIZED,
            )
            .await;
            checked(
                post("/api/ingest")
                    .header("sec-fetch-site", "cross-site")
                    .json(&ingest),
                S::FORBIDDEN,
            )
            .await;
            assert_eq!(
                document(post("/api/ingest").json(&ingest)).await["run"]["id"],
                "origin"
            );
            let runs = document(get("/api/runs")).await;
            let entries = runs["runs"].as_array().unwrap();
            assert_eq!(entries.len(), 2);
            let id = entries.iter().find(|r| r["id"] != "origin").unwrap()["id"]
                .as_str()
                .unwrap();
            let local_snap = document(get(&format!("/api/snapshot?run={id}"))).await;
            assert_eq!(local_snap["run_id"], id);
            local
                .verify_logs_qa(&format!("{url}/api/runs/{id}"), ro)
                .await;
            let r = checked(get("/api/snapshot?run=origin"), S::OK).await;
            assert_eq!(r.headers()["x-kgraph-run-id"], "origin");
            assert_eq!(r.json::<Value>().await.unwrap()["run_id"], "upstream-id");
            let mut events = Events::open(get("/api/events?run=origin")).await;
            let snap = events.snapshot().await;
            assert_eq!(snap["run_id"], "origin");
            assert_eq!(snap["attention"]["input_gate"]["can_respond"], !ro);
            checked(get("/api/runs/origin/logs?path=work.log"), S::FORBIDDEN).await;
            checked(get("/api/runs/unknown/qa"), S::NOT_FOUND).await;
            assert_eq!(
                document(get("/api/runs/origin/qa")).await,
                json!({"available":false})
            );
            let home = checked(get("/?run=origin"), S::OK)
                .await
                .text()
                .await
                .unwrap();
            for entry in [
                "\"run\": \"origin\"",
                "\"runs\": \"/api/runs\"",
                "\"events\": \"/api/events\"",
            ] {
                assert!(home.contains(entry));
            }
            checked(get("/api/runs/origin/response"), S::NOT_FOUND).await;
            let delete = || {
                c.delete(format!("{url}/api/runs/origin"))
                    .bearer_auth("secret")
            };
            if ro {
                checked(
                    post("/api/runs/origin/respond").json(&answer(&snap)),
                    S::FORBIDDEN,
                )
                .await;
                checked(delete(), S::FORBIDDEN).await;
                checked(post("/api/runs/origin/delete"), S::FORBIDDEN).await;
                assert_eq!(
                    document(get("/api/runs")).await["runs"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
            } else {
                checked(delete().header("origin", "null"), S::FORBIDDEN).await;
                let decision =
                    document(post("/api/runs/origin/respond").json(&answer(&snap))).await;
                assert_eq!(decision["delivery"], "remote_queue");
                let queued = document(get("/api/runs/origin/response")).await;
                assert_eq!(queued["decisions"], decision["decision"]["decisions"]);
                assert_eq!(
                    queued["packet_hash"],
                    snap["attention"]["input_gate"]["packet_hash"]
                );
                assert_eq!(
                    document(delete()).await,
                    json!({"ok":true,"removed":"origin"})
                );
                let queued_path = registry::kgraph_home().join("responses/origin.json");
                assert!(!queued_path.exists());
                checked(get("/api/runs/origin/response"), S::NOT_FOUND).await;
                // A leftover file from an older hub cannot expose a deleted run.
                write_json(&queued_path, &queued, true).unwrap();
                checked(get("/api/runs/origin/response"), S::NOT_FOUND).await;
                checked(post("/api/runs/origin/delete"), S::NOT_FOUND).await;
                document(post("/api/ingest").json(&ingest)).await;
                checked(get("/api/runs/origin/response"), S::NOT_FOUND).await;
                assert!(
                    document(get("/api/snapshot?run=origin")).await["attention"]["input_gate"]
                        ["submitted"]
                        .is_null()
                );
                assert_eq!(
                    document(post(&format!("/api/runs/{id}/delete"))).await["removed"],
                    id
                );
            }
            stop(s).await;
        }
    }
}
