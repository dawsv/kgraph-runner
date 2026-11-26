//! Workflow topology and live telemetry for Hero Town.
use crate::{
    interactions::{
        self as live, array, pairs, s, ACTIVE_SUBTASK_PARENTS, PACKET_LIMIT, TASK_KINDS,
    },
    redaction::{has_credentials, redact_credentials},
};
use axum::body::Bytes;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use kgraph_core::types::{Context, Node, RetryRoute};
use once_cell::sync::OnceCell;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

pub(crate) const STATUSES: &[&str] = &[
    "running",
    "circuit_broken",
    "budget_exceeded",
    "failed",
    "awaiting_subagents",
    "pending",
    "passed",
    "cached",
];
const CONTAINERS: &[&str] = &["subgraph", "splitter", "loop_subgraph"];
pub fn parse_timestamp(v: Option<&str>) -> DateTime<Utc> {
    let v = v.unwrap_or_default();
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return t.with_timezone(&Utc);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%:z"] {
        if let Ok(t) = DateTime::parse_from_str(v, fmt) {
            return t.with_timezone(&Utc);
        }
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(v, fmt) {
            return t.and_utc();
        }
    }
    NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .ok()
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}
fn stamp(v: &Value) -> DateTime<Utc> {
    parse_timestamp(v.as_str())
}
fn time_value(v: &Value) -> &Value {
    if s(&v["updated_at"]).is_empty() {
        &v["started_at"]
    } else {
        &v["updated_at"]
    }
}
fn read_telemetry(path: Option<&Path>, warnings: &mut Vec<String>) -> Value {
    path.and_then(|p| match live::read_json(p, None) {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(if e.kind() == io::ErrorKind::NotFound {
                format!("Waiting for {}", p.display())
            } else {
                format!("Could not read {}: {e}", p.display())
            });
            None
        }
    })
    .unwrap_or_else(|| json!({}))
}
fn attention_text(v: &Value, limit: usize) -> Option<String> {
    let value = v.as_str()?;
    if has_credentials(value) {
        return None;
    }
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return None;
    }
    if value.chars().count() <= limit {
        return Some(value);
    }
    let prefix = value.chars().take(limit).collect::<String>();
    Some(
        prefix
            .rsplit_once(' ')
            .map(|(v, _)| v)
            .filter(|v| v.chars().count() >= limit / 2)
            .map(str::to_owned)
            .unwrap_or_else(|| prefix.chars().take(limit - 1).collect())
            + "…",
    )
}
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().to_string() + &chars.as_str().to_lowercase())
        .unwrap_or_default()
}
fn previews(gate: &Value) -> Vec<String> {
    array(&gate["questions"])
        .iter()
        .take(6)
        .filter_map(|q| {
            let question = attention_text(&q["question"], 180)?;
            if let Some(ticket) = attention_text(&q["ticket"], 80) {
                attention_text(&json!(format!("[{ticket}] {question}")), 180)
            } else {
                Some(question)
            }
        })
        .collect()
}
fn latest_pending<'a>(nodes: &'a [Value], selected: &mut Option<&'a Value>) {
    for n in nodes {
        if ACTIVE_SUBTASK_PARENTS.contains(&s(&n["status"])) {
            for item in array(&n["sub_graph"]["nodes"]) {
                if item["status"] == "awaiting_subagents"
                    && item["input_gate"].is_object()
                    && item["input_gate"]["submitted"].is_null()
                    && selected.is_none_or(|previous| {
                        stamp(&item["updated_at"]) > stamp(&previous["updated_at"])
                    })
                {
                    *selected = Some(item);
                }
            }
        }
        latest_pending(array(&n["children"]), selected);
    }
}
pub(crate) fn subtask_attention(nodes: &[Value]) -> Option<Value> {
    let mut selected = None;
    latest_pending(nodes, &mut selected);
    let n = selected?;
    let gate = &n["input_gate"];
    Some(json!({
        "kind": "decision",
        "headline": n["title"].as_str().filter(|s|!s.is_empty()).map(str::to_owned).unwrap_or_else(||capitalize(&s(&n["id"]).replace(['_','-']," "))),
        "detail": attention_text(&n["detail"],200),
        "node_path": gate["node_path"],
        "input_gate": gate,
        "items": previews(gate),
        "packet_path": null,
    }))
}
fn attention_kind(status: &str) -> Option<&'static str> {
    match status {
        "running" => Some("running"),
        "awaiting_subagents" => Some("waiting"),
        "failed" | "budget_exceeded" | "circuit_broken" => Some("stuck"),
        "passed" | "cached" => Some("done"),
        _ => None,
    }
}
fn candidates<'a>(nodes: &'a [Value], kind: Option<&str>, out: &mut Vec<&'a Value>) {
    for n in nodes {
        let previous = out.len();
        candidates(array(&n["children"]), kind, out);
        if previous == out.len() && attention_kind(s(&n["status"])) == kind {
            out.push(n);
        }
    }
}
struct Paths<'a> {
    state: Option<&'a Path>,
    report: Option<&'a Path>,
    project: Option<&'a Path>,
    packet_bases: std::cell::OnceCell<Option<(PathBuf, PathBuf)>>,
    watch: &'a mut dyn FnMut(&Path),
}
impl Paths<'_> {
    fn packet(&mut self, v: &Value) -> Option<PathBuf> {
        let raw = v.as_str()?;
        let (state, report) = self
            .packet_bases
            .get_or_init(|| {
                Some((
                    live::resolve(self.state?).ok()?,
                    live::resolve(self.report?).ok()?,
                ))
            })
            .as_ref()?;
        live::resolve_packet(raw, state, report, self.project, self.watch).ok()
    }
    fn watch_output(&mut self, packet_path: &Path, packet: &Value) {
        if let Some(state) = self.state {
            if let Ok(output) = live::validate_kgraph_output_path(
                live::output_path(packet),
                state,
                Some(packet_path),
                self.project,
            ) {
                (self.watch)(&output);
            }
        }
    }
}
fn attention(
    status: &str,
    nodes: &[Value],
    report: &Value,
    use_report: bool,
    paths: &mut Paths<'_>,
) -> Value {
    let mut seen = HashSet::new();
    let mut packets = Vec::new();
    if use_report {
        for raw in array(&report["packet_paths"]) {
            if let Some(p) = paths.packet(raw) {
                if attention_text(&json!(p), 200).is_some() && seen.insert(live::resolve(&p).ok()) {
                    if status == "awaiting_subagents" && raw != &report["packet_path"] {
                        if let Ok(packet) = live::read_json(&p, Some(PACKET_LIMIT)) {
                            paths.watch_output(&p, &packet);
                        }
                    }
                    packets.push(p);
                }
            }
        }
    }
    let attach = |mut a: Value| {
        if a.is_object() && packets.len() > 1 {
            a["packet_paths"] = json!(packets);
        }
        a
    };
    let kind = attention_kind(status);
    if kind.is_none() || kind == Some("done") {
        if let Some(a) = subtask_attention(nodes) {
            return attach(a);
        }
        let mut pending = Vec::new();
        candidates(nodes, None, &mut pending);
        return if kind.is_none() || !pending.is_empty() {
            Value::Null
        } else {
            attach(json!({
                "kind": "done",
                "headline": "Workflow complete",
                "detail": null,
                "node_path": null,
                "items": [],
                "packet_path": null,
            }))
        };
    }
    let mut kind = kind.unwrap();
    let segments = if use_report {
        live::report_segments(report)
    } else {
        vec![]
    };
    let (packet_path, packet) = if use_report && matches!(kind, "waiting" | "stuck") {
        paths
            .packet(&report["packet_path"])
            .and_then(|p| {
                live::read_json(&p, Some(PACKET_LIMIT))
                    .ok()
                    .map(|v| (Some(p), v))
            })
            .unwrap_or_default()
    } else {
        (None, Value::Null)
    };
    let stage = attention_text(&packet["stage"], 80);
    let bug = attention_text(&packet["bug_id"], 80);
    let mut matched = Vec::new();
    candidates(nodes, Some(kind), &mut matched);
    let specific = live::match_report_node(matched.iter().map(|n| s(&n["path"])), &segments);
    let node = matched
        .iter()
        .copied()
        .find(|n| specific == n["path"].as_str())
        .or_else(|| {
            matched.iter().rev().copied().max_by_key(|n| {
                (
                    bug.as_ref()
                        .is_some_and(|b| s(&n["path"]).split('/').any(|p| p == b)),
                    stage.is_some() && stage.as_deref() == n["id"].as_str(),
                    std::cmp::Reverse(
                        segments
                            .iter()
                            .position(|seg| n["id"] == *seg)
                            .unwrap_or(segments.len()),
                    ),
                    stamp(&n["updated_at"]),
                )
            })
        })
        .unwrap_or(&Value::Null);
    let fallback = capitalize(s(&node["id"]).replace('_', " ").trim());
    let detail = attention_text(&node["detail"], 200)
        .or_else(|| attention_text(&node["error_signature"], 200))
        .or_else(|| {
            if !use_report {
                return None;
            }
            let e = report["error"].as_str()?;
            let e = e
                .strip_prefix('[')
                .and_then(|v| v.split_once("] "))
                .map_or(e, |(_, s)| s);
            attention_text(&json!(e), 200)
        });
    let title = attention_text(
        packet
            .get("title")
            .filter(|v| !s(v).is_empty())
            .unwrap_or(&packet["bug_spec"]["title"]),
        130,
    );
    let headline = match (&title, &bug) {
        (Some(t), Some(b)) if t != b => format!("{b} · {t}"),
        _ => title
            .or(bug)
            .or_else(|| stage.as_ref().map(|s| capitalize(&s.replace('_', " "))))
            .unwrap_or(fallback),
    };
    let summary = attention_text(&packet["summary"], 200);
    let reason = attention_text(&packet["reason"], 200);
    let detail = if kind == "stuck" {
        reason.or(detail).or(summary)
    } else {
        summary.or(reason).or(detail)
    };
    let mut items: Vec<String> = array(&packet["instructions"])
        .iter()
        .take(3)
        .filter_map(|v| attention_text(v, 240))
        .collect();
    items.extend(
        array(&packet["bugs"])
            .iter()
            .take(8)
            .filter_map(|v| attention_text(if v.is_object() { &v["title"] } else { v }, 130)),
    );
    let review_gate =
        stage.as_deref() == Some("user_verify_gate") || node["id"] == "user_verify_gate";
    let mut approval = &packet["approval_path"];
    if review_gate && s(approval).is_empty() {
        approval = array(&packet["outputs"]).first().unwrap_or(&Value::Null);
    }
    let gate = if kind == "waiting" && packet.is_object() {
        if let Some(path) = packet_path.as_deref() {
            paths.watch_output(path, &packet);
        }
        live::extract_input_gate(
            &packet,
            packet_path.as_deref(),
            node["path"].as_str(),
            paths.state,
            paths.project,
            paths.state.is_some(),
        )
    } else {
        None
    };
    if gate.is_none() {
        if let Some(a) = subtask_attention(nodes) {
            return attach(a);
        }
    }
    if kind == "waiting" && (gate.is_some() || review_gate || !s(approval).is_empty()) {
        kind = "decision";
    }
    if let Some(v) = attention_text(
        packet
            .get("review_summary_path")
            .filter(|v| !s(v).is_empty())
            .unwrap_or(&packet["review_summary"]),
        160,
    ) {
        items.push(format!("Review {v}"));
    }
    if let Some(v) = attention_text(approval, 160) {
        items.push(format!("Record the decision in {v}"));
    }
    if items.is_empty() {
        if let Some(g) = &gate {
            items = previews(g);
        }
    }
    let headline = if headline.is_empty() {
        match kind {
            "running" => "Workflow running",
            "waiting" => "Workflow paused",
            "stuck" => "Workflow blocked",
            _ => "Approval needed",
        }
        .into()
    } else {
        headline
    };
    let mut result = json!({
        "kind": kind,
        "headline": headline,
        "detail": detail,
        "node_path": node["path"],
        "items": items,
        "packet_path": packet_path.filter(|p|attention_text(&json!(p),200).is_some()),
    });
    if let Some(g) = gate {
        result["input_gate"] = g;
    }
    attach(result)
}
fn cached_paths(report: &Value, records: &Map<String, Value>) -> HashSet<String> {
    let strings = |v: &Value| {
        array(v)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<HashSet<_>>()
    };
    if report.get("cached_nodes").is_some() {
        return strings(&report["cached_nodes"]);
    }
    let reached = strings(&report["scope"]["reached_nodes"]);
    let summary = &report["summary"];
    let count = summary["cached"].as_u64().unwrap_or_default();
    if count == 0 || summary["total_events"].as_u64() != Some(reached.len() as u64) {
        return HashSet::new();
    }
    let timed: HashSet<_> = array(&report["slowest_nodes"])
        .iter()
        .filter_map(|n| n["node"].as_str())
        .collect();
    let found: HashSet<_> = reached
        .into_iter()
        .filter(|p| {
            !timed.contains(p.as_str())
                && records.get(p).is_some_and(|n| {
                    matches!(s(&n["status"]), "cached" | "passed")
                        && !CONTAINERS.contains(&s(&n["kind"]))
                })
        })
        .collect();
    if found.len() as u64 == count {
        found
    } else {
        HashSet::new()
    }
}
fn aggregate(nodes: &[Value]) -> &'static str {
    if nodes.is_empty() {
        return "pending";
    }
    STATUSES
        .iter()
        .copied()
        .find(|s| nodes.iter().any(|n| n["status"] == *s))
        .unwrap_or("cached")
}
fn budget_block(records: &mut Map<String, Value>, report: &Value) {
    if report["status"] != "budget_exceeded" {
        return;
    }
    let Some((p, detail)) = s(&report["error"])
        .strip_prefix('[')
        .and_then(|e| e.split_once("] "))
    else {
        return;
    };
    if p.split('/').any(str::is_empty)
        || records
            .values()
            .any(|r| stamp(time_value(r)) > stamp(&report["generated_at"]))
    {
        return;
    }
    let r = records.entry(p).or_insert_with(|| json!({}));
    if r.get("kind").is_none() {
        r["kind"] = json!("worker");
    }
    r["status"] = json!("budget_exceeded");
    r["detail"] = json!(detail);
    r["updated_at"] = report["generated_at"].clone();
}
fn retry_edge(children: &[Value], route: Option<&RetryRoute>, retries: &Value) -> Value {
    let wave = |i: usize| children[i]["wave"].as_u64().unwrap_or_default();
    let mut ordered: Vec<_> = (0..children.len()).collect();
    ordered.sort_by_key(|&i| wave(i));
    let mut from = *ordered
        .iter()
        .max_by_key(|&&i| {
            (
                wave(i),
                matches!(s(&children[i]["kind"]), "verifier" | "gate"),
                i,
            )
        })
        .unwrap();
    let steps: Vec<_> = ordered
        .iter()
        .copied()
        .filter(|&i| {
            matches!(
                s(&children[i]["kind"]),
                "worker" | "code" | "splitter" | "loop_subgraph"
            )
        })
        .collect();
    let mut to = steps
        .iter()
        .find(|&&i| children[i]["kind"] != "code")
        .or(steps.first())
        .copied()
        .unwrap_or(ordered[0]);
    let source = route.and_then(|r| children.iter().position(|n| n["id"] == r.from));
    let target = route.and_then(|r| children.iter().position(|n| n["id"] == r.to));
    if children.len() > 1 {
        if from == to {
            from = children.len() - 1;
            to = 0;
        }
        if source == Some(to) && target.is_none() {
            to = *ordered.iter().find(|&&i| Some(i) != source).unwrap();
        }
        if target == Some(from) && source.is_none() {
            from = *ordered.iter().rev().find(|&&i| Some(i) != target).unwrap();
        }
    }
    json!({
        "from": children[source.unwrap_or(from)]["path"],
        "to": children[target.unwrap_or(to)]["path"],
        "kind": "retry",
        "label": route.map(|r|r.label.clone()).filter(|s|!s.is_empty()).unwrap_or_else(||if retries.is_null(){"if fails → retry".into()}else{format!("if fails → retry (max {retries})")}),
    })
}
struct Tree<'a> {
    records: &'a Map<String, Value>,
    paths: HashMap<String, BTreeSet<String>>,
    cached: HashSet<String>,
    report_time: DateTime<Utc>,
    progress: Value,
    counts: Map<String, Value>,
    ctx: &'a Context,
    state: Option<&'a Path>,
    warnings: &'a mut Vec<String>,
}
impl Tree<'_> {
    fn level(
        &mut self,
        defined: &[Node],
        parent: &str,
        include_recorded: bool,
        parallel: bool,
    ) -> (Vec<Value>, Vec<Value>) {
        // Definitions retain their first position if a splitter emits a
        // repeated ID, with the last definition supplying the node fields.
        let by_id: HashMap<_, _> = defined.iter().map(|n| (n.id(), n)).collect();
        let mut seen = HashSet::with_capacity(defined.len());
        let mut children: Vec<_> = defined
            .iter()
            .filter(|n| seen.insert(n.id()))
            .map(|n| (n.id().to_owned(), Some(by_id[n.id()])))
            .collect();
        if include_recorded {
            if let Some(paths) = self.paths.get(parent) {
                children.extend(
                    paths
                        .iter()
                        .filter(|id| !by_id.contains_key(id.as_str()))
                        .map(|id| (id.clone(), None)),
                );
            }
        }
        let mut nodes: Vec<_> = children
            .into_iter()
            .map(|(id, def)| {
                let p = if parent.is_empty() {
                    id.clone()
                } else {
                    format!("{parent}/{id}")
                };
                self.node(&id, &p, def, include_recorded)
            })
            .collect();
        let edges = if parallel {
            vec![]
        } else {
            live::wave_edges(&mut nodes, self.warnings, parent)
        };
        (nodes, edges)
    }
    fn node(&mut self, id: &str, path: &str, def: Option<&Node>, include_recorded: bool) -> Value {
        let r = self.records.get(path).unwrap_or(&Value::Null);
        let node_type = def.map(Node::snapshot_node_type).unwrap_or_else(|| {
            let k = s(r.get("node_type").unwrap_or(&r["kind"]));
            if CONTAINERS.contains(&k) {
                k
            } else if self.paths.contains_key(path) {
                "subgraph"
            } else {
                "task"
            }
        });
        let kind = if node_type == "task" {
            def.map(Node::kind_str).unwrap_or_else(|| s(&r["kind"]))
        } else {
            node_type
        };
        let kind = if TASK_KINDS.contains(&kind) || CONTAINERS.contains(&kind) {
            kind
        } else {
            "code"
        };
        let mut retries = r["max_retries"].clone();
        let mut workers = r["max_workers"].clone();
        let mut timeout = r["timeout_s"].clone();
        let mut cacheable = r["cacheable"].clone();
        let mut route = None;
        let mut branches = Vec::new();
        let defined = match def {
            Some(Node::Task(n)) => {
                retries = json!(n.max_retries);
                timeout = json!(n.timeout_s);
                cacheable = json!(n.cacheable);
                &[][..]
            }
            Some(Node::Subgraph(n)) => n.nodes.as_slice(),
            Some(Node::LoopSubgraph(n)) => {
                retries = json!(n.max_retries);
                route = n.retry_route.as_ref();
                n.nodes.as_slice()
            }
            Some(Node::Splitter(n)) => {
                workers = json!(n.max_workers);
                match (n.items_fn)(self.ctx) {
                    Err(e) => self
                        .warnings
                        .push(format!("{path} items are unavailable: {e}")),
                    Ok(items) => {
                        for (i, item) in items.iter().enumerate() {
                            match (n.child_builder_fn)(item, self.ctx) {
                                Ok(child) => branches.push(child),
                                Err(e) => self
                                    .warnings
                                    .push(format!("{path} branch {} is unavailable: {e}", i + 1)),
                            }
                        }
                    }
                }
                branches.as_slice()
            }
            None => &[],
        };
        let (children, mut edges) = if CONTAINERS.contains(&node_type) {
            self.level(
                defined,
                path,
                include_recorded || node_type == "splitter",
                node_type == "splitter",
            )
        } else {
            (vec![], vec![])
        };
        if node_type == "loop_subgraph" && !children.is_empty() {
            edges.push(retry_edge(&children, route, &retries));
        }
        let mut status = s(&r["status"]);
        if !STATUSES.contains(&status) {
            status = aggregate(&children);
        }
        let mut updated = time_value(r);
        if status == "passed" && self.cached.contains(path) && stamp(updated) <= self.report_time {
            status = "cached";
        }
        if let Some(latest) = children
            .iter()
            .rev()
            .max_by_key(|n| stamp(&n["updated_at"]))
        {
            if stamp(&latest["updated_at"]) > stamp(updated) {
                updated = &latest["updated_at"];
                status = aggregate(&children);
                if node_type == "loop_subgraph" && matches!(status, "cached" | "passed") {
                    status = "running";
                }
            }
        }
        self.counts[status] = json!(self.counts[status].as_u64().unwrap_or(0) + 1);
        let mut n = json!({
            "id": id,
            "path": path,
            "node_type": node_type,
            "kind": kind,
            "deps": def.map(|n|json!(n.deps())).unwrap_or_else(||json!(array(&r["deps"]).iter().filter_map(Value::as_str).collect::<Vec<_>>())),
            "wave": 0,
            "resource": def.map(|n|json!(n.resource())).unwrap_or_else(||r["resource"].clone()),
            "max_retries": retries,
            "timeout_s": timeout,
            "cacheable": cacheable,
            "cache_miss_reason": r["cache_miss_reason"],
            "max_workers": workers,
            "status": status,
            "attempts": r.get("attempts").unwrap_or(&json!(0)),
            "duration_s": r["duration_s"],
            "input_hash": r["input_hash"],
            "outputs": array(&r["outputs"]),
            "output_hashes": r["output_hashes"].as_object().cloned().unwrap_or_default(),
            "error_signature": r["error_signature"],
            "detail": redact_detail(&r["detail"]),
            "started_at": r["started_at"],
            "updated_at": updated,
        });
        n["children"] = Value::Array(children);
        n["edges"] = Value::Array(edges);
        if let Some(entry) = self.progress.get(path) {
            let (graph, summary) = live::build_node_sub_graph(
                path,
                entry,
                self.warnings,
                self.state,
                ACTIVE_SUBTASK_PARENTS.contains(&status),
            );
            if let Some(g) = graph {
                n["sub_graph"] = g;
                n["sub_progress"] = json!(summary);
            }
        }
        n
    }
}
fn redact_detail(v: &Value) -> Value {
    v.as_str()
        .map(|s| json!(redact_credentials(s)))
        .unwrap_or_else(|| v.clone())
}
#[allow(clippy::too_many_arguments)]
fn build(
    workflow: Option<&Path>,
    state: Option<&Path>,
    report: Option<&Path>,
    definition: Option<(&[Node], &Context)>,
    title: Option<&str>,
    status_override: Option<&str>,
    project: Option<&Path>,
    watch: &mut dyn FnMut(&Path),
) -> Value {
    let mut warnings = Vec::new();
    let overview = read_telemetry(report, &mut warnings);
    let mut telemetry = read_telemetry(state, &mut warnings);
    let mut records = telemetry
        .get_mut("nodes")
        .and_then(Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    records
        .retain(|path, record| path.split('/').all(|part| !part.is_empty()) && record.is_object());
    budget_block(&mut records, &overview);
    let progress = state
        .map(|p| live::read_node_progress(p, &mut warnings))
        .unwrap_or_default();
    let mut paths: HashMap<String, BTreeSet<String>> = HashMap::new();
    for p in records.keys().chain(
        pairs(&progress)
            .filter(|(_, v)| v["items"].as_object().is_some_and(|m| !m.is_empty()))
            .map(|(p, _)| p),
    ) {
        let mut parent = String::new();
        for part in p.split('/') {
            paths
                .entry(parent.clone())
                .or_default()
                .insert(part.to_owned());
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(part);
        }
    }
    let empty = Context::new();
    let (graph, ctx) = definition.unwrap_or((&[], &empty));
    let cached = cached_paths(&overview, &records);
    let report_time = stamp(&overview["generated_at"]);
    let mut tree = Tree {
        records: &records,
        paths,
        cached,
        report_time,
        progress,
        counts: STATUSES.iter().map(|&k| (k.to_owned(), json!(0))).collect(),
        ctx,
        state,
        warnings: &mut warnings,
    };
    let (nodes, edges) = tree.level(graph, "", definition.is_none(), false);
    let latest = nodes
        .iter()
        .map(|n| stamp(&n["updated_at"]))
        .max()
        .unwrap_or(report_time);
    let mut status = aggregate(&nodes);
    if status != "running" && latest <= report_time {
        status = overview["status"].as_str().unwrap_or(status);
    }
    let status = status_override.filter(|s| !s.is_empty()).unwrap_or(status);
    let workflow = workflow
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            overview["workflow_ref"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| telemetry["workflow_ref"].as_str())
                .map(str::to_owned)
        });
    let project = project.or_else(|| workflow.as_deref().and_then(|w| Path::new(w).parent()));
    let title = title
        .filter(|s| !s.is_empty())
        .or_else(|| {
            workflow
                .as_deref()
                .map(Path::new)
                .or(state)
                .and_then(Path::file_stem)
                .and_then(|s| s.to_str())
        })
        .unwrap_or("workflow");
    let att = attention(
        status,
        &nodes,
        &overview,
        overview["status"] == status && latest <= report_time,
        &mut Paths {
            state,
            report,
            project,
            packet_bases: Default::default(),
            watch,
        },
    );
    let failures = array(&overview["failure_signatures"])
        .iter()
        .cloned()
        .map(|mut v| {
            if v.get("detail").is_some() {
                v["detail"] = redact_detail(&v["detail"]);
            }
            v
        })
        .collect::<Vec<_>>();
    let counts = tree.counts;
    let mut snapshot = json!({
        "version": 1,
        "title": title,
        "status": status,
        "counts": null,
        "nodes": null,
        "edges": null,
        "attention": null,
        "summary": overview["summary"].as_object().cloned().unwrap_or_default(),
        "scope": overview["scope"].as_object().cloned().unwrap_or_default(),
        "failure_signatures": failures,
        "slowest_nodes": array(&overview["slowest_nodes"]),
        "evolution_hints": array(&overview["evolution_hints"]),
        "workflow_ref": workflow,
        "state_file": state,
        "report_file": report,
        "generated_at": live::now(),
        "warning": (!warnings.is_empty()).then(||warnings.join(" • ")),
    });
    snapshot["counts"] = Value::Object(counts);
    snapshot["nodes"] = Value::Array(nodes);
    snapshot["edges"] = Value::Array(edges);
    snapshot["attention"] = att;
    if overview["telemetry_summary"].is_object() {
        snapshot["telemetry_summary"] = overview["telemetry_summary"].clone();
    }
    if let Some(state) = state {
        watch(&crate::workboard::leases_path_for_state(state));
        let leases = crate::workboard::read_active_leases(state);
        if !leases.is_empty() {
            snapshot["leases"] = json!(leases);
        }
    }
    snapshot
}
pub fn build_town_snapshot_from_graph(
    workflow_ref: Option<&Path>,
    state_path: Option<&Path>,
    report_path: Option<&Path>,
    definition: Option<(&[Node], &Context)>,
    title: Option<&str>,
    status_override: Option<&str>,
    project_dir: Option<&Path>,
) -> Value {
    build(
        workflow_ref,
        state_path,
        report_path,
        definition,
        title,
        status_override,
        project_dir,
        &mut |_| {},
    )
}
pub fn disable_responses(snapshot: &Value) -> Value {
    fn scrub(v: &mut Value) {
        match v {
            Value::Object(m) => {
                if let Some(g) = m.get_mut("input_gate").and_then(Value::as_object_mut) {
                    g.insert("can_respond".into(), json!(false));
                }
                for v in m.values_mut() {
                    scrub(v);
                }
            }
            Value::Array(a) => {
                for v in a {
                    scrub(v);
                }
            }
            _ => {}
        }
    }
    let mut result = snapshot.clone();
    scrub(&mut result);
    result
}
type Signature = (u64, u128, u64, i128, i128);
fn signature(p: &Path) -> Option<Signature> {
    #[cfg(not(windows))]
    let m = fs::metadata(p).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ns = |s: i64, n: i64| s as i128 * 1_000_000_000 + n as i128;
        Some((
            m.dev(),
            u128::from(m.ino()),
            m.len(),
            ns(m.mtime(), m.mtime_nsec()),
            ns(m.ctime(), m.ctime_nsec()),
        ))
    }
    #[cfg(windows)]
    {
        windows_signature(p)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let ns = |t: std::time::SystemTime| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as i128)
        };
        Some((
            0,
            0,
            m.len(),
            ns(m.modified().ok()?)?,
            ns(m.created().ok()?)?,
        ))
    }
}
// The stable Windows MetadataExt exposes creation time rather than change
// time. Read both identity and metadata from the same open file handle.
#[cfg(windows)]
fn windows_signature(path: &Path) -> Option<Signature> {
    use std::ffi::c_void;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume: u64,
        id: [u8; 16],
    }
    #[repr(C)]
    #[derive(Default)]
    struct FileBasicInfo {
        creation: i64,
        access: i64,
        write: i64,
        change: i64,
        attributes: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            buffer: *mut c_void,
            len: u32,
        ) -> i32;
    }
    let file = fs::OpenOptions::new()
        .access_mode(0x80)
        .custom_flags(0x02000000)
        .open(path)
        .ok()?;
    let mut id = FileIdInfo::default();
    let mut basic = FileBasicInfo::default();
    // The structures above match the Win32 FILE_ID_INFO and FILE_BASIC_INFO
    // layouts. Each buffer is writable for the duration of its call.
    let identified = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            (&raw mut id).cast(),
            std::mem::size_of_val(&id) as u32,
        )
    } != 0;
    let described = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            0,
            (&raw mut basic).cast(),
            std::mem::size_of_val(&basic) as u32,
        )
    } != 0;
    let size = file.metadata().ok()?.len();
    if !identified || !described {
        // Older network filesystems can decline extended metadata. In that
        // case invalidate on every request rather than reuse a stale result.
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        return Some((
            0,
            GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128,
            size,
            0,
            0,
        ));
    }
    Some((
        id.volume,
        u128::from_le_bytes(id.id),
        size,
        i128::from(basic.write) * 100,
        i128::from(basic.change) * 100,
    ))
}

pub type DefinitionLoader = Arc<dyn Fn() -> Result<(Vec<Node>, Context), String> + Send + Sync>;
pub type SnapshotProvider = Arc<dyn Fn() -> Result<Value, String> + Send + Sync>;
struct Snapshot {
    value: Arc<Value>,
    expires_at: Option<DateTime<Utc>>,
    // Keep distinct representations for authenticated and read-only viewers.
    bytes: [OnceCell<Bytes>; 2],
}

#[derive(PartialEq)]
struct GraphKey {
    workflow: Option<(PathBuf, Option<Signature>)>,
    context: Option<(PathBuf, Option<Signature>)>,
}

#[derive(Default)]
struct Cache {
    graph_key: Option<GraphKey>,
    definition: Option<(Vec<Node>, Context)>,
    workflow_title: Option<String>,
    watched: HashMap<PathBuf, Option<Signature>>,
    snapshot: Option<Arc<Snapshot>>,
}

impl Cache {
    fn current(&self, graph_key: &GraphKey) -> Option<&Arc<Snapshot>> {
        let snapshot = self.snapshot.as_ref()?;
        (self.graph_key.as_ref() == Some(graph_key)
            && snapshot.expires_at.is_none_or(|at| at > Utc::now())
            && self
                .watched
                .iter()
                .all(|(path, sig)| signature(path) == *sig))
        .then_some(snapshot)
    }
}

pub struct CachedSnapshotProvider {
    workflow: Option<PathBuf>,
    state: Option<PathBuf>,
    report: Option<PathBuf>,
    context_file: Option<PathBuf>,
    title: Option<String>,
    loader: Option<DefinitionLoader>,
    recorded_only: bool,
    cache: RwLock<Cache>,
}

impl CachedSnapshotProvider {
    pub fn new(
        workflow: Option<PathBuf>,
        state: Option<PathBuf>,
        report: Option<PathBuf>,
        context_file: Option<PathBuf>,
        title: Option<String>,
        loader: Option<DefinitionLoader>,
    ) -> Self {
        Self {
            workflow,
            state,
            report,
            context_file,
            title,
            loader,
            recorded_only: false,
            cache: RwLock::new(Cache::default()),
        }
    }

    /// Use checkpoint topology for workflows without a native definition loader.
    pub fn recorded_only(mut self) -> Self {
        self.recorded_only = true;
        self
    }

    fn graph_key(&self) -> GraphKey {
        let key = |p: &Path| {
            (
                live::resolve(p).unwrap_or_else(|_| p.to_owned()),
                signature(p),
            )
        };
        GraphKey {
            workflow: self.workflow.as_deref().map(key),
            context: self.context_file.as_deref().map(key),
        }
    }

    pub fn value(&self) -> Result<Arc<Value>, String> {
        Ok(self.snapshot()?.value.clone())
    }

    pub(crate) fn warm_bytes(&self, read_only: bool) -> Option<Bytes> {
        // Never wait on a rebuild on the async request thread.
        let cache = self.cache.try_read().ok()?;
        let bytes = cache.snapshot.as_ref()?.bytes[usize::from(read_only)].get()?;
        cache.current(&self.graph_key())?;
        Some(bytes.clone())
    }

    pub(crate) fn json_bytes(&self, read_only: bool) -> Result<Bytes, String> {
        let snapshot = self.snapshot()?;
        snapshot.bytes[usize::from(read_only)]
            .get_or_try_init(|| {
                let bytes = if read_only {
                    serde_json::to_vec(&disable_responses(&snapshot.value))
                } else {
                    serde_json::to_vec(&*snapshot.value)
                };
                bytes.map(Bytes::from).map_err(|err| err.to_string())
            })
            .cloned()
    }

    fn snapshot(&self) -> Result<Arc<Snapshot>, String> {
        let Self {
            workflow,
            state,
            report,
            title,
            loader,
            context_file,
            ..
        } = self;
        if workflow.is_none() && state.is_none() {
            return Err("Provide a workflow or a state file".into());
        }
        {
            let cache = self
                .cache
                .read()
                .map_err(|_| "Snapshot cache lock is poisoned")?;
            if let Some(snapshot) = cache.current(&self.graph_key()) {
                return Ok(snapshot.clone());
            }
        }
        let mut cache = self
            .cache
            .write()
            .map_err(|_| "Snapshot cache lock is poisoned")?;
        let graph_key = self.graph_key();
        if let Some(snapshot) = cache.current(&graph_key) {
            return Ok(snapshot.clone());
        }
        if cache.graph_key.as_ref() != Some(&graph_key) {
            let (definition, workflow_title) = match (loader, workflow) {
                _ if self.recorded_only => (None, None),
                (Some(load), _) => (Some(load()?), None),
                (None, Some(path)) => {
                    let graph = kgraph_subgraphs::declarative::load_declarative_workflow(
                        path,
                        None,
                        context_file.as_deref(),
                    )?;
                    (Some((graph.nodes, graph.context)), graph.title)
                }
                (None, None) => (None, None),
            };
            cache.definition = definition;
            cache.workflow_title = workflow_title;
            cache.graph_key = Some(graph_key);
            cache.snapshot = None;
        }
        let mut watched = HashMap::new();
        let mut watch = |p: &Path| {
            watched.entry(p.to_owned()).or_insert_with(|| signature(p));
        };
        for p in state.iter().chain(report.iter()) {
            watch(p);
        }
        if let Some(s) = state {
            watch(&live::progress_path_for_state(s));
        }
        let resolved_workflow = cache
            .graph_key
            .as_ref()
            .and_then(|key| key.workflow.as_ref());
        let v = build(
            resolved_workflow.map(|(path, _)| path.as_path()),
            state.as_deref(),
            report.as_deref(),
            cache.definition.as_ref().map(|(n, c)| (n.as_slice(), c)),
            title.as_deref().or(cache.workflow_title.as_deref()),
            None,
            None,
            &mut watch,
        );
        let snapshot = Arc::new(Snapshot {
            expires_at: array(&v["leases"])
                .iter()
                .map(|l| stamp(&l["expires_at"]))
                .min(),
            value: Arc::new(v),
            bytes: Default::default(),
        });
        cache.snapshot = Some(snapshot.clone());
        cache.watched = watched;
        Ok(snapshot)
    }
}

pub fn cached_town_snapshot(
    workflow: Option<PathBuf>,
    state: Option<PathBuf>,
    report: Option<PathBuf>,
    context_file: Option<PathBuf>,
    title: Option<String>,
    loader: Option<DefinitionLoader>,
) -> SnapshotProvider {
    let provider =
        CachedSnapshotProvider::new(workflow, state, report, context_file, title, loader);
    Arc::new(move || Ok((*provider.value()?).clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::{
        files::write_json,
        types::{LoopSubgraphNode, SplitterNode, TaskKind, TaskNode, VerifyOutcome},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn task(id: &str, kind: TaskKind) -> Node {
        Node::Task(TaskNode::new(
            id,
            kind,
            Arc::new(|_, _| Ok(Value::Null)),
            Arc::new(|_| Ok(String::new())),
        ))
    }
    #[test]
    fn nested_splitters_and_loops_retain_children_and_retry_routes() {
        let splitter = |name: &str, child: Node| {
            Node::Splitter(SplitterNode::new(
                name,
                Arc::new(|_| Ok(vec![json!(1)])),
                Arc::new(move |_, _| Ok(child.clone())),
            ))
        };
        let nested = splitter("outer", splitter("inner", task("leaf", TaskKind::Code)));
        let mut verify = task("verify", TaskKind::Verifier);
        if let Node::Task(n) = &mut verify {
            n.deps = vec!["repair".into()];
        }
        let loop_node = LoopSubgraphNode::new(
            "cycle",
            vec![task("repair", TaskKind::Worker), verify],
            Arc::new(|_| Ok(VerifyOutcome::pass(""))),
        );
        let v = build_town_snapshot_from_graph(
            Some(Path::new("release.json")),
            None,
            None,
            Some((&[nested, Node::LoopSubgraph(loop_node)], &Context::new())),
            None,
            None,
            None,
        );
        assert_eq!(
            v["nodes"][0]["children"][0]["children"][0]["path"],
            "outer/inner/leaf"
        );
        assert_eq!(
            v["nodes"][1]["edges"],
            json!([{"from":"cycle/repair","to":"cycle/verify"},{"from":"cycle/verify","to":"cycle/repair","kind":"retry","label":"if fails → retry (max 2)"}])
        );
        assert_eq!(v["counts"]["pending"], 6);
        assert!(v["warning"].is_null());
        assert!(v.get("qa_scorecard").is_none());
        assert_eq!(
            parse_timestamp(Some("2025-02-01T11:00:00+02:00")),
            parse_timestamp(Some("2025-02-01 09:00:00"))
        );
        assert_eq!(parse_timestamp(None), parse_timestamp(Some("0001-01-01")));
    }
    #[test]
    fn snapshot_status_accounts_for_budget_cache_and_latest_child() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state.json");
        let report = temp.path().join("report.json");
        write_json(&state,&json!({"nodes":{"loop":{"kind":"loop_subgraph","status":"passed","updated_at":"2025-01-01"},
            "loop/child":{"status":"passed","updated_at":"2025-01-02","detail":"api_key=private","cache_miss_reason":"inputs changed"}}}),true).unwrap();
        write_json(
            &report,
            &json!({
                "status": "passed",
                "generated_at": "2025-01-03",
                "cached_nodes": ["loop/child"],
                "qa_scorecard": {},
                "failure_signatures": [{"detail":"password='hidden phrase'"}],
            }),
            true,
        )
        .unwrap();
        let snapshot = || {
            build_town_snapshot_from_graph(
                None,
                Some(&state),
                Some(&report),
                None,
                None,
                None,
                None,
            )
        };
        let v = snapshot();
        assert_eq!(v["status"], "running");
        assert_eq!(v["nodes"][0]["children"][0]["status"], "cached");
        assert_eq!(
            v["nodes"][0]["children"][0]["cache_miss_reason"],
            "inputs changed"
        );
        assert_eq!(v["failure_signatures"][0]["detail"], "[redacted]");
        assert!(!v.to_string().contains("private"));
        assert!(v.get("qa_scorecard").is_none());
        write_json(&state, &json!({}), true).unwrap();
        write_json(&report,&json!({"status":"budget_exceeded","generated_at":"2025-01-03","error":"[scan/check] Limit reached"}),true).unwrap();
        let v = snapshot();
        assert_eq!(v["attention"]["node_path"], "scan/check");
        assert_eq!(v["attention"]["detail"], "Limit reached");
        assert_eq!(v["nodes"][0]["children"][0]["kind"], "worker");
    }
    #[test]
    fn default_loader_reloads_native_definition_and_recovers_from_invalid_json() {
        let temp = tempfile::tempdir().unwrap();
        let workflow = temp.path().join("release.json");
        let context = temp.path().join("ctx.json");
        write_json(
            &workflow,
            &json!({"nodes":[{"id":"{{ctx.stage}}","type":"subgraph","nodes":[]}]}),
            true,
        )
        .unwrap();
        write_json(&context, &json!({"stage":"build"}), true).unwrap();
        let current = cached_town_snapshot(
            Some(workflow.clone()),
            None,
            None,
            Some(context.clone()),
            None,
            None,
        );
        assert_eq!(current().unwrap()["nodes"][0]["id"], "build");
        write_json(&context, &json!({"stage":"check"}), true).unwrap();
        assert_eq!(current().unwrap()["nodes"][0]["id"], "check");
        fs::write(&workflow, "{").unwrap();
        assert!(current().is_err());
        write_json(&workflow, &json!({"nodes":[]}), true).unwrap();
        assert_eq!(current().unwrap()["nodes"], json!([]));
    }
    #[test]
    fn cache_notices_replacements_context_progress_and_gate_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().canonicalize().unwrap();
        let root = project.join(".kgraph");
        let state = root.join("state.json");
        let report = root.join("report.json");
        let workflow = project.join("workflow.json");
        let ctx = project.join("ctx.json");
        let first = root.join("first.json");
        let second = root.join("second.json");
        let outside = project.join("outside.json");
        write_json(
            &state,
            &json!({"nodes":{"work":{"status":"awaiting_subagents","detail":"A"}}}),
            true,
        )
        .unwrap();
        write_json(&report,&json!({"status":"awaiting_subagents","packet_path":first,"packet_paths":[first,outside,second]}),true).unwrap();
        write_json(&workflow, &json!({}), true).unwrap();
        write_json(&ctx, &json!({}), true).unwrap();
        write_json(&outside, &json!({"question":"private"}), true).unwrap();
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = loads.clone();
        let loader: DefinitionLoader = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok((vec![task("work", TaskKind::Gate)], Context::new()))
        });
        let provider = CachedSnapshotProvider::new(
            Some(workflow.clone()),
            Some(state.clone()),
            Some(report.clone()),
            Some(ctx.clone()),
            None,
            Some(loader),
        );
        let current = || {
            let snapshot = provider.value().unwrap();
            let encoded = provider.json_bytes(false).unwrap();
            assert_eq!(encoded.as_ref(), snapshot.to_string().as_bytes());
            snapshot
        };
        let initial = current();
        assert!(initial["attention"]["packet_path"].is_null());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| assert_eq!(current(), initial));
            }
        });
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let old = state.metadata().unwrap();
        write_json(
            &state,
            &json!({"nodes":{"work":{"status":"awaiting_subagents","detail":"B"}}}),
            true,
        )
        .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&state)
            .unwrap()
            .set_modified(old.modified().unwrap())
            .unwrap();
        assert_eq!(state.metadata().unwrap().len(), old.len());
        assert!(provider.warm_bytes(false).is_none());
        assert_eq!(current()["nodes"][0]["detail"], "B");
        write_json(
            &first,
            &json!({"question":"Continue?","output_path":"answer.json"}),
            true,
        )
        .unwrap();
        let v = current();
        assert_eq!(v["attention"]["packet_path"], json!(first));
        assert!(v["attention"].get("packet_paths").is_none());
        write_json(&second, &json!({"question":"Second?"}), true).unwrap();
        assert_eq!(
            current()["attention"]["packet_paths"],
            json!([first, second])
        );
        write_json(
            &root.join("answer.json"),
            &json!({"resolved":true,"summary":"Reviewed"}),
            true,
        )
        .unwrap();
        assert_eq!(
            current()["attention"]["input_gate"]["submitted"]["summary"],
            "Reviewed"
        );
        live::ask_node_question(&state, "work", "review", &json!({"question":"Confirm?"})).unwrap();
        let snapshot = current();
        let disabled = disable_responses(&snapshot);
        for (read_only, expected) in [(false, snapshot.as_ref()), (true, &disabled)] {
            let bytes = provider.json_bytes(read_only).unwrap();
            assert_eq!(bytes.as_ref(), expected.to_string().as_bytes());
            assert_eq!(
                provider.warm_bytes(read_only).unwrap().as_ptr(),
                bytes.as_ptr()
            );
        }
        assert_eq!(
            snapshot["nodes"][0]["sub_graph"]["nodes"][0]["input_gate"]["can_respond"],
            true
        );
        assert_eq!(disabled["attention"]["input_gate"]["can_respond"], false);
        assert_eq!(
            disabled["nodes"][0]["sub_graph"]["nodes"][0]["input_gate"]["can_respond"],
            false
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        write_json(&ctx, &json!({"different":true}), true).unwrap();
        assert!(provider.warm_bytes(false).is_none());
        assert!(provider.warm_bytes(true).is_none());
        current();
        write_json(&workflow, &json!({"revision":2}), true).unwrap();
        current();
        assert_eq!(loads.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn default_loader_propagates_declarative_title_unless_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let workflow = dir.path().join("flow.toml");
        fs::write(
            &workflow,
            "title = \"Declared Title\"\n[[nodes]]\nid = \"step\"\ntype = \"shell\"\ncommand = [\"echo\", \"ok\"]\n",
        )
        .unwrap();
        let inherited = cached_town_snapshot(Some(workflow.clone()), None, None, None, None, None);
        assert_eq!(inherited().unwrap()["title"], "Declared Title");
        let overridden = cached_town_snapshot(
            Some(workflow),
            None,
            None,
            None,
            Some("Explicit Title".into()),
            None,
        );
        assert_eq!(overridden().unwrap()["title"], "Explicit Title");
    }
}
