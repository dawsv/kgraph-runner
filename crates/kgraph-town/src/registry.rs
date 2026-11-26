//! Machine-wide run inventory, including locally registered and pushed snapshots.
use crate::{
    interactions::{now, pairs, progress_path_for_state, read_json, resolve, s, safe_run_id},
    snapshot::{parse_timestamp, STATUSES},
};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use kgraph_core::{
    files::{file_lock, valid_id, write_json, FileLock},
    hash::{sha256_bytes, sha256_json},
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

fn absolute(p: &Path) -> PathBuf {
    resolve(p).unwrap_or_else(|_| p.to_owned())
}
fn expand(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = env::home_dir() {
            return home.join(rest);
        }
    }
    p.to_owned()
}
pub fn kgraph_home() -> PathBuf {
    absolute(
        &env::var_os("KGRAPH_HOME")
            .filter(|s| !s.is_empty())
            .map(|s| expand(Path::new(&s)))
            .unwrap_or_else(|| env::home_dir().unwrap_or_default().join(".kgraph")),
    )
}
pub(crate) fn registry_lock() -> Result<FileLock, String> {
    file_lock(&kgraph_home().join("registry.lock"))
}
fn read(path: &Path) -> Value {
    read_json(path, None).unwrap_or_else(|_| json!({}))
}
fn stamp(v: &Value) -> DateTime<Utc> {
    parse_timestamp(v.as_str())
}
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // gethostname writes at most the supplied buffer length.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..len]).to_lowercase();
        }
    }
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Unknown host".into())
        .to_lowercase()
}
#[derive(serde::Deserialize, Default)]
struct Inventory {
    runs: Map<String, Value>,
    #[serde(default)]
    dismissed: Map<String, Value>,
}
impl Inventory {
    fn load() -> Result<Self, String> {
        let path = kgraph_home().join("registry.json");
        match read_json(&path, None) {
            Ok(doc) => serde_json::from_value(doc).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !path.is_symlink() => {
                Ok(Self::default())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
    fn save(&self) -> Result<(), String> {
        let mut d = json!({"version":1,"runs":self.runs});
        if !self.dismissed.is_empty() {
            d["dismissed"] = json!(self.dismissed);
        }
        write_json(&kgraph_home().join("registry.json"), &d, true)
    }
}
pub(crate) fn registered_run(id: &str) -> Result<Option<Value>, String> {
    Ok(Inventory::load()?.runs.remove(id))
}
fn remove_artifact(directory: &str, id: &str) -> Result<(), String> {
    let path = kgraph_home().join(directory).join(format!("{id}.json"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
fn slug(s: &str) -> String {
    static SEP: Lazy<Regex> = Lazy::new(|| Regex::new("[^a-z0-9]+").unwrap());
    let lowercase = s.to_lowercase();
    let clean = SEP.replace_all(&lowercase, "-");
    let result: String = clean.trim_matches('-').chars().take(48).collect();
    if result.is_empty() {
        "run".into()
    } else {
        result
    }
}
fn normcase(s: &str) -> String {
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s.to_owned()
    }
}
#[allow(clippy::too_many_arguments)]
fn entry(
    workflow: Option<&Path>,
    state: &Path,
    report: &Path,
    project: &Path,
    factory: &str,
    status: &str,
    updated: Option<&str>,
    name: Option<&str>,
    project_name: Option<&str>,
) -> Value {
    let derived_name = workflow
        .and_then(Path::file_stem)
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| {
            let n = state
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            n.strip_suffix("_state").unwrap_or(n)
        });
    let name = name.filter(|s| !s.is_empty()).unwrap_or(derived_name);
    let project_name = project_name
        .filter(|s| !s.is_empty())
        .or_else(|| project.file_name().and_then(|s| s.to_str()))
        .unwrap_or_else(|| project.to_str().unwrap_or_default());
    let digest = sha256_bytes(state.as_os_str().as_encoded_bytes());
    json!({
        "id": format!("{}-{}-{}",slug(project_name),slug(name),&digest[..8]),
        "name": name,
        "project_name": project_name,
        "project_dir": project,
        "workflow_path": workflow,
        "state_path": state,
        "report_path": report,
        "factory": factory,
        "status": status,
        "updated_at": updated,
    })
}
#[allow(clippy::too_many_arguments)]
pub fn register_run(
    workflow: &Path,
    state: &Path,
    report: &Path,
    project: Option<&Path>,
    factory: &str,
    status: &str,
    name: Option<&str>,
    project_name: Option<&str>,
    expected_updated_at: Option<&str>,
) -> Result<Option<Value>, String> {
    let _guard = registry_lock()?;
    let mut inv = Inventory::load()?;
    let e = entry(
        Some(&absolute(workflow)),
        &absolute(state),
        &absolute(report),
        &absolute(project.unwrap_or(Path::new("."))),
        factory,
        status,
        Some(&now()),
        name,
        project_name,
    );
    let id = s(&e["id"]);
    if let Some(expected) = expected_updated_at {
        if inv.runs.get(id).and_then(|v| v["updated_at"].as_str()) != Some(expected) {
            return Ok(inv.runs.get(id).cloned());
        }
    }
    if !inv.runs.contains_key(id) {
        remove_artifact("responses", id)?;
    }
    inv.dismissed.remove(&normcase(s(&e["state_path"])));
    inv.runs.insert(id.to_owned(), e.clone());
    inv.save()?;
    Ok(Some(e))
}
fn windows_path(s: &str) -> bool {
    s.contains('\\') || s.starts_with("//") || s.as_bytes().get(1) == Some(&b':')
}
fn foreign_absolute(p: &str, windows: bool) -> bool {
    p.starts_with('/')
        || (windows
            && (p.starts_with('\\')
                || (p.as_bytes().get(1) == Some(&b':')
                    && p.as_bytes()
                        .get(2)
                        .is_some_and(|b| matches!(b, b'/' | b'\\')))))
}
fn normalize_origin(p: &str, windows: bool) -> String {
    let mut value = p.to_owned();
    if windows {
        value = value.replace('\\', "/").to_lowercase();
    }
    let (prefix, rest) = if windows && value.as_bytes().get(1) == Some(&b':') {
        (&value[..2], &value[2..])
    } else {
        ("", value.as_str())
    };
    let double = rest.starts_with("//") && !rest.starts_with("///");
    let mut parts = vec![];
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() > if double && windows { 2 } else { 0 } {
                    parts.pop();
                }
            }
            _ => parts.push(part),
        }
    }
    let path = format!(
        "{prefix}{}{}",
        if double { "//" } else { "/" },
        parts.join("/")
    );
    if windows {
        path.replace('/', "\\")
    } else {
        path
    }
}
fn snapshot_state(snapshot: &Value, local: bool) -> Option<String> {
    let state = snapshot["state_file"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && !s.contains('\0'))?;
    let mut windows = windows_path(state) || (local && cfg!(windows));
    let mut p = state.to_owned();
    if !foreign_absolute(state, windows) {
        let mut base = snapshot["project_dir"].as_str().unwrap_or("").to_owned();
        if base.is_empty() {
            let workflow = s(&snapshot["workflow_ref"]);
            windows |= windows_path(workflow);
            let last = if windows {
                workflow.rfind(['\\', '/'])
            } else {
                workflow.rfind('/')
            };
            if let Some(i) = last {
                let at_root =
                    i == 0 || (windows && i == 2 && workflow.as_bytes().get(1) == Some(&b':'));
                base = workflow[..if at_root { i + 1 } else { i }].to_owned();
            }
        }
        if base.is_empty() && local {
            base = env::current_dir().ok()?.to_string_lossy().into_owned();
        }
        windows |= windows_path(&base);
        if !foreign_absolute(&base, windows) {
            return None;
        }
        p = format!("{}/{}", base.trim_end_matches(['\\', '/']), state);
    }
    if local && windows == cfg!(windows) {
        p = absolute(Path::new(&p)).to_string_lossy().into_owned();
    }
    Some(normalize_origin(&p, windows))
}
fn run_key(e: &Value, local: &str) -> Option<(String, String)> {
    let remote = e["source"] == "remote";
    let host = if remote {
        e["source_host"].as_str()?.to_lowercase()
    } else {
        local.to_owned()
    };
    let state = if s(&e["state_path"]).is_empty() && remote {
        snapshot_state(
            &read(Path::new(e["remote_snapshot_path"].as_str()?)),
            host == local,
        )
    } else {
        snapshot_state(&json!({"state_file":e["state_path"]}), host == local)
    }?;
    Some((host, state))
}
pub fn ingest_remote_run(meta: &Value, snapshot: &Value) -> Result<Value, String> {
    if !meta.is_object() || !snapshot.is_object() {
        return Err("run and snapshot must be JSON objects".into());
    }
    let field = |k: &str, default: &str| -> Result<String, String> {
        meta.get(k)
            .map_or(Some(default), Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("run.{k} must be a nonempty string"))
    };
    let source = field("source_host", "Unknown host")?;
    let host = source.to_lowercase();
    let local = hostname();
    let state = snapshot_state(snapshot, host == local);
    let key = state.as_ref().map(|s| (host.clone(), s.clone()));
    let project = field("project_name", "Remote project")?;
    let name = field(
        "name",
        snapshot["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("workflow"),
    )?;
    let status = field(
        "status",
        snapshot["status"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("pending"),
    )?;
    let mut id = if let Some(v) = meta.get("run_id").or_else(|| meta.get("id")) {
        v.as_str()
            .filter(|id| safe_run_id(id))
            .ok_or("run ID must be a safe slug of at most 128 characters")?
            .to_owned()
    } else {
        let identity = if let Some(p) = &state {
            json!([host, p])
        } else {
            json!([host, project, name])
        };
        format!(
            "remote-{}-{}-{}",
            slug(&project),
            slug(&name),
            &sha256_json(&identity)[..16]
        )
    };
    let _guard = registry_lock()?;
    let mut inv = Inventory::load()?;
    if key.is_some() {
        if let Some((prior, _)) = inv
            .runs
            .iter()
            .filter(|(_, e)| run_key(e, &local) == key)
            .max_by_key(|(k, e)| {
                (
                    e.get("report_path").is_some(),
                    *k == &id,
                    stamp(&e["updated_at"]),
                )
            })
        {
            id = prior.clone();
        }
    }
    for (old, e) in &inv.runs {
        if !old.eq_ignore_ascii_case(&id) {
            continue;
        }
        let previous = run_key(e, &local);
        if previous.is_some() && previous == key {
            continue;
        }
        if old != &id
            || e["source"] != "remote"
            || e["source_host"].as_str().map(str::to_lowercase).as_deref() != Some(&host)
            || (state.is_some() && previous.is_some())
        {
            return Err("run ID conflicts with an existing run".into());
        }
    }
    let snapshot_path = kgraph_home().join("remote").join(format!("{id}.json"));
    let mut e = inv
        .runs
        .get(&id)
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    e["id"] = json!(id);
    e["name"] = json!(name);
    e["project_name"] = json!(project);
    e["source"] = json!("remote");
    e["source_host"] = json!(source);
    e["remote_snapshot_path"] = json!(snapshot_path);
    e["status"] = json!(status);
    e["updated_at"] = json!(now());
    if let Some(p) = state {
        e["state_path"] = json!(p);
        if host == local {
            inv.dismissed.remove(&normcase(&p));
        }
    }
    if !inv.runs.contains_key(&id) {
        remove_artifact("responses", &id)?;
    }
    write_json(&snapshot_path, snapshot, false)?;
    inv.runs.insert(id, e.clone());
    inv.save()?;
    Ok(e)
}
fn discover(dir: &Path) -> Vec<Value> {
    let project = absolute(&expand(dir));
    let Ok(entries) = fs::read_dir(project.join(".kgraph")) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_name()?.to_str()?.strip_suffix("_state.json")?;
            let report = path.with_file_name(format!("{stem}_report.json"));
            let r = read(&report);
            let workflow = r["workflow_ref"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| project.join(s))
                .unwrap_or_else(|| project.join(format!("{stem}.py")));
            Some(entry(
                Some(&absolute(&workflow)),
                &absolute(&path),
                &absolute(&report),
                &project,
                "build_graph",
                "pending",
                None,
                None,
                None,
            ))
        })
        .collect()
}
pub fn remove_run(id: &str, watch_dirs: &[PathBuf]) -> Result<bool, String> {
    if !valid_id(id) {
        return Ok(false);
    }
    let _guard = registry_lock()?;
    let mut inv = Inventory::load()?;
    let watched: Vec<_> = watch_dirs.iter().flat_map(|d| discover(d)).collect();
    let e = inv.runs.get(id).or_else(|| {
        watched
            .iter()
            .find(|v| v["id"] == id && !inv.dismissed.contains_key(&normcase(s(&v["state_path"]))))
    });
    let Some(e) = e else { return Ok(false) };
    let local = hostname();
    let key = run_key(e, &local);
    let mut ids = HashSet::from([id.to_owned()]);
    if key.is_some() {
        ids.extend(
            inv.runs
                .iter()
                .filter(|(_, v)| run_key(v, &local) == key)
                .map(|(k, _)| k.clone()),
        );
        for w in &watched {
            if run_key(w, &local) == key {
                ids.insert(s(&w["id"]).to_owned());
                inv.dismissed
                    .insert(normcase(s(&w["state_path"])), json!(true));
            }
        }
    }
    for key in ids {
        if valid_id(&key) {
            for directory in ["responses", "remote"] {
                remove_artifact(directory, &key)?;
            }
        }
        inv.runs.remove(&key);
    }
    inv.save()?;
    Ok(true)
}
fn modified(p: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::from)
}
fn live_entry(mut e: Value) -> Value {
    let state = PathBuf::from(s(&e["state_path"]));
    let report = PathBuf::from(s(&e["report_path"]));
    let state_time = modified(&state);
    let mut report_time = modified(&report);
    if state_time.is_none() && report_time.is_none() {
        if e["status"] != "running" {
            e["status"] = json!("missing");
        }
        return e;
    }
    let overview = read(&report);
    let records = read(&state);
    let generated = stamp(&overview["generated_at"]);
    if generated.year() > 1 {
        report_time = Some(generated);
    }
    let mut updated = stamp(&e["updated_at"]);
    let mut status = s(&e["status"]).to_owned();
    if let (Some(t), Some(st)) = (report_time, overview["status"].as_str()) {
        if t >= updated {
            updated = t;
            status = st.to_owned();
        }
    }
    let mut fresh = HashSet::new();
    let mut latest = updated;
    for (_, r) in pairs(&records["nodes"]) {
        let t = parse_timestamp(
            r["updated_at"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| r["started_at"].as_str()),
        );
        let t = if t.year() == 1 { state_time } else { Some(t) };
        if let (Some(t), Some(st)) = (t, r["status"].as_str()) {
            if t > updated {
                fresh.insert(st);
                latest = latest.max(t);
            }
        }
    }
    if !fresh.is_empty() {
        if status == "running"
            && fresh
                .iter()
                .all(|k| matches!(*k, "passed" | "cached" | "pending"))
        {
            fresh.insert("running");
        }
        if let Some(s) = STATUSES.iter().find(|&&s| fresh.contains(s)) {
            status = (*s).to_owned();
        }
    }
    updated = latest;
    if updated.year() == 1 {
        updated = state_time.max(report_time).unwrap_or(updated);
    }
    if let Some(t) = modified(&progress_path_for_state(&state)) {
        updated = updated.max(t);
    }
    e["status"] = json!(status);
    e["updated_at"] = json!(updated.to_rfc3339_opts(SecondsFormat::Micros, false));
    e
}
pub fn list_runs(watch_dirs: &[PathBuf]) -> Vec<Value> {
    const LOCAL: &[&str] = &[
        "project_dir",
        "workflow_path",
        "state_path",
        "report_path",
        "factory",
    ];
    const REMOTE: &[&str] = &["source_host", "remote_snapshot_path"];
    let inv = Inventory::load().unwrap_or_default();
    let mut entries = HashMap::new();
    let mut state_paths = HashSet::new();
    for (id, e) in inv.runs {
        let remote = e["source"] == "remote";
        if e.is_object()
            && ["id", "name", "project_name", "status", "updated_at"]
                .iter()
                .chain(if remote { REMOTE } else { LOCAL })
                .all(|k| e[*k].is_string())
        {
            if !remote {
                state_paths.insert(s(&e["state_path"]).to_owned());
            }
            entries.insert(id, e);
        }
    }
    for e in watch_dirs.iter().flat_map(|d| discover(d)) {
        let p = s(&e["state_path"]);
        let id = s(&e["id"]);
        if !entries.contains_key(id)
            && !state_paths.contains(p)
            && !inv.dismissed.contains_key(&normcase(p))
        {
            state_paths.insert(p.to_owned());
            entries.insert(id.to_owned(), e);
        }
    }
    let mut all: Vec<_> = entries
        .into_values()
        .map(|e| {
            if e["source"] != "remote" {
                return live_entry(e);
            }
            if LOCAL.iter().all(|k| e[*k].is_string()) {
                let mut live = live_entry(e.clone());
                if stamp(&live["updated_at"]) > stamp(&e["updated_at"]) {
                    live["source"] = json!("local");
                    return live;
                }
            }
            e
        })
        .collect();
    let priority = |e: &Value| {
        (
            stamp(&e["updated_at"]),
            e["source"] == "remote",
            s(&e["id"]).to_owned(),
        )
    };
    all.sort_by_cached_key(|e| std::cmp::Reverse(priority(e)));
    let host = hostname();
    let mut seen = HashSet::new();
    all.retain(|e| run_key(e, &host).is_none_or(|k| seen.insert(k)));
    all
}
pub fn get_run(id: &str, watch_dirs: &[PathBuf]) -> Option<Value> {
    list_runs(watch_dirs).into_iter().find(|r| r["id"] == id)
}
pub fn single_run_entry(snapshot: &Value) -> Value {
    let workflow = snapshot["workflow_ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|p| absolute(Path::new(p)));
    let state = absolute(Path::new(
        snapshot["state_file"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(".kgraph/town_state.json"),
    ));
    let project = state
        .ancestors()
        .skip(1)
        .find(|p| p.file_name().is_some_and(|n| n == ".kgraph"))
        .and_then(Path::parent)
        .map(Path::to_owned)
        .unwrap_or_else(|| absolute(Path::new(".")));
    let stem = state
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let report = snapshot["report_file"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|p| absolute(Path::new(p)))
        .unwrap_or_else(|| {
            state.with_file_name(if stem == "state" {
                "report.json".to_owned()
            } else {
                format!(
                    "{}_report.json",
                    stem.strip_suffix("_state").unwrap_or(stem)
                )
            })
        });
    entry(
        workflow.as_deref(),
        &state,
        &report,
        &project,
        "build_graph",
        snapshot["status"].as_str().unwrap_or("pending"),
        snapshot["generated_at"].as_str(),
        snapshot["title"].as_str(),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactions::{
        apply_remote_response, ask_node_question, read_remote_response, submit_gate_response,
    };
    use crate::snapshot::build_town_snapshot_from_graph;
    #[test]
    fn local_remote_registry_and_queue() {
        // Isolate the process-wide home setting from other parallel Cargo tests.
        if env::var_os("KGRAPH_REGISTRY_TEST_CHILD").is_none() {
            let temp = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "registry::tests::local_remote_registry_and_queue",
                    "--nocapture",
                ])
                .env("KGRAPH_REGISTRY_TEST_CHILD", "1")
                .env("KGRAPH_HOME", temp.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let project = kgraph_home().join("demo");
        let state = project.join(".kgraph/demo_state.json");
        let report = state.with_file_name("demo_report.json");
        let workflow = project.join("demo.py");
        let watch = [project.clone()];
        write_json(
            &state,
            &json!({"nodes":{"stage":{"status":"running"}}}),
            true,
        )
        .unwrap();
        write_json(&report, &json!({}), true).unwrap();
        assert_eq!(list_runs(&watch).len(), 1);
        let e = register_run(
            &workflow,
            &state,
            &report,
            Some(&project),
            "build_graph",
            "running",
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
        let guarded = register_run(
            &workflow,
            &state,
            &report,
            Some(&project),
            "build_graph",
            "passed",
            None,
            None,
            Some("stale"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(e, guarded);
        ask_node_question(&state, "stage", "pick", &json!({"question":"Proceed?"})).unwrap();
        let snapshot = build_town_snapshot_from_graph(
            None,
            Some(&state),
            Some(&report),
            None,
            None,
            None,
            None,
        );
        let local = ingest_remote_run(&json!({"source_host":hostname()}), &snapshot).unwrap();
        assert_eq!(local["id"], e["id"]);
        assert_eq!(local["report_path"], e["report_path"]);
        assert_eq!(list_runs(&watch).len(), 1);
        let foreign = json!({"source_host":"other-worker","run_id":"origin"});
        let remote = ingest_remote_run(&foreign, &snapshot).unwrap();
        assert!(remote.get("report_path").is_none());
        assert_eq!(list_runs(&watch).len(), 2);
        assert!(ingest_remote_run(
            &json!({"source_host":"unrelated","run_id":"origin"}),
            &snapshot
        )
        .is_err());
        assert!(ingest_remote_run(&json!({"run_id":"con"}), &snapshot).is_err());
        let gate = &snapshot["attention"]["input_gate"];
        let payload = json!({
            "node_path": gate["node_path"],
            "decisions": {"q1":"Yes"},
            "submitted_at": null,
            "packet_hash": gate["packet_hash"],
        });
        let (code, v) = submit_gate_response(remote.as_object().unwrap(), &payload);
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["delivery"], "remote_queue");
        assert!(!state
            .with_file_name("artifacts")
            .join("pick_decision.json")
            .exists());
        let queued = read_remote_response("origin").unwrap();
        assert_eq!(queued["node_path"], gate["node_path"]);
        assert_eq!(queued["packet_hash"], gate["packet_hash"]);
        let stored = read(Path::new(s(&remote["remote_snapshot_path"])));
        assert_eq!(
            stored["attention"]["input_gate"]["submitted"]["answered_at"],
            v["answered_at"]
        );
        assert_eq!(stored["nodes"][0]["sub_progress"]["done"], 1);
        assert_eq!(
            stored["nodes"][0]["sub_graph"]["counts"]["awaiting_subagents"],
            0
        );
        ingest_remote_run(&foreign, &snapshot).unwrap();
        let replay = apply_remote_response(&snapshot, "origin");
        assert_eq!(
            replay["nodes"][0]["sub_graph"]["nodes"][0]["status"],
            "passed"
        );
        assert_eq!(
            submit_gate_response(remote.as_object().unwrap(), &payload).0,
            409
        );
        let mut changed = snapshot.clone();
        changed["attention"]["input_gate"]["packet_hash"] = json!("new");
        assert!(
            apply_remote_response(&changed, "origin")["attention"]["input_gate"]
                .get("submitted")
                .is_none()
        );
        assert!(remove_run(s(&e["id"]), &watch).unwrap());
        assert_eq!(list_runs(&watch).len(), 1);
        assert!(!remove_run(s(&e["id"]), &watch).unwrap());
        let inv = read(&kgraph_home().join("registry.json"));
        assert_eq!(inv["version"], 1);
        assert_eq!(inv["dismissed"][normcase(state.to_str().unwrap())], true);
        let inventory_path = kgraph_home().join("registry.json");
        let mut inventory = read(&inventory_path);
        inventory["runs"]["alias"] = remote.clone();
        inventory["runs"]["alias"]["id"] = json!("alias");
        write_json(&inventory_path, &inventory, true).unwrap();
        let alias_response = kgraph_home().join("responses/alias.json");
        write_json(&alias_response, &queued, true).unwrap();
        assert!(remove_run("origin", &watch).unwrap());
        assert!(!alias_response.exists());
        assert!(!kgraph_home().join("responses/origin.json").exists());
        assert_eq!(
            submit_gate_response(remote.as_object().unwrap(), &payload).0,
            404
        );
        assert!(list_runs(&watch).is_empty());
        register_run(
            &workflow,
            &state,
            &report,
            Some(&project),
            "build_graph",
            "running",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(list_runs(&watch).len(), 1);

        for damaged in [
            b"{truncated".as_slice(),
            br#"{"runs":[]}"#,
            br#"{"runs":{},"dismissed":null}"#,
        ] {
            fs::write(&inventory_path, damaged).unwrap();
            assert!(register_run(
                &workflow,
                &state,
                &report,
                Some(&project),
                "build_graph",
                "running",
                None,
                None,
                None
            )
            .is_err());
            assert!(ingest_remote_run(&foreign, &snapshot).is_err());
            assert!(remove_run("origin", &watch).is_err());
            assert_eq!(fs::read(&inventory_path).unwrap(), damaged);
        }
    }
    #[test]
    fn canonicalizes_origin_paths_without_using_the_hubs_path_conventions() {
        assert_eq!(snapshot_state(&json!({"state_file":"folder/../run.json","workflow_ref":"C:\\Projects\\workflow.py"}),false).as_deref(),Some("c:\\projects\\run.json"));
        assert_eq!(
            snapshot_state(
                &json!({"state_file":"a/../state.json","project_dir":"/srv/project"}),
                false
            )
            .as_deref(),
            Some("/srv/project/state.json")
        );
        assert_eq!(
            snapshot_state(&json!({"state_file":"run.json"}), false),
            None
        );
        assert_eq!(
            normalize_origin("\\\\SERVER\\share\\..\\run.json", true),
            "\\\\server\\share\\run.json"
        );
    }
}
