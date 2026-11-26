//! Live sub-task telemetry, local artifacts, and decision delivery.
use crate::redaction::{redact_credentials, redact_tail_with_lookback};
use chrono::{DateTime, SecondsFormat, Utc};
use kgraph_core::{
    files::{file_lock, valid_id, write_json},
    hash::sha256_bytes,
    qa_metrics,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

pub const PROGRESS_STATUSES: &[&str] = &[
    "pending",
    "running",
    "passed",
    "cached",
    "failed",
    "awaiting_subagents",
];
pub const ACTIVE_SUBTASK_PARENTS: &[&str] = &["awaiting_subagents", "running"];
pub const PACKET_LIMIT: usize = 256 * 1024;
const QUESTION_FIELDS: &[&str] = &[
    "question",
    "questions",
    "options",
    "recommended",
    "ticket",
    "multi_select",
    "allow_note",
];
pub(crate) const TASK_KINDS: &[&str] = &["code", "worker", "verifier", "gate"];

pub(crate) fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_default()
}
pub(crate) fn pairs(v: &Value) -> impl Iterator<Item = (&String, &Value)> {
    v.as_object().into_iter().flatten()
}
pub(crate) fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}
pub(crate) fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}
pub(crate) fn timestamp(t: std::time::SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Micros, false)
}
fn nonempty(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}
fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(redact_credentials)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn unique<T: Clone + Eq + std::hash::Hash>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|v| seen.insert(v.clone()))
        .collect()
}

pub(crate) fn regular(path: &Path, nofollow: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        if nofollow {
            crate::artifact::open_absolute(path, false)?
        } else {
            options.custom_flags(libc::O_NONBLOCK).open(path)?
        }
    };
    #[cfg(not(unix))]
    let f = {
        if nofollow && path.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Expected a regular file",
            ));
        }
        if !fs::metadata(path)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Expected a regular file",
            ));
        }
        options.open(path)?
    };
    if !f.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected a regular file",
        ));
    }
    Ok(f)
}

pub fn json_file(path: &Path, limit: Option<usize>) -> io::Result<(Value, fs::Metadata, Vec<u8>)> {
    let f = regular(path, false)?;
    let meta = f.metadata()?;
    let mut bytes = Vec::new();
    f.take(limit.map_or(u64::MAX, |n| n as u64 + 1))
        .read_to_end(&mut bytes)?;
    if limit.is_some_and(|n| bytes.len() > n) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON file exceeds the size limit",
        ));
    }
    let v: Value = serde_json::from_slice(&bytes)?;
    if !v.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected a JSON object",
        ));
    }
    Ok((v, meta, bytes))
}
pub(crate) fn read_json(path: &Path, limit: Option<usize>) -> io::Result<Value> {
    json_file(path, limit).map(|(v, _, _)| v)
}
fn optional_json(path: &Path) -> io::Result<Value> {
    match read_json(path, None) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        r => r,
    }
}
pub fn valid_node_path(v: &str) -> bool {
    !v.contains(['\\', ':'])
        && !v.chars().any(|c| c < ' ')
        && v.split('/')
            .all(|p| !p.trim().is_empty() && p != "." && p != "..")
}
pub fn progress_path_for_state(state: &Path) -> PathBuf {
    state.with_file_name(format!(
        "{}_progress.json",
        state.file_stem().unwrap_or_default().to_string_lossy()
    ))
}
/// Resolve existing components, including symlinks, before appending missing ones.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            Component::Normal(_) => {
                result.push(component.as_os_str());
                match fs::symlink_metadata(&result) {
                    Ok(m) if m.is_symlink() => result = result.canonicalize()?,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            _ => result.push(component.as_os_str()),
        }
    }
    Ok(result)
}
pub fn allowed_kgraph_root(state: &Path) -> Result<PathBuf, String> {
    resolve(state)
        .map_err(|e| e.to_string())?
        .ancestors()
        .skip(1)
        .find(|p| p.file_name().is_some_and(|n| n == ".kgraph"))
        .map(Path::to_owned)
        .ok_or_else(|| {
            "Interactive responses require the state file to be inside a .kgraph directory".into()
        })
}
pub fn reject_path_traversal(path: &Path) -> Result<(), String> {
    let raw = path.to_string_lossy();
    let parts = if raw.as_bytes().get(1) == Some(&b':') && raw.as_bytes()[0].is_ascii_alphabetic() {
        &raw[2..]
    } else {
        &raw
    };
    if parts.split(['\\', '/']).any(|p| p == "..") {
        Err("Gate paths cannot contain parent traversal".into())
    } else {
        Ok(())
    }
}
pub fn ensure_inside(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let path = resolve(path).map_err(|e| e.to_string())?;
    if path == root || !path.starts_with(root) {
        Err("Gate paths must stay inside the workflow's .kgraph directory".into())
    } else {
        Ok(path)
    }
}
fn json_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|p| p.to_str())
        .is_some_and(|p| p.eq_ignore_ascii_case("json"))
}
pub fn validate_kgraph_output_path(
    raw: &str,
    state: &Path,
    packet: Option<&Path>,
    project: Option<&Path>,
) -> Result<PathBuf, String> {
    if raw.trim().is_empty() || raw.contains('\0') {
        return Err("The packet must declare a JSON output path".into());
    }
    let output = Path::new(raw);
    if !json_ext(output) {
        return Err("The gate output must be a .json file".into());
    }
    reject_path_traversal(output)?;
    let root = allowed_kgraph_root(state)?;
    let mut base = project.or_else(|| state.parent()).unwrap_or(Path::new("."));
    let mut protected = vec![resolve(state).map_err(|e| e.to_string())?];
    if let Some(packet) = packet {
        reject_path_traversal(packet)?;
        protected.push(ensure_inside(packet, &root)?);
        base = packet.parent().unwrap_or(base);
    }
    let declared = std::path::absolute(base.join(output)).map_err(|e| e.to_string())?;
    let target = ensure_inside(&declared, &root)?;
    // Allow canonical aliases above the workflow (such as /var on macOS),
    // but retain and check every declared component within .kgraph.
    let lexical_root = declared
        .ancestors()
        .find(|p| {
            p.file_name().is_some_and(|n| n == ".kgraph") && resolve(p).ok().as_ref() == Some(&root)
        })
        .ok_or("Expected an output inside .kgraph")?;
    if lexical_root.is_symlink()
        || root.join(declared.strip_prefix(lexical_root).unwrap()) != target
    {
        return Err("The gate output cannot contain symlinks".into());
    }
    if !json_ext(&target) || protected.contains(&target) || (target.exists() && !target.is_file()) {
        return Err("The gate requires a separate JSON response file".into());
    }
    Ok(target)
}

fn progress_items(entry: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let mut insert = |id: &str, item: &Value| {
        if valid_id(id) && item.is_object() {
            let mut item = item.clone();
            item["id"] = json!(id);
            out.insert(id.to_owned(), item);
        }
    };
    for (id, item) in pairs(&entry["items"]) {
        insert(id, item);
    }
    for item in array(&entry["items"]) {
        insert(s(&item["id"]), item);
    }
    out
}
fn read_progress_file(path: &Path) -> io::Result<Value> {
    let mut data = optional_json(path)?;
    if let Some(m) = data.as_object_mut() {
        m.retain(|k, v| {
            if valid_node_path(k) && v.is_object() {
                v["items"] = Value::Object(progress_items(v));
                true
            } else {
                false
            }
        });
    }
    Ok(data)
}
pub fn read_node_progress(state: &Path, warnings: &mut Vec<String>) -> Value {
    let path = progress_path_for_state(state);
    read_progress_file(&path).unwrap_or_else(|e| {
        warnings.push(format!("Could not read {}: {e}", path.display()));
        json!({})
    })
}
fn valid_text(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|x| !x.trim().is_empty() && !x.contains('\0'))
}
fn valid_progress(v: &Value) -> bool {
    v.is_null() || v.as_f64().is_some_and(|n| (0.0..=100.0).contains(&n))
}
fn validate_questions(v: &Value) -> Result<(), String> {
    for k in ["id", "question", "recommended", "ticket"] {
        if !v[k].is_null() && !valid_text(&v[k]) {
            return Err(format!("{k} must be nonempty text"));
        }
    }
    for k in ["multi_select", "allow_note"] {
        if v.get(k).is_some_and(|x| !x.is_boolean()) {
            return Err(format!("{k} must be a boolean"));
        }
    }
    if !v["options"].is_null()
        && !v["options"]
            .as_array()
            .is_some_and(|a| a.iter().all(valid_text))
    {
        return Err("options must be a list of nonempty strings".into());
    }
    if !v["questions"].is_null() {
        if !v["question"].is_null() {
            return Err("Provide either question or questions".into());
        }
        let list = v["questions"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("questions must be a nonempty list of question objects")?;
        for q in list {
            if !q.is_object() || q["question"].is_null() {
                return Err("Each question must be an object with question text".into());
            }
            validate_questions(&question_fields(q))?;
        }
    }
    Ok(())
}
fn question_fields(q: &Value) -> Value {
    Value::Object(
        pairs(q)
            .filter(|(k, _)| {
                k.as_str() == "id"
                    || (QUESTION_FIELDS.contains(&k.as_str()) && k.as_str() != "questions")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}
pub fn update_node_progress(
    state: &Path,
    node: &str,
    item_id: Option<&str>,
    fields: &Value,
    clear: bool,
) -> Result<Value, String> {
    if !valid_node_path(node) {
        return Err("Invalid node path".into());
    }
    if (item_id.is_some() || !clear) && !item_id.is_some_and(valid_id) {
        return Err("A safe item ID is required".into());
    }
    let map = fields
        .as_object()
        .ok_or("Progress fields must be an object")?;
    for (k, allowed) in [("status", PROGRESS_STATUSES), ("kind", TASK_KINDS)] {
        if map.get(k).is_some_and(|v| !allowed.contains(&s(v))) {
            return Err(format!("Invalid sub-task {k}"));
        }
    }
    if !valid_progress(&fields["progress"]) {
        return Err("Progress must be a number from 0 to 100".into());
    }
    for k in ["title", "detail", "agent"] {
        if !fields[k].is_null() && !fields[k].is_string() {
            return Err(format!("{k} must be text"));
        }
    }
    for k in ["deps", "outputs"] {
        if let Some(v) = map.get(k) {
            let a = v
                .as_array()
                .ok_or_else(|| format!("{k} must be a sequence"))?;
            if !a
                .iter()
                .all(|v| v.as_str().is_some_and(|id| k == "outputs" || valid_id(id)))
            {
                return Err(format!("Invalid {k} entry"));
            }
        }
    }
    validate_questions(fields)?;
    let path = progress_path_for_state(state);
    let _guard = file_lock(&path.with_extension("lock"))?;
    let mut data = read_progress_file(&path).map_err(|e| e.to_string())?;
    if clear {
        if let Some(id) = item_id {
            if let Some(items) = data
                .get_mut(node)
                .and_then(|e| e.get_mut("items"))
                .and_then(Value::as_object_mut)
            {
                items.remove(id);
            }
        } else {
            data.as_object_mut().unwrap().remove(node);
        }
    } else {
        let id = item_id.unwrap();
        if data[node]["items"].get(id).is_none() {
            let order = pairs(&data[node]["items"])
                .map(|(_, i)| i["order"].as_u64().unwrap_or(0).saturating_add(1))
                .max()
                .unwrap_or(0)
                .max(data[node]["next_order"].as_u64().unwrap_or(0));
            data[node]["next_order"] =
                json!(order.checked_add(1).ok_or("Progress order overflow")?);
            data[node]["items"][id] = json!({
                "id": id,
                "order": order,
                "status": "pending",
                "kind": "code",
                "title": id,
                "detail": null,
                "deps": [],
                "agent": null,
                "progress": null,
                "outputs": [],
            });
        }
        let item = &mut data[node]["items"][id];
        let previous = item.clone();
        let m = item.as_object_mut().unwrap();
        if !fields["question"].is_null() {
            m.remove("questions");
        }
        if !fields["questions"].is_null() {
            for k in QUESTION_FIELDS {
                m.remove(*k);
            }
        }
        for (k, v) in map {
            if QUESTION_FIELDS.contains(&k.as_str())
                || [
                    "status", "title", "kind", "detail", "deps", "agent", "progress", "outputs",
                ]
                .contains(&k.as_str())
            {
                let v = if k == "questions" && v.is_array() {
                    json!(array(v).iter().map(question_fields).collect::<Vec<_>>())
                } else {
                    v.clone()
                };
                m.insert(k.clone(), v);
            }
        }
        if previous["submitted"].is_object() {
            if subtask_questions(&previous) != subtask_questions(item) {
                for k in ["submitted", "decision", "decision_summary"] {
                    item.as_object_mut().unwrap().remove(k);
                }
            } else if fields["status"] == "awaiting_subagents" && previous["status"] == "passed" {
                for k in ["status", "progress", "detail"] {
                    item[k] = previous[k].clone();
                }
            }
        }
        item["updated_at"] = json!(now());
    }
    write_json(&path, &data, true)?;
    Ok(data)
}
pub fn ask_node_question(
    state: &Path,
    node: &str,
    item_id: &str,
    fields: &Value,
) -> Result<Value, String> {
    if fields["question"].is_null() == fields["questions"].is_null() {
        return Err("Provide either question or questions".into());
    }
    validate_questions(fields)?;
    let title = nonempty(&fields["title"])
        .or_else(|| nonempty(&fields["question"]))
        .unwrap_or(item_id);
    let update = json!({
        "status": "awaiting_subagents",
        "kind": "gate",
        "agent": fields.get("agent").unwrap_or(&json!("Reviewer")),
        "title": title,
        "detail": nonempty(&fields["detail"]).or_else(||nonempty(&fields["question"])),
        "deps": fields.get("deps").unwrap_or(&json!([])),
        "progress": null,
        "questions": questions(fields),
    });
    update_node_progress(state, node, Some(item_id), &update, false)
}
fn questions(packet: &Value) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut raw = array(&packet["questions"]);
    if raw.is_empty() && packet["question"].is_string() {
        raw = std::slice::from_ref(packet);
    }
    let mut result = Vec::new();
    for (index, q) in raw.iter().enumerate() {
        let Some(question) = text(if q.is_string() { q } else { &q["question"] }) else {
            continue;
        };
        let default_id = format!("q{}", index + 1);
        let mut id = text(&q["id"]).unwrap_or_else(|| default_id.clone());
        if seen.contains(&id) {
            id = default_id;
            while seen.contains(&id) {
                id.push('_');
            }
        }
        seen.insert(id.clone());
        result.push(json!({
            "id": id,
            "ticket": text(&q["ticket"]).or_else(||text(&q["ticket_id"])),
            "question": question,
            "options": unique(array(&q["options"]).iter().filter_map(text)),
            "recommended": text(&q["recommended"]),
            "multi_select": q["multi_select"]==true,
            "allow_note": q["allow_note"]!=false,
        }));
    }
    result
}
fn subtask_questions(item: &Value) -> Vec<Value> {
    if !array(&item["questions"]).is_empty() || nonempty(&item["question"]).is_some() {
        return questions(item);
    }
    if item["kind"] == "gate"
        || ["reviewer", "user", "human", "approver", "owner", "lead"]
            .contains(&s(&item["agent"]).trim().to_lowercase().as_str())
    {
        return questions(&json!({
            "id": item["id"],
            "ticket": item["ticket"],
            "question": text(&item["detail"]).or_else(||text(&item["title"])).unwrap_or_else(||s(&item["id"]).to_owned()),
        }));
    }
    vec![]
}
fn subtask_submission(doc: &Value) -> Value {
    let decisions: Map<_, _> = pairs(&doc["decisions"])
        .filter_map(|(k, v)| {
            text(&json!(k)).map(|k| {
                (
                    k,
                    if v.is_array() {
                        json!(array(v).iter().map(text).collect::<Vec<_>>())
                    } else {
                        json!(text(v))
                    },
                )
            })
        })
        .collect();
    json!({
        "answered_at": text(&doc["answered_at"]),
        "approved_by": text(&doc["approved_by"]),
        "summary": text(&doc["summary"]),
        "decisions": decisions,
        "notes": text(&doc["notes"]).unwrap_or_default(),
    })
}
fn subtask_path(v: &str) -> Option<(&str, &str)> {
    v.split_once("::")
        .filter(|(p, id)| valid_node_path(p) && valid_id(id))
}
fn subtask_decision_path(state: &Path, id: &str) -> Result<PathBuf, String> {
    let path = resolve(state)
        .map_err(|e| e.to_string())?
        .with_file_name("artifacts")
        .join(format!("{id}_decision.json"));
    if path.is_symlink() {
        return Err("A sub-task decision file cannot be a symlink".into());
    }
    validate_kgraph_output_path(&path.to_string_lossy(), state, None, None)
}

// Python json.dumps(..., ensure_ascii=True, sort_keys=True, separators=(",",":")).
fn identity_hash(v: &Value) -> String {
    fn encode(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                let mut keys = m.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    out.push_str(&ascii_json(&serde_json::to_string(k).unwrap()));
                    out.push(':');
                    encode(&m[k], out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    encode(v, out);
                }
                out.push(']');
            }
            Value::Number(n) if n.is_f64() => {
                out.push_str(&crate::ui::python_json(n).expect("finite JSON number"))
            }
            _ => out.push_str(&ascii_json(&v.to_string())),
        }
    }
    let mut out = String::new();
    encode(v, &mut out);
    sha256_bytes(out.as_bytes())
}
fn ascii_json(raw: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for c in raw.chars() {
        if c <= '\u{7e}' {
            out.push(c);
        } else {
            for u in c.encode_utf16(&mut [0; 2]) {
                write!(&mut out, "\\u{u:04x}").unwrap();
            }
        }
    }
    out
}
pub fn extract_subtask_input_gate(
    node: &str,
    item: &Value,
    state: Option<&Path>,
    allow_respond: bool,
) -> Option<Value> {
    let id = s(&item["id"]);
    if item["status"] != "awaiting_subagents" || !valid_node_path(node) || !valid_id(id) {
        return None;
    }
    let q = subtask_questions(item);
    if q.is_empty() {
        return None;
    }
    let path = format!("{node}::{id}");
    let mut gate = json!({
        "gate_type": "questions",
        "node_path": path,
        "parent_node_path": node,
        "subtask_item_id": id,
        "questions": q,
        "packet_hash": identity_hash(&json!([path,q])),
        "can_respond": allow_respond && state.is_none_or(|p|subtask_decision_path(p,id).is_ok()),
    });
    if item["submitted"].as_object().is_some_and(|m| !m.is_empty()) {
        gate["submitted"] = subtask_submission(&item["submitted"]);
    }
    Some(gate)
}
pub(crate) fn wave_edges(
    nodes: &mut [Value],
    warnings: &mut Vec<String>,
    parent: &str,
) -> Vec<Value> {
    let by_id: HashMap<_, _> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (s(&n["id"]), i))
        .collect();
    let mut indegree = vec![0; nodes.len()];
    let mut outgoing = vec![Vec::new(); nodes.len()];
    let mut edges = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        for dep in unique(array(&node["deps"]).iter().filter_map(Value::as_str)) {
            if let Some(&j) = by_id.get(dep) {
                outgoing[j].push(i);
                indegree[i] += 1;
                edges.push(json!({"from":nodes[j]["path"],"to":node["path"]}));
            } else {
                warnings.push(format!(
                    "{} depends on unknown sibling {dep}",
                    s(&node["path"])
                ));
            }
        }
    }
    let mut ready: VecDeque<_> = (0..nodes.len()).filter(|&i| indegree[i] == 0).collect();
    let mut waves = vec![0; nodes.len()];
    let mut visited = 0;
    while let Some(j) = ready.pop_front() {
        visited += 1;
        for &i in &outgoing[j] {
            waves[i] = waves[i].max(waves[j] + 1);
            indegree[i] -= 1;
            if indegree[i] == 0 {
                ready.push_back(i);
            }
        }
    }
    if visited < nodes.len() {
        warnings.push(format!(
            "Dependency cycle in {}",
            if parent.is_empty() {
                "the root graph"
            } else {
                parent
            }
        ));
    } else {
        for (node, wave) in nodes.iter_mut().zip(waves) {
            node["wave"] = json!(wave);
        }
    }
    edges
}
pub fn build_node_sub_graph(
    node: &str,
    entry: &Value,
    warnings: &mut Vec<String>,
    state: Option<&Path>,
    allow_respond: bool,
) -> (Option<Value>, Option<Value>) {
    let mut items: Vec<_> = progress_items(entry).into_iter().collect();
    if items.is_empty() {
        return (None, None);
    }
    items.sort_by(|(a, x), (b, y)| {
        (x["order"].as_u64().unwrap_or(0), a).cmp(&(y["order"].as_u64().unwrap_or(0), b))
    });
    let mut counts: HashMap<&str, usize> = PROGRESS_STATUSES.iter().map(|&k| (k, 0)).collect();
    let mut nodes = Vec::new();
    for (id, item) in items {
        let status = PROGRESS_STATUSES
            .iter()
            .copied()
            .find(|k| item["status"] == *k)
            .unwrap_or("pending");
        *counts.get_mut(status).unwrap() += 1;
        let mut n = json!({
            "id": id,
            "title": text(&item["title"]).unwrap_or_else(||id.clone()),
            "path": format!("{node}::{id}"),
            "node_type": "task",
            "kind": TASK_KINDS.iter().find(|k|item["kind"]==**k).unwrap_or(&"code"),
            "deps": array(&item["deps"]).iter().filter_map(Value::as_str).filter(|id|valid_id(id)).collect::<Vec<_>>(),
            "wave": 0,
            "status": status,
            "attempts": item.get("attempts").unwrap_or(&json!(0)),
            "duration_s": null,
            "detail": text(&item["detail"]),
            "agent": text(&item["agent"]),
            "progress": if valid_progress(&item["progress"]){&item["progress"]}else{&Value::Null},
            "outputs": array(&item["outputs"]).iter().filter_map(text).collect::<Vec<_>>(),
            "output_hashes": {},
            "error_signature": null,
            "started_at": null,
            "updated_at": text(&item["updated_at"]),
            "children": [],
            "edges": [],
        });
        for k in ["ticket", "question", "recommended"] {
            if let Some(v) = text(&item[k]) {
                n[k] = json!(v);
            }
        }
        if item["options"].is_array() {
            n["options"] = json!(array(&item["options"])
                .iter()
                .filter_map(text)
                .collect::<Vec<_>>());
        }
        if !array(&item["questions"]).is_empty() {
            n["questions"] = json!(questions(&item));
        }
        if let Some(g) = extract_subtask_input_gate(node, &item, state, allow_respond) {
            n["input_gate"] = g;
        }
        if item["submitted"].as_object().is_some_and(|m| !m.is_empty()) {
            n["submitted"] = subtask_submission(&item["submitted"]);
        }
        if let Some(v) =
            text(&item["decision_summary"]).or_else(|| text(&n["submitted"]["summary"]))
        {
            n["decision_summary"] = json!(v);
        }
        nodes.push(n);
    }
    let edges = wave_edges(&mut nodes, warnings, node);
    (
        Some(json!({"nodes":nodes,"edges":edges,"counts":counts})),
        Some(json!({
            "total": nodes.len(),
            "done": counts["passed"]+counts["cached"],
            "running": counts["running"],
            "failed": counts["failed"],
            "waiting": counts["awaiting_subagents"],
            "pending": counts["pending"],
        })),
    )
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn declared_output(packet: &Value) -> Option<&str> {
    let v = [&packet["output_path"], &packet["approval_path"]]
        .into_iter()
        .find(|v| truthy(v))
        .or_else(|| array(&packet["outputs"]).first())?;
    v.as_str()
}
pub(crate) fn output_path(packet: &Value) -> &str {
    declared_output(packet).unwrap_or_default()
}
fn approval_fields(packet: &Value) -> Vec<Value> {
    static VALID: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]{0,79}$").unwrap());
    static PRIVATE: Lazy<Regex> =
        Lazy::new(|| Regex::new("(?i)token|password|passwd|secret|key|credential").unwrap());
    const RESERVED: &[&str] = &[
        "approved",
        "approved_by",
        "rejected_by",
        "answered_at",
        "notes",
        "resolved",
        "decisions",
        "summary",
        "review_sha256",
        "rejected_bugs",
        "source",
        "node_path",
        "packet_hash",
        "submitted_at",
    ];
    let fields = match &packet["approved_fields"] {
        Value::Array(a) => a.clone(),
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| {
                let mut f = if v.is_object() {
                    v.clone()
                } else {
                    json!({"default":v,"label":v})
                };
                if f.get("key").is_none() {
                    f["key"] = json!(k);
                }
                f
            })
            .collect(),
        _ => pairs(&packet["approval_example"])
            .filter(|(k, v)| k.starts_with("approve_") && v.is_boolean())
            .map(|(k, v)| json!({"key":k,"default":v}))
            .collect(),
    };
    let mut result: Vec<Value> = Vec::new();
    for f in fields {
        let k = f.as_str().unwrap_or_else(|| s(&f["key"]));
        if !VALID.is_match(k) || PRIVATE.is_match(k) || RESERVED.contains(&k) {
            continue;
        }
        let label = text(&f["label"]).unwrap_or_else(|| {
            let mut label = k.replace('_', " ").to_ascii_lowercase();
            label[..1].make_ascii_uppercase();
            label
        });
        let field = json!({"key":k,"label":label,"default":f["default"]!=false});
        if let Some(existing) = result.iter_mut().find(|v| v["key"] == k) {
            *existing = field;
        } else {
            result.push(field);
        }
    }
    result
}
fn bugs(packet: &Value) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    for bug in array(&packet["bugs"]) {
        if let Some(id) = text(if bug.is_string() { bug } else { &bug["id"] }) {
            let entry = json!({"id":id,"title":text(&bug["title"]).unwrap_or_else(||id.clone())});
            if let Some(old) = result.iter_mut().find(|b| b["id"] == id) {
                *old = entry;
            } else {
                result.push(entry);
            }
        }
    }
    result
}
fn submission(doc: &Value, fallback: Option<String>) -> Value {
    json!({
        "answered_at": text(&doc["answered_at"]).or(fallback),
        "approved_by": text(&doc["approved_by"]).or_else(||text(&doc["rejected_by"])),
        "approved": doc["approved"].as_bool(),
        "summary": text(&doc["summary"]),
    })
}
pub fn extract_input_gate(
    packet: &Value,
    packet_path: Option<&Path>,
    node_path: Option<&str>,
    state: Option<&Path>,
    project: Option<&Path>,
    allow_respond: bool,
) -> Option<Value> {
    if !packet.is_object() {
        return None;
    }
    let q = questions(packet);
    let approval = packet["stage"] == "user_verify_gate"
        || packet["node_id"] == "user_verify_gate"
        || truthy(&packet["approval_path"])
        || packet.get("approved_fields").is_some()
        || packet.get("approval_example").is_some();
    let kind = match (approval, q.is_empty()) {
        (true, true) => "approval",
        (true, false) => "hybrid",
        (false, false) => "questions",
        _ => return None,
    };
    let mut gate = json!({
        "gate_type": kind,
        "node_path": node_path,
        "questions": q,
        "approved_fields": approval_fields(packet),
        "review_sha256": text(&packet["review_sha256"]),
        "review_summary": text(&packet["review_summary"]).or_else(||text(&packet["review_summary_path"])),
        "bugs": bugs(packet),
    });
    let raw = output_path(packet);
    let mut identity = gate.clone();
    identity["target"] = json!(declared_output(packet));
    identity["revision"] = packet["revision"].clone();
    identity["stale_approval"] = packet["stale_approval_sha256"].clone();
    gate["packet_hash"] = json!(identity_hash(&identity));
    let enabled = allow_respond && node_path.is_some_and(valid_node_path);
    gate["can_respond"] = json!(enabled && state.is_none());
    if let Some(state) = state {
        if let Ok(output) = validate_kgraph_output_path(raw, state, packet_path, project) {
            gate["can_respond"] = json!(enabled);
            if let Ok((doc, meta, bytes)) = json_file(&output, Some(PACKET_LIMIT)) {
                let modified = meta.modified().ok();
                let packet_modified = packet_path
                    .map(|p| p.metadata().and_then(|m| m.modified()))
                    .transpose();
                if (q.is_empty() || doc["resolved"] == true)
                    && (!approval || doc["approved"].is_boolean())
                    && (gate["review_sha256"].is_null()
                        || gate["review_sha256"] == doc["review_sha256"])
                    && packet_modified.is_ok_and(|t| modified >= t)
                    && sha256_bytes(&bytes) != s(&packet["stale_approval_sha256"])
                {
                    gate["submitted"] = submission(&doc, modified.map(timestamp));
                }
            }
        }
    }
    Some(gate)
}
pub fn read_subtask_answer(state: &Path, node: &str, id: &str) -> Result<Option<Value>, String> {
    if !valid_node_path(node) || !valid_id(id) {
        return Err("Invalid sub-task path".into());
    }
    let data = read_node_progress(state, &mut vec![]);
    let item = &data[node]["items"][id];
    if !item.is_null() {
        if item["decision"].as_object().is_some_and(|m| !m.is_empty()) {
            return Ok(Some(item["decision"].clone()));
        }
        if item["status"] != "passed" {
            return Ok(None);
        }
    }
    let doc = subtask_decision_path(state, id)
        .ok()
        .and_then(|p| read_json(&p, Some(PACKET_LIMIT)).ok());
    Ok(doc.filter(|d| {
        if d["node_path"] != format!("{node}::{id}") {
            return false;
        }
        if item.is_null() {
            return true;
        }
        let mut waiting = item.clone();
        waiting["status"] = json!("awaiting_subagents");
        extract_subtask_input_gate(node, &waiting, Some(state), true)
            .is_some_and(|g| g["packet_hash"] == d["packet_hash"])
    }))
}
pub(crate) fn report_segments(report: &Value) -> Vec<&str> {
    static SEGMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\[\]]+)\] ").unwrap());
    report["node_path"]
        .as_str()
        .filter(|s| valid_node_path(s))
        .into_iter()
        .chain(
            SEGMENT
                .captures_iter(s(&report["error"]))
                .map(|c| c.get(1).unwrap().as_str()),
        )
        .collect()
}
pub(crate) fn match_report_node<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    segments: &[&str],
) -> Option<&'a str> {
    let paths: HashSet<_> = paths.into_iter().collect();
    for (i, &seg) in segments.iter().enumerate() {
        if let Some(&p) = paths.get(seg) {
            return Some(p);
        }
        let mut matches: Vec<_> = paths
            .iter()
            .copied()
            .filter(|p| p.ends_with(&format!("/{seg}")))
            .collect();
        for prefix in segments[..i].iter().rev() {
            let scoped: Vec<_> = matches
                .iter()
                .copied()
                .filter(|p| p.starts_with(&format!("{prefix}/")))
                .collect();
            if !scoped.is_empty() {
                matches = scoped;
                break;
            }
        }
        if matches.len() == 1 {
            return matches.first().copied();
        }
    }
    None
}
pub fn active_gate<'a>(
    state: &'a Value,
    report: &'a Value,
    requested: Option<&str>,
) -> Option<(String, Option<&'a str>)> {
    let records: Vec<_> = pairs(&state["nodes"])
        .filter(|(p, v)| valid_node_path(p) && v.is_object())
        .collect();
    let containers: HashSet<_> = records
        .iter()
        .filter_map(|(p, _)| p.rsplit_once('/').map(|(parent, _)| parent))
        .collect();
    let tasks: Vec<_> = records
        .iter()
        .filter(|(p, _)| !containers.contains(p.as_str()))
        .collect();
    let waiting: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|(_, v)| v["status"] == "awaiting_subagents")
        .collect();
    let segments = report_segments(report);
    let active = match_report_node(tasks.iter().map(|(p, _)| p.as_str()), &segments)
        .or_else(|| (waiting.len() == 1).then(|| waiting[0].0.as_str()));
    if let Some(target) = requested.or(active) {
        if let Some((_, v)) = waiting.iter().find(|(p, _)| p.as_str() == target) {
            let packet = if Some(target) == active {
                nonempty(&report["packet_path"])
            } else {
                None
            }
            .or_else(|| v["packet_path"].as_str())
            .or_else(|| {
                if waiting.len() == 1 {
                    state["packet_path"].as_str()
                } else {
                    None
                }
            });
            if packet.is_some() || Some(target) == active {
                return Some((target.to_owned(), packet));
            }
        }
    }
    if records.is_empty() && report["status"] == "awaiting_subagents" {
        let path = nonempty(&report["node_path"])
            .or_else(|| segments.iter().rev().copied().find(|p| valid_node_path(p)))?;
        if valid_node_path(path) && requested.is_none_or(|r| r == path) {
            return Some((path.to_owned(), report["packet_path"].as_str()));
        }
    }
    None
}
pub fn resolve_packet(
    raw: &str,
    state: &Path,
    report: &Path,
    project: Option<&Path>,
    watch: &mut dyn FnMut(&Path),
) -> Result<PathBuf, String> {
    if raw.trim().is_empty() || raw.contains('\0') {
        return Err("The active gate has no packet".into());
    }
    let path = Path::new(raw);
    reject_path_traversal(path)?;
    let root = allowed_kgraph_root(state)?;
    let candidates: Vec<_> = if path.is_absolute() {
        vec![path.to_owned()]
    } else {
        [project, root.parent(), report.parent(), state.parent()]
            .into_iter()
            .flatten()
            .map(|b| b.join(path))
            .collect()
    };
    for p in &candidates {
        watch(p);
    }
    let candidate = candidates
        .iter()
        .find(|p| p.is_file())
        .or(candidates.first())
        .ok_or("The active gate packet is unavailable")?;
    if !ensure_inside(candidate, &root)?.is_file() {
        return Err("The active gate packet is unavailable".into());
    }
    Ok(candidate.clone())
}
fn trusted_local(run: &Map<String, Value>) -> bool {
    if run.get("source").and_then(Value::as_str) != Some("remote") {
        return true;
    }
    // Ingest never supplies a local report path. Only deduplication against
    // the origin host's registered run can retain that trusted metadata.
    run.get("report_path").and_then(nonempty).is_some()
        && run
            .get("state_path")
            .and_then(nonempty)
            .is_some_and(|p| Path::new(p).is_file())
        && run
            .get("source_host")
            .and_then(Value::as_str)
            .is_none_or(|h| h.to_lowercase() == crate::registry::hostname())
}
fn local_state(run: &Map<String, Value>) -> Result<(PathBuf, PathBuf), String> {
    let raw = run
        .get("state_path")
        .and_then(nonempty)
        .ok_or("This operation requires a local run")?;
    if !trusted_local(run) {
        return Err("This operation requires a local run".into());
    }
    let state = resolve(Path::new(raw)).map_err(|e| e.to_string())?;
    let root = allowed_kgraph_root(&state)?;
    Ok((state, root))
}
fn resolve_log(raw: &str, root: &Path, bases: &[&Path]) -> Result<PathBuf, String> {
    if raw.trim().is_empty() || raw.contains('\0') {
        return Err("A valid log path is required".into());
    }
    let path = Path::new(raw);
    reject_path_traversal(path)?;
    if path.is_absolute() {
        return ensure_inside(path, root);
    }
    let candidates: Vec<_> = bases.iter().map(|b| b.join(path)).collect();
    ensure_inside(
        candidates
            .iter()
            .find(|p| p.exists() || p.is_symlink())
            .or_else(|| candidates.iter().find(|p| p.starts_with(root)))
            .ok_or("The log must be inside the workflow's .kgraph directory")?,
        root,
    )
}
type Response = (u16, Value);
fn error(code: u16, msg: impl ToString) -> Response {
    (code, json!({"error":msg.to_string()}))
}
fn tail_limit(raw: &str) -> Option<usize> {
    static DECIMAL: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\p{Nd}$").unwrap());
    fn decimal(c: char) -> Option<u32> {
        if c.is_ascii() {
            return c.to_digit(10);
        }
        let is_digit = |c: char| DECIMAL.is_match(c.encode_utf8(&mut [0; 4]));
        if !is_digit(c) {
            return None;
        }
        // Unicode decimal digits occur in consecutive sets of ten.
        let mut first = c as u32;
        while char::from_u32(first - 1).is_some_and(is_digit) {
            first -= 1;
        }
        Some((c as u32 - first) % 10)
    }
    let raw = raw.trim();
    let negative = raw.starts_with('-');
    let digits = raw.strip_prefix(['-', '+']).unwrap_or(raw);
    let mut previous_digit = false;
    let mut amount = 0usize;
    for c in digits.chars() {
        if c == '_' && previous_digit {
            previous_digit = false;
            continue;
        }
        let d = decimal(c)?;
        previous_digit = true;
        amount = amount.saturating_mul(10).saturating_add(d as usize);
    }
    previous_digit.then(|| {
        if negative {
            256
        } else {
            amount.clamp(256, 131072)
        }
    })
}
pub fn read_node_log_tail(
    run: &Map<String, Value>,
    raw: Option<&str>,
    max_bytes: Option<&str>,
) -> Response {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return error(400, "A log path is required");
    };
    let Some(limit) = tail_limit(max_bytes.unwrap_or("32768")) else {
        return error(400, "max_bytes must be an integer");
    };
    let denied = || {
        error(
            403,
            "The log must be a recorded regular file inside the workflow's .kgraph directory",
        )
    };
    let Ok((state, root)) = local_state(run) else {
        return denied();
    };
    let project = resolve(
        run.get("project_dir")
            .and_then(nonempty)
            .map(Path::new)
            .unwrap_or(root.parent().unwrap()),
    );
    let Ok(project) = project else {
        return denied();
    };
    let bases = [
        project.as_path(),
        root.parent().unwrap(),
        state.parent().unwrap(),
        root.as_path(),
    ];
    let Ok(path) = resolve_log(raw, &root, &bases) else {
        return denied();
    };
    let Ok(data) = optional_json(&state) else {
        return denied();
    };
    let recorded = |v: &str| resolve_log(v, &root, &bases).is_ok_and(|p| p == path);
    let in_state = pairs(&data["nodes"]).any(|(_, v)| {
        array(&v["outputs"])
            .iter()
            .filter_map(Value::as_str)
            .any(recorded)
            || pairs(&v["output_hashes"]).any(|(k, _)| recorded(k))
    });
    if !in_state {
        let progress = ensure_inside(&progress_path_for_state(&state), &root)
            .ok()
            .and_then(|p| read_progress_file(&p).ok())
            .unwrap_or_default();
        if !pairs(&progress)
            .flat_map(|(_, e)| pairs(&e["items"]))
            .flat_map(|(_, i)| array(&i["outputs"]))
            .filter_map(Value::as_str)
            .any(recorded)
        {
            return error(403, "The log path is not a recorded output");
        }
    }
    let read = || -> io::Result<(String, u64)> {
        if !fs::metadata(&path)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "The log must be a regular file",
            ));
        }
        let mut file = regular(&path, true)?;
        let size = file.metadata()?.len();
        let start = size.saturating_sub(limit as u64 + 4096);
        let cutoff = (size.saturating_sub(limit as u64) - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(size - start).read_to_end(&mut bytes)?;
        // An incomplete UTF-8 sequence at the boundary belongs to the tail.
        let prefix = &bytes[..cutoff.min(bytes.len())];
        let complete = match std::str::from_utf8(prefix) {
            Ok(_) => prefix.len(),
            Err(_) => {
                let mut i = prefix.len();
                while i > 0 && prefix[i - 1] & 0xc0 == 0x80 {
                    i -= 1;
                }
                if i > 0
                    && std::str::from_utf8(&prefix[i - 1..]).is_err_and(|e| e.error_len().is_none())
                {
                    i - 1
                } else {
                    prefix.len()
                }
            }
        };
        let split = String::from_utf8_lossy(&bytes[..complete]).len();
        Ok((
            redact_tail_with_lookback(&String::from_utf8_lossy(&bytes), split, limit),
            size,
        ))
    };
    match read() {
        Ok((content, size)) => (
            200,
            json!({
                "path": path,
                "name": path.file_name().unwrap_or_default().to_string_lossy(),
                "content": content,
                "size_bytes": size,
                "truncated": size>limit as u64,
            }),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => error(404, "Log file not found"),
        Err(_) => denied(),
    }
}
pub fn find_run_qa_checklist(run: &Map<String, Value>) -> Option<PathBuf> {
    let (state, root) = local_state(run).ok()?;
    let valid = |p: &Path| {
        ensure_inside(p, &root)
            .ok()
            .filter(|r| r == p && p.is_file() && !p.is_symlink())
    };
    if let Some(report) = run.get("report_path").and_then(nonempty).map(Path::new) {
        let doc = optional_json(report).unwrap_or_default();
        if let Some(raw) = nonempty(&doc["qa_checklist_path"])
            .filter(|v| !v.contains('\0') && !v.trim().is_empty())
        {
            let path = Path::new(raw);
            if reject_path_traversal(path).is_ok() {
                let r = resolve(report).ok();
                for base in [
                    root.parent(),
                    r.as_deref().and_then(Path::parent),
                    state.parent(),
                    Some(&root),
                ]
                .into_iter()
                .flatten()
                {
                    if let Some(p) = valid(&base.join(path)) {
                        return Some(p);
                    }
                }
            }
        }
    }
    valid(&qa_metrics::run_qa_checklist_path(&state)).or_else(|| {
        let parent = state.parent()?;
        if parent == root {
            return None;
        }
        valid(&parent.join("review/manual_qa.json"))
            .or_else(|| valid(&parent.join("manual_qa.json")))
    })
}
pub fn read_run_qa(run: &Map<String, Value>, can_edit: bool) -> Response {
    let Some(path) = find_run_qa_checklist(run) else {
        return (200, json!({"available":false}));
    };
    let data = match read_json(&path, None) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (200, json!({"available":false})),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return error(403, e),
        Err(e) => return error(400, e),
    };
    match qa_metrics::compute_qa_scorecard_value(&data) {
        Ok(card) => {
            let Ok((_, root)) = local_state(run) else {
                return error(403, "This operation requires a local run");
            };
            (
                200,
                json!({
                    "available": true,
                    "can_edit": can_edit,
                    "checklist_path": path.strip_prefix(root).ok(),
                    "scorecard": card,
                    "manual_qa_values": qa_metrics::MANUAL_QA_VALUES,
                    "gap_types": qa_metrics::GAP_TYPES,
                }),
            )
        }
        Err(e) => error(400, e),
    }
}
pub fn submit_run_qa(run: &Map<String, Value>, payload: &Value) -> Response {
    if !payload.is_object() {
        return error(400, "Payload must be a JSON object");
    }
    let Some(path) = find_run_qa_checklist(run) else {
        return error(404, "QA checklist not found");
    };
    submit_qa_checklist(&path, payload)
}

fn submit_qa_checklist(path: &Path, payload: &Value) -> Response {
    if !payload["resolved_on_attempt"].is_null()
        && payload["resolved_on_attempt"].as_u64().unwrap_or(0) < 1
    {
        return error(400, "resolved_on_attempt must be a positive integer");
    }
    let lock = path.with_file_name(format!(
        "{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if lock.is_symlink() || (lock.exists() && !lock.is_file()) {
        return error(403, "The QA checklist lock must be a regular file");
    }
    if !payload["tester"].is_null() && !payload["tester"].is_string() {
        return error(400, "tester must be text");
    }
    let input = match serde_json::from_value::<qa_metrics::QaResultInput>(payload.clone()) {
        Ok(v) => v,
        Err(e) => return error(400, e),
    };
    match qa_metrics::record_qa_result(path, &input, payload["tester"].as_str())
        .and_then(|_| qa_metrics::write_qa_scorecard(path, None, None, false, None))
    {
        Ok(card) => (
            200,
            json!({"ok":true,"available":true,"can_edit":true,"scorecard":card}),
        ),
        Err(e) => {
            // kgraph-core's public QA API returns strings, retaining std::io's
            // numeric OS error at the end of filesystem diagnostics.
            let kind = e
                .strip_suffix(')')
                .and_then(|text| text.rsplit_once(" (os error "))
                .and_then(|(_, code)| code.parse().ok())
                .map(|code| io::Error::from_raw_os_error(code).kind());
            let status = match kind {
                Some(io::ErrorKind::NotFound) => 404,
                Some(io::ErrorKind::PermissionDenied) => 403,
                _ => 400,
            };
            error(status, e)
        }
    }
}

fn bounded(v: &Value, label: &str, limit: usize, required: bool) -> Result<String, String> {
    if !v
        .as_str()
        .is_some_and(|v| !v.contains('\0') && v.chars().count() <= limit)
    {
        return Err(format!(
            "{label} must be text of at most {limit} characters"
        ));
    }
    let result = text(v).unwrap_or_default();
    if required && result.is_empty() {
        Err(format!("{label} is required"))
    } else {
        Ok(result)
    }
}
fn decision(gate: &Value, payload: &Value) -> Result<Value, String> {
    let responder = bounded(
        payload.get("approved_by").unwrap_or(&json!("Reviewer")),
        "Responder name",
        80,
        true,
    )?;
    let notes = bounded(
        payload.get("notes").unwrap_or(&json!("")),
        "Notes",
        8000,
        false,
    )?;
    let mut doc =
        json!({"approved_by":responder,"answered_at":now(),"notes":notes,"source":"hero_town_hub"});
    let mut summary = Vec::new();
    if !array(&gate["questions"]).is_empty() {
        let decisions = payload["decisions"]
            .as_object()
            .ok_or("Decisions must be an object with an answer for each question")?;
        for q in array(&gate["questions"]) {
            let id = s(&q["id"]);
            let label = format!("Answer for {id}");
            let v = decisions.get(id).unwrap_or(&Value::Null);
            let choices = array(&q["options"]);
            let multiple = q["multi_select"] == true && !choices.is_empty();
            let raw = if multiple {
                v.as_array()
                    .filter(|a| !a.is_empty())
                    .map(Vec::as_slice)
                    .ok_or_else(|| format!("{label} must contain at least one choice"))?
            } else {
                std::slice::from_ref(v)
            };
            let answers = unique(
                raw.iter()
                    .map(|v| bounded(v, &label, 8000, true))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            if q["allow_note"] == false
                && answers.iter().any(|a| {
                    if choices.is_empty() {
                        nonempty(&q["recommended"]).is_some_and(|r| r != a)
                    } else {
                        !choices.iter().any(|v| v == a)
                    }
                })
            {
                return Err(format!("{label} must use the choices in the packet"));
            }
            doc["resolved"] = json!(true);
            doc["decisions"][id] = if multiple {
                json!(answers)
            } else {
                json!(answers[0])
            };
            summary.push(format!("{id}: {}", answers.join(", ")));
        }
    }
    if matches!(s(&gate["gate_type"]), "approval" | "hybrid") {
        let approved = payload["approved"]
            .as_bool()
            .ok_or("Approval requires an approved boolean")?;
        if payload
            .get("approved_fields")
            .is_some_and(|v| !v.is_object())
        {
            return Err("Approval fields must be an object".into());
        }
        for f in array(&gate["approved_fields"]) {
            let key = s(&f["key"]);
            let value = payload["approved_fields"]
                .get(key)
                .or_else(|| payload.get(key))
                .unwrap_or(&f["default"]);
            if !value.is_boolean() {
                return Err(format!("{} must be a boolean", s(&f["label"])));
            }
            doc[key] = value.clone();
        }
        doc["approved"] = json!(approved);
        if !gate["review_sha256"].is_null() {
            doc["review_sha256"] = gate["review_sha256"].clone();
        }
        if !approved {
            doc["rejected_by"] = json!(responder);
            if let Some(v) = payload.get("rejected_bugs") {
                let Some(a) = v.as_array().filter(|a| {
                    a.iter().all(|k| {
                        k.is_string() && array(&gate["bugs"]).iter().any(|b| b["id"] == *k)
                    })
                }) else {
                    return Err("Rejected bugs must be listed in the review packet".into());
                };
                if !a.is_empty() {
                    doc["rejected_bugs"] = json!(unique(a.iter()));
                }
            }
        }
        summary.insert(
            0,
            if approved {
                "Approved"
            } else {
                "Fixes requested"
            }
            .into(),
        );
    }
    let mut summary = summary.join(" · ");
    if summary.chars().count() > 1200 {
        summary = summary
            .chars()
            .take(1199)
            .chain(std::iter::once('…'))
            .collect();
    }
    doc["summary"] = json!(summary);
    if ascii_json(&serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?).len()
        >= PACKET_LIMIT - 512
    {
        return Err("The decision exceeds the size limit".into());
    }
    Ok(doc)
}
fn check_gate(g: &Value, p: &Value) -> Result<(), Response> {
    if p.get("packet_hash").is_some_and(|v| *v != g["packet_hash"]) {
        return Err(error(
            409,
            "The gate changed. Refresh and review the current packet before submitting",
        ));
    }
    if p.get("submitted_at")
        .is_some_and(|v| *v != g["submitted"]["answered_at"])
    {
        return Err(newer_decision());
    }
    Ok(())
}
fn newer_decision() -> Response {
    error(
        409,
        "A newer decision was submitted. Refresh before editing it",
    )
}
fn success(doc: &Value, node: &str, delivery: &str) -> Response {
    (
        200,
        json!({
            "ok": true,
            "delivery": delivery,
            "node_path": node,
            "answered_at": doc["answered_at"],
            "summary": doc["summary"],
            "decision": doc,
        }),
    )
}
pub(crate) fn safe_run_id(id: &str) -> bool {
    static RESERVED: Lazy<Regex> =
        Lazy::new(|| Regex::new("(?i)^(?:con|prn|aux|nul|(?:com|lpt)[0-9])$").unwrap());
    valid_id(id) && id.len() <= 128 && !RESERVED.is_match(id)
}
fn response_path(id: &str) -> Result<PathBuf, String> {
    if !safe_run_id(id) {
        Err("Invalid run ID".into())
    } else {
        Ok(crate::registry::kgraph_home()
            .join("responses")
            .join(format!("{id}.json")))
    }
}
pub fn read_remote_response(id: &str) -> Option<Value> {
    let _guard = crate::registry::registry_lock().ok()?;
    crate::registry::registered_run(id).ok()??;
    read_queued_decision(id)
}
fn read_queued_decision(id: &str) -> Option<Value> {
    read_json(&response_path(id).ok()?, Some(PACKET_LIMIT)).ok()
}
fn remote_parent<'a>(nodes: &'a [Value], parent: &str) -> Option<&'a Value> {
    for n in nodes.iter().rev() {
        if n["path"] == parent && ACTIVE_SUBTASK_PARENTS.contains(&s(&n["status"])) {
            return Some(n);
        }
        if let Some(v) = remote_parent(array(&n["children"]), parent) {
            return Some(v);
        }
    }
    None
}
fn remote_parent_mut<'a>(nodes: &'a mut Value, parent: &str) -> Option<&'a mut Value> {
    for n in nodes.as_array_mut()?.iter_mut().rev() {
        if n["path"] == parent && ACTIVE_SUBTASK_PARENTS.contains(&s(&n["status"])) {
            return Some(n);
        }
        if let Some(v) = n
            .get_mut("children")
            .and_then(|children| remote_parent_mut(children, parent))
        {
            return Some(v);
        }
    }
    None
}
fn remote_gate<'a>(snapshot: &'a Value, requested: Option<&str>) -> Option<&'a Value> {
    let att = &snapshot["attention"];
    let gate = &att["input_gate"];
    let mut target = requested.or_else(|| gate["node_path"].as_str())?;
    if subtask_path(s(&gate["node_path"])).is_some() && gate["parent_node_path"] == target {
        target = s(&gate["node_path"]);
    }
    let top = gate["node_path"] == target && att["node_path"] == target;
    if top
        && valid_node_path(target)
        && matches!(s(&att["kind"]), "decision" | "waiting")
        && snapshot["status"] == "awaiting_subagents"
    {
        return Some(gate);
    }
    let (parent, _) = subtask_path(target)?;
    if top && att["kind"] == "decision" && ACTIVE_SUBTASK_PARENTS.contains(&s(&snapshot["status"]))
    {
        return Some(gate);
    }
    let n = remote_parent(array(&snapshot["nodes"]), parent)?;
    array(&n["sub_graph"]["nodes"])
        .iter()
        .find(|i| {
            i["path"] == target
                && i["input_gate"]["node_path"] == target
                && (i["status"] == "awaiting_subagents" || i["input_gate"]["submitted"].is_object())
        })
        .map(|i| &i["input_gate"])
}
fn mark_remote(snapshot: &mut Value, gate: &Value, doc: &Value) {
    let path = s(&gate["node_path"]);
    let top = snapshot["attention"]["node_path"] == path;
    let Some((parent, _)) = subtask_path(path) else {
        if top {
            snapshot["attention"]["input_gate"]["submitted"] = submission(doc, None);
        }
        return;
    };
    let submitted = subtask_submission(doc);
    if top {
        snapshot["attention"]["input_gate"]["submitted"] = submitted.clone();
    }
    let Some(n) = remote_parent_mut(&mut snapshot["nodes"], parent) else {
        return;
    };
    let Some(index) = array(&n["sub_graph"]["nodes"])
        .iter()
        .position(|i| i["path"] == path && i["input_gate"]["packet_hash"] == gate["packet_hash"])
    else {
        return;
    };
    if n["sub_graph"]["nodes"][index]["status"] == "awaiting_subagents" {
        fn move_count(v: &mut Value, from: &str, to: &str) {
            if let (Some(a), Some(b)) = (v[from].as_i64(), v[to].as_i64()) {
                v[from] = json!(a.saturating_sub(1).max(0));
                v[to] = json!(b.saturating_add(1));
            }
        }
        move_count(&mut n["sub_progress"], "waiting", "done");
        move_count(
            &mut n["sub_graph"]["counts"],
            "awaiting_subagents",
            "passed",
        );
    }
    let i = &mut n["sub_graph"]["nodes"][index];
    i["input_gate"]["submitted"] = submitted.clone();
    i["submitted"] = submitted;
    i["status"] = json!("passed");
    i["progress"] = json!(100);
    i["detail"] = json!(format!(
        "Answered by {}: {}",
        s(&doc["approved_by"]),
        s(&doc["summary"])
    ));
    i["decision_summary"] = doc["summary"].clone();
    i["updated_at"] = doc["answered_at"].clone();
    if top {
        if let Some(att) = crate::snapshot::subtask_attention(array(&snapshot["nodes"])) {
            snapshot["attention"] = att;
        }
    }
}
pub fn apply_remote_response(snapshot: &Value, id: &str) -> Value {
    apply_decision(snapshot, read_remote_response(id))
}
fn apply_decision(snapshot: &Value, decision: Option<Value>) -> Value {
    let mut snapshot = snapshot.clone();
    if let Some(doc) = decision {
        if let Some(g) = remote_gate(&snapshot, doc["node_path"].as_str()).cloned() {
            if doc["node_path"] == g["node_path"]
                && doc["packet_hash"] == g["packet_hash"]
                && s(&doc["answered_at"]) > s(&g["submitted"]["answered_at"])
            {
                mark_remote(&mut snapshot, &g, &doc);
            }
        }
    }
    snapshot
}
type Delivery = Result<Response, Response>;
fn storage(_: String) -> Response {
    error(500, "The gate response could not be stored")
}
fn artifact_error(e: io::Error) -> Response {
    if matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotADirectory
    ) {
        return error(403, "The gate output must stay inside .kgraph");
    }
    storage(e.to_string())
}
fn inactive() -> Response {
    error(409, "The requested node is no longer awaiting input")
}
fn path_error(e: String) -> Response {
    let code = if matches!(
        e.as_str(),
        "The packet must declare a JSON output path"
            | "The gate output must be a .json file"
            | "The active gate packet is unavailable"
            | "The active gate has no packet"
    ) {
        400
    } else {
        403
    };
    error(code, e)
}
fn submit_subtask(
    state: &Path,
    report: Option<&Path>,
    requested: &str,
    payload: &Value,
) -> Delivery {
    let inactive = || error(409, "The requested sub-task is no longer awaiting input");
    let (node, id) = subtask_path(requested).ok_or_else(inactive)?;
    let root = allowed_kgraph_root(state).map_err(path_error)?;
    let progress = progress_path_for_state(state);
    ensure_inside(&progress, &root).map_err(path_error)?;
    let lock = progress.with_extension("lock");
    ensure_inside(&lock, &root).map_err(path_error)?;
    let _guard = crate::artifact::lock(&root, &lock).map_err(artifact_error)?;
    let telemetry = optional_json(state).map_err(|e| error(400, e))?;
    let mut data = read_progress_file(&progress).map_err(|e| error(400, e))?;
    if !ACTIVE_SUBTASK_PARENTS.contains(&s(&telemetry["nodes"][node]["status"])) {
        return Err(inactive());
    }
    let item = data
        .get_mut(node)
        .and_then(|v| v.get_mut("items"))
        .and_then(|v| v.get_mut(id))
        .ok_or_else(inactive)?;
    if item["status"] != "awaiting_subagents"
        && !(item["status"] == "passed"
            && nonempty(&item["submitted"]["answered_at"]).is_some()
            && item["submitted"]["answered_at"] == payload["submitted_at"])
    {
        return Err(inactive());
    }
    let mut waiting = item.clone();
    waiting["status"] = json!("awaiting_subagents");
    let gate =
        extract_subtask_input_gate(node, &waiting, Some(state), true).ok_or_else(inactive)?;
    check_gate(&gate, payload)?;
    let output = subtask_decision_path(state, id).map_err(path_error)?;
    if report.and_then(|p| resolve(p).ok()).as_deref() == Some(&output)
        || resolve(&progress).ok().as_deref() == Some(&output)
    {
        return Err(error(
            403,
            "The gate requires a separate JSON response file",
        ));
    }
    let mut doc = decision(&gate, payload).map_err(|e| error(400, e))?;
    doc["node_path"] = json!(requested);
    doc["packet_hash"] = gate["packet_hash"].clone();
    if nonempty(&item["question"]).is_none() && array(&item["questions"]).is_empty() {
        item["questions"] = gate["questions"].clone();
    }
    item["submitted"] = subtask_submission(&doc);
    item["decision"] = doc.clone();
    item["decision_summary"] = doc["summary"].clone();
    item["status"] = json!("passed");
    item["progress"] = json!(100);
    item["updated_at"] = doc["answered_at"].clone();
    item["detail"] = json!(format!(
        "Answered by {}: {}",
        s(&doc["approved_by"]),
        s(&doc["summary"])
    ));
    item["outputs"] = json!(unique(
        array(&item["outputs"])
            .iter()
            .cloned()
            .chain(std::iter::once(json!(output)))
    ));
    crate::artifact::write(&root, &output, &doc).map_err(artifact_error)?;
    crate::artifact::write(&root, &progress, &data).map_err(artifact_error)?;
    let (code, mut v) = success(&doc, requested, "local_subtask");
    v["output_path"] = json!(output);
    Ok((code, v))
}
fn submit_remote(run: &Map<String, Value>, payload: &Value) -> Delivery {
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let response = response_path(id).map_err(|e| error(400, e))?;
    let _guard = crate::registry::registry_lock().map_err(storage)?;
    let current = crate::registry::registered_run(id)
        .map_err(storage)?
        .ok_or_else(|| error(404, "Run not found"))?;
    let path = current
        .get("remote_snapshot_path")
        .and_then(Value::as_str)
        .map(Path::new)
        .ok_or_else(inactive)?;
    let mut snapshot = apply_decision(
        &read_json(path, None).map_err(|e| error(400, e))?,
        read_queued_decision(id),
    );
    let mut gate = remote_gate(&snapshot, payload["node_path"].as_str())
        .cloned()
        .ok_or_else(inactive)?;
    if gate["can_respond"] != true {
        return Err(error(403, "This gate cannot receive a response"));
    }
    check_gate(&gate, payload)?;
    let node = s(&gate["node_path"]).to_owned();
    if subtask_path(&node).is_some()
        && nonempty(&gate["submitted"]["answered_at"]).is_some()
        && gate["submitted"]["answered_at"] != payload["submitted_at"]
    {
        return Err(newer_decision());
    }
    let kind = s(&gate["gate_type"]).to_owned();
    gate["questions"] = json!(questions(&gate));
    gate["approved_fields"] = json!(approval_fields(&gate));
    gate["bugs"] = json!(bugs(&gate));
    gate["review_sha256"] = json!(text(&gate["review_sha256"]));
    if !matches!(kind.as_str(), "questions" | "approval" | "hybrid")
        || (kind != "approval" && array(&gate["questions"]).is_empty())
    {
        return Err(error(400, "Active node does not accept interactive input"));
    }
    let doc = decision(&gate, payload).map_err(|e| error(400, e))?;
    let mut queued = doc.clone();
    queued["node_path"] = json!(node);
    queued["packet_hash"] = gate["packet_hash"].clone();
    write_json(&response, &queued, true).map_err(storage)?;
    if snapshot["attention"]["input_gate"]["node_path"] == node
        && snapshot["attention"]["node_path"] == node
    {
        snapshot["attention"]["input_gate"] = gate.clone();
    } else if let Some((parent, _)) = subtask_path(&node) {
        if let Some(n) = remote_parent_mut(&mut snapshot["nodes"], parent) {
            if let Some(item) = n["sub_graph"]["nodes"]
                .as_array_mut()
                .and_then(|items| items.iter_mut().find(|i| i["path"] == node))
            {
                item["input_gate"] = gate.clone();
            }
        }
    }
    mark_remote(&mut snapshot, &gate, &doc);
    write_json(path, &snapshot, false).map_err(storage)?;
    Ok(success(&doc, &node, "remote_queue"))
}
fn deliver(run: &Map<String, Value>, payload: &Value) -> Delivery {
    if !payload.is_object() {
        return Err(error(400, "Payload must be a JSON object"));
    }
    let requested = payload["node_path"].as_str();
    if !payload["node_path"].is_null()
        && !requested.is_some_and(|n| valid_node_path(n) || subtask_path(n).is_some())
    {
        return Err(error(400, "Invalid node path"));
    }
    let raw = run.get("state_path").and_then(nonempty).map(Path::new);
    let report = run.get("report_path").and_then(nonempty).map(Path::new);
    let project = run.get("project_dir").and_then(nonempty).map(Path::new);
    if !trusted_local(run) {
        return submit_remote(run, payload);
    }
    let state = resolve(raw.ok_or_else(inactive)?).map_err(|e| error(403, e))?;
    let root = allowed_kgraph_root(&state).map_err(path_error)?;
    if let Some(target) = requested.filter(|n| subtask_path(n).is_some()) {
        return submit_subtask(&state, report, target, payload);
    }
    let elevated = || {
        let snapshot = crate::snapshot::build_town_snapshot_from_graph(
            None,
            Some(&state),
            report,
            None,
            None,
            None,
            project,
        );
        let g = &snapshot["attention"]["input_gate"];
        let target = g["node_path"].as_str()?;
        (subtask_path(target).is_some() && requested.is_none_or(|p| g["parent_node_path"] == p))
            .then(|| submit_subtask(&state, report, target, payload))
    };
    let Some(report) = report else {
        return elevated().unwrap_or_else(|| Err(inactive()));
    };
    let lock = state.with_file_name(format!(
        "{}_response.lock",
        state.file_stem().unwrap_or_default().to_string_lossy()
    ));
    ensure_inside(&lock, &root).map_err(path_error)?;
    let _guard = crate::artifact::lock(&root, &lock).map_err(artifact_error)?;
    let telemetry = optional_json(&state).map_err(|e| error(400, e))?;
    let overview = optional_json(report).map_err(|e| error(400, e))?;
    let Some((node, packet)) = active_gate(&telemetry, &overview, requested) else {
        return elevated().unwrap_or_else(|| Err(inactive()));
    };
    let unsupported = || error(400, "Active node does not accept interactive input");
    let Some(raw) = packet.filter(|p| !p.is_empty()) else {
        return elevated().unwrap_or_else(|| Err(unsupported()));
    };
    let packet_path = match resolve_packet(raw, &state, report, project, &mut |_| {}) {
        Ok(p) => p,
        Err(e) => {
            let err = path_error(e);
            return if err.0 == 400 {
                elevated().unwrap_or(Err(err))
            } else {
                Err(err)
            };
        }
    };
    let packet = match read_json(&packet_path, Some(PACKET_LIMIT)) {
        Ok(v) => v,
        Err(e) => return elevated().unwrap_or_else(|| Err(error(400, e))),
    };
    let Some(gate) = extract_input_gate(
        &packet,
        Some(&packet_path),
        Some(&node),
        Some(&state),
        project,
        true,
    ) else {
        return elevated().unwrap_or_else(|| Err(unsupported()));
    };
    let output =
        validate_kgraph_output_path(output_path(&packet), &state, Some(&packet_path), project)
            .map_err(path_error)?;
    if [report, &progress_path_for_state(&state)]
        .iter()
        .any(|p| resolve(p).ok().as_deref() == Some(&output))
    {
        return Err(error(
            403,
            "The gate requires a separate JSON response file",
        ));
    }
    check_gate(&gate, payload)?;
    let doc = decision(&gate, payload).map_err(|e| error(400, e))?;
    crate::artifact::write(&root, &output, &doc).map_err(artifact_error)?;
    Ok(success(&doc, &node, "local"))
}
pub fn submit_gate_response(run: &Map<String, Value>, payload: &Value) -> Response {
    deliver(run, payload).unwrap_or_else(|e| e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::{tempdir, TempDir};
    struct Run {
        _temp: TempDir,
        state: PathBuf,
        #[cfg_attr(not(unix), allow(dead_code))]
        report: PathBuf,
        packet: PathBuf,
        entry: Map<String, Value>,
    }
    impl Run {
        fn new() -> Self {
            let temp = tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join(".kgraph/run");
            let state = root.join("state.json");
            let report = root.join("report.json");
            let packet = root.join("packet.json");
            write_json(
                &state,
                &json!({"nodes":{"release/check":{"status":"awaiting_subagents"}}}),
                true,
            )
            .unwrap();
            write_json(&report,&json!({"status":"awaiting_subagents","node_path":"release/check","packet_path":packet}),true).unwrap();
            let entry = json!({"state_path":state,"report_path":report})
                .as_object()
                .unwrap()
                .clone();
            Self {
                _temp: temp,
                state,
                report,
                packet,
                entry,
            }
        }
        fn submit(&self, p: Value) -> Response {
            submit_gate_response(&self.entry, &p)
        }
    }
    #[test]
    fn progress_lock_preserves_sequence_and_rejects_malformed_updates() {
        let r = Run::new();
        let sidecar = progress_path_for_state(&r.state);
        write_json(
            &sidecar,
            &json!({"stage":{"items":[{"id":"first","order":5}]}}),
            true,
        )
        .unwrap();
        thread::scope(|scope| {
            for i in 0..24 {
                let state = &r.state;
                scope.spawn(move || {
                    update_node_progress(
                        state,
                        "stage",
                        Some(&format!("w{i}")),
                        &json!({"kind":"worker"}),
                        false,
                    )
                    .unwrap()
                });
            }
        });
        let d = read_node_progress(&r.state, &mut vec![]);
        let mut orders = pairs(&d["stage"]["items"])
            .map(|(_, v)| v["order"].as_i64().unwrap())
            .collect::<Vec<_>>();
        orders.sort_unstable();
        assert_eq!(orders, (5..30).collect::<Vec<_>>());
        assert_eq!(d["stage"]["next_order"], 30);
        for fields in [
            json!({"progress":true}),
            json!({"status":"unknown"}),
            json!({"questions":[{"question":""}]}),
            json!({"deps":["bad/id"]}),
        ] {
            assert!(update_node_progress(&r.state, "stage", Some("x"), &fields, false).is_err());
        }
        update_node_progress(&r.state, "stage", Some("w1"), &json!({}), true).unwrap();
        let updated =
            update_node_progress(&r.state, "stage", Some("more"), &json!({}), false).unwrap();
        assert_eq!(updated["stage"]["items"]["more"]["order"], 30);
        fs::write(&sidecar, "{").unwrap();
        let mut warnings = Vec::new();
        assert_eq!(read_node_progress(&r.state, &mut warnings), json!({}));
        assert!(warnings[0].contains("state_progress.json"));
        assert!(update_node_progress(&r.state, "stage", Some("x"), &json!({}), false).is_err());
        assert_eq!(fs::read_to_string(sidecar).unwrap(), "{");
    }
    #[test]
    fn waves_and_cycle_diagnostics() {
        let mut warnings = Vec::new();
        let entry = json!({"items":[{"id":"b","order":1,"deps":["a","a","missing"]},{"id":"a","order":0,"detail":"token=secret"}]});
        let (g, p) = build_node_sub_graph("p", &entry, &mut warnings, None, true);
        let g = g.unwrap();
        assert_eq!(g["nodes"][1]["wave"], 1);
        assert_eq!(g["edges"], json!([{"from":"p::a","to":"p::b"}]));
        assert_eq!(g["nodes"][0]["detail"], "[redacted]");
        assert_eq!(p.unwrap()["pending"], 2);
        assert_eq!(warnings, ["p::b depends on unknown sibling missing"]);
        let mut cyclic = g["nodes"].as_array().unwrap().clone();
        cyclic[0]["deps"] = json!(["b"]);
        wave_edges(&mut cyclic, &mut warnings, "p");
        assert_eq!(warnings.last().unwrap(), "Dependency cycle in p");
    }
    #[test]
    fn approval_dictionary_preserves_packet_order_in_public_hash() {
        let packet:Value=serde_json::from_str(r#"{"stage":"user_verify_gate","approved_fields":{"z_last":true,"a_first":false},"output_path":"answer.json"}"#).unwrap();
        let gate = extract_input_gate(&packet, None, Some("work"), None, None, true).unwrap();
        assert_eq!(gate["approved_fields"][0]["key"], "z_last");
        assert_eq!(gate["approved_fields"][1]["key"], "a_first");
        assert_eq!(
            gate["packet_hash"],
            "6bde9757b8f35274566f2941f7e285dd3c42a9c071ef775b8710c5b2371a011d"
        );
    }
    #[test]
    fn outer_gate_matches_python_hash_and_rechecks_answer_and_revision() {
        let r = Run::new();
        let packet = json!({
            "question": "Ship 🚀?",
            "id": "q",
            "options": ["A","B"],
            "output_path": "answer.json",
            "stage": "user_verify_gate",
            "approved_fields": ["approve_release","private_key","source"],
            "bugs": [{"id":"B","title":"Fix"}],
        });
        write_json(&r.packet, &packet, true).unwrap();
        let extract = |p: &Value| {
            extract_input_gate(
                p,
                Some(&r.packet),
                Some("release/check"),
                Some(&r.state),
                None,
                true,
            )
            .unwrap()
        };
        let gate = extract(&packet);
        assert_eq!(
            gate["packet_hash"],
            "b4968cd30ab78f7339ac97c06607bc56323320296613bb5440b652a368079b8a"
        );
        assert_eq!(
            gate["approved_fields"],
            json!([{"key":"approve_release","label":"Approve release","default":true}])
        );
        let mut payload = json!({
            "approved": false,
            "rejected_bugs": ["B","B"],
            "packet_hash": gate["packet_hash"],
            "submitted_at": null,
            "decisions": {"q":"A"},
            "notes": "Bearer abc",
            "output_path": "private.json",
        });
        let (code, result) = r.submit(payload.clone());
        assert_eq!(code, 200, "{result}");
        assert_eq!(result["delivery"], "local");
        let output = r.packet.with_file_name("answer.json");
        assert_eq!(read_json(&output, None).unwrap(), result["decision"]);
        assert_eq!(result["decision"]["notes"], "[redacted]");
        assert_eq!(result["decision"]["rejected_bugs"], json!(["B"]));
        assert_eq!(
            extract(&packet)["submitted"]["answered_at"],
            result["answered_at"]
        );
        assert_eq!(r.submit(payload.clone()).0, 409);
        payload["submitted_at"] = result["answered_at"].clone();
        assert_eq!(r.submit(payload.clone()).0, 200);
        let mut revised = packet.clone();
        revised["revision"] = json!(2);
        write_json(&r.packet, &revised, true).unwrap();
        let future =
            output.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(2);
        File::open(&r.packet).unwrap().set_modified(future).unwrap();
        assert!(extract(&revised).get("submitted").is_none());
        assert_eq!(r.submit(payload.clone()).0, 409);
        for raw in [
            "../escape.json",
            "..\\escape.json",
            "state.json",
            "report.json",
            "packet.json",
            "state_progress.json",
        ] {
            let mut p = packet.clone();
            p["output_path"] = json!(raw);
            write_json(&r.packet, &p, true).unwrap();
            assert_eq!(
                r.submit(json!({"approved":true,"decisions":{"q":"A"}})).0,
                403,
                "{raw}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn validated_gate_output_rejects_substituted_parent_and_target() {
        use std::os::unix::fs::symlink;
        let run = Run::new();
        let root = allowed_kgraph_root(&run.state).unwrap();
        let outside = run._temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let validate = || {
            validate_kgraph_output_path("answers/gate.json", &run.state, Some(&run.packet), None)
        };
        let output = validate().unwrap();
        let parent = output.parent().unwrap();
        symlink(&outside, parent).unwrap();
        assert!(validate().is_err());
        assert!(crate::artifact::write(&root, &output, &json!({})).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(parent).unwrap();
        fs::create_dir(parent).unwrap();
        let sentinel = outside.join("sentinel.json");
        fs::write(&sentinel, "original").unwrap();
        symlink(&sentinel, &output).unwrap();
        assert!(validate().is_err());
        assert!(crate::artifact::write(&root, &output, &json!({})).is_err());
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "original");
        assert!(output.is_symlink());
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn checklist_disappearing_after_discovery_returns_not_found() {
        let run = Run::new();
        let path = qa_metrics::run_qa_checklist_path(&run.state);
        write_json(&path, &json!({}), true).unwrap();
        let discovered = find_run_qa_checklist(&run.entry).unwrap();
        fs::remove_file(&discovered).unwrap();
        assert_eq!(
            submit_qa_checklist(&discovered, &json!({"case_id":"check","manual_qa":"pass"})).0,
            404
        );
    }

    #[cfg(unix)]
    #[test]
    fn qa_submission_with_unreadable_checklist_returns_forbidden() {
        use std::os::unix::fs::PermissionsExt;

        let run = Run::new();
        let path = qa_metrics::run_qa_checklist_path(&run.state);
        write_json(&path, &json!({}), true).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Privileged test runners can still open mode-000 files.
        if let Err(e) = File::open(&path) {
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                submit_run_qa(&run.entry, &json!({"case_id":"check","manual_qa":"pass"})).0,
                403
            );
        }
    }

    #[test]
    fn elevated_subtask_carries_answers_and_discards_obsolete_decisions() {
        let r = Run::new();
        let node = "release/check";
        let id = "input";
        write_json(&r.packet, &json!({"title":"Check release"}), true).unwrap();
        let fields = json!({"question":"Bêta?"});
        ask_node_question(&r.state, node, id, &fields).unwrap();
        let data = read_node_progress(&r.state, &mut vec![]);
        let item = &data[node]["items"][id];
        let gate = extract_subtask_input_gate(node, item, Some(&r.state), true).unwrap();
        let short = extract_subtask_input_gate(
            node,
            &json!({"id":"input","question":"Bêta?","status":"awaiting_subagents"}),
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            short["packet_hash"],
            "41f8e645c754bcdeb24999600edc185e5de902faf7b53e8a5df143fe4d5a1464"
        );
        let payload =
            json!({"node_path":node,"packet_hash":gate["packet_hash"],"decisions":{"q1":"Yes"}});
        let (code, result) = r.submit(payload);
        assert_eq!(code, 200, "{result}");
        assert_eq!(result["delivery"], "local_subtask");
        assert_eq!(
            read_subtask_answer(&r.state, node, id).unwrap(),
            Some(result["decision"].clone())
        );
        let data = ask_node_question(&r.state, node, id, &fields).unwrap();
        assert_eq!(data[node]["items"][id]["status"], "passed");
        let payload = json!({"node_path":gate["node_path"],"packet_hash":gate["packet_hash"],"decisions":{"q1":"Maybe"}});
        assert_eq!(r.submit(payload.clone()).0, 409);
        let mut edit = payload.clone();
        edit["submitted_at"] = result["answered_at"].clone();
        assert_eq!(r.submit(edit).0, 200);
        let refreshed =
            ask_node_question(&r.state, node, id, &json!({"question":"Changed?"})).unwrap();
        assert!(refreshed[node]["items"][id].get("submitted").is_none());
        assert!(read_subtask_answer(&r.state, node, id).unwrap().is_none());
        assert_eq!(r.submit(payload).0, 409);
        #[cfg(unix)]
        {
            let out = r
                .state
                .with_file_name("artifacts")
                .join("input_decision.json");
            fs::remove_file(&out).unwrap();
            std::os::unix::fs::symlink(&r.report, &out).unwrap();
            assert_eq!(
                r.submit(json!({"node_path":gate["node_path"],"decisions":{"q1":"Yes"}}))
                    .0,
                403
            );
        }
    }
    #[test]
    fn tail_limits_follow_python_decimal_parsing_and_clamping() {
        assert_eq!(tail_limit("  +1_024 "), Some(1024));
        assert_eq!(tail_limit("١٠٢٤"), Some(1024));
        assert_eq!(tail_limit("-1"), Some(256));
        assert_eq!(tail_limit(&"9".repeat(50)), Some(131072));
        for bad in ["", "+", "1_", "_2", "1__2", "1.0"] {
            assert_eq!(tail_limit(bad), None);
        }
    }
    #[test]
    fn log_tail_requires_local_recorded_regular_files_and_redacts_before_cutoff() {
        let r = Run::new();
        let log = r.state.with_file_name("worker.log");
        let tmp = r._temp.path().canonicalize().unwrap();
        let outside = tmp.join("outside.log");
        let hidden = r.state.with_file_name("hidden.log");
        let link = r.state.with_file_name("escape.log");
        let missing = r.state.with_file_name("missing.log");
        let ending = format!("\n{}\n", "x".repeat(200));
        fs::write(
            &log,
            format!(
                "{}password={}{}",
                "a\n".repeat(3000),
                "test-password-".repeat(8),
                ending
            ),
        )
        .unwrap();
        fs::write(&outside, "private").unwrap();
        fs::write(&hidden, "private").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        write_json(&r.state,&json!({"nodes":{"work":{"outputs":[missing,link,outside],"output_hashes":{log.to_str().unwrap():"hash"}}}}),true).unwrap();
        let (code, tail) = read_node_log_tail(&r.entry, log.to_str(), Some("256"));
        assert_eq!(code, 200, "{tail}");
        assert_eq!(tail["content"], format!("[redacted]{ending}"));
        assert_eq!(tail["truncated"], true);
        for raw in [
            hidden.to_str().unwrap(),
            outside.to_str().unwrap(),
            "../state.json",
            "..\\state.json",
            "nul\0",
        ] {
            assert_eq!(read_node_log_tail(&r.entry, Some(raw), None).0, 403);
        }
        #[cfg(unix)]
        assert_eq!(read_node_log_tail(&r.entry, link.to_str(), None).0, 403);
        assert_eq!(read_node_log_tail(&r.entry, missing.to_str(), None).0, 404);
        assert_eq!(
            read_node_log_tail(&r.entry, log.to_str(), Some("bad")).0,
            400
        );
        let mut remote = r.entry.clone();
        remote.insert("source".into(), json!("remote"));
        remote.remove("report_path");
        assert_eq!(read_node_log_tail(&remote, log.to_str(), None).0, 403);
        update_node_progress(
            &r.state,
            "work",
            Some("extra"),
            &json!({"outputs":["hidden.log"]}),
            false,
        )
        .unwrap();
        assert_eq!(
            read_node_log_tail(&r.entry, Some("hidden.log"), None).0,
            200
        );
    }
    #[test]
    fn qa_checklists_are_run_scoped_and_declared_paths_cannot_alias_files() {
        let r = Run::new();
        let root = allowed_kgraph_root(&r.state).unwrap();
        let a = root.join("a_state.json");
        let b = root.join("b_state.json");
        let report = root.join("a_report.json");
        let preferred = qa_metrics::run_qa_checklist_path(&a);
        let shared = root.join("manual_qa.json");
        let declared = root.join("review/manual_qa.json");
        let a_run = json!({"state_path":a,"report_path":report});
        let b_run = json!({"state_path":b});
        let a_run = a_run.as_object().unwrap();
        let b_run = b_run.as_object().unwrap();
        for p in [&a, &b, &preferred, &shared, &declared] {
            write_json(p, &json!({}), true).unwrap();
        }
        assert_eq!(find_run_qa_checklist(a_run), Some(preferred.clone()));
        assert_eq!(find_run_qa_checklist(b_run), None);
        write_json(
            &report,
            &json!({"qa_checklist_path":".kgraph/review/manual_qa.json"}),
            true,
        )
        .unwrap();
        assert_eq!(find_run_qa_checklist(a_run), Some(declared.clone()));
        #[cfg(unix)]
        {
            let alias = root.join("alias.json");
            std::os::unix::fs::symlink(&declared, &alias).unwrap();
            write_json(&report, &json!({"qa_checklist_path":alias}), true).unwrap();
            assert_eq!(find_run_qa_checklist(a_run), Some(preferred.clone()));
            std::os::unix::fs::symlink(
                preferred.parent().unwrap(),
                qa_metrics::run_qa_checklist_path(&b).parent().unwrap(),
            )
            .unwrap();
            assert!(find_run_qa_checklist(b_run).is_none());
        }
        assert_eq!(read_run_qa(b_run, true), (200, json!({"available":false})));
        assert_eq!(submit_run_qa(b_run, &json!({})).0, 404);
        write_json(&report, &json!({}), true).unwrap();
        write_json(
            &preferred,
            &json!({
                "version": 1,
                "batch_id": "release",
                "workflow_type": "generic",
                "tester": "Daniel",
                "cases": [{"id":"check","automated_status":"pass"}],
            }),
            true,
        )
        .unwrap();
        let (code, card) = read_run_qa(a_run, true);
        assert_eq!(code, 200, "{card}");
        assert_eq!(card["available"], true);
        assert_eq!(card["checklist_path"], "a_state_qa/manual_qa.json");
        let (code, result) = submit_run_qa(
            a_run,
            &json!({"case_id":"check","manual_qa":"pass","tester":"Reviewer"}),
        );
        assert_eq!(code, 200, "{result}");
        assert_eq!(result["scorecard"]["tester"], "Reviewer");
        assert_eq!(
            read_json(&preferred.with_file_name("qa_scorecard.json"), None).unwrap(),
            result["scorecard"]
        );
    }
}
