//! Cross-process leases for paused Hero Town work packets.
use crate::interactions as live;
use chrono::{DateTime, TimeDelta, Utc};
use kgraph_core::files::{file_lock, write_json, FileLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LeaseRecord {
    pub node_path: String,
    pub packet_path: String,
    pub worker_id: String,
    #[serde(default)]
    pub generation: u64,
    pub leased_at: String,
    pub expires_at: String,
    pub heartbeat_count: u64,
    pub worktree_path: Option<PathBuf>,
    pub metadata: Map<String, Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorkItem {
    pub node_path: String,
    pub packet_path: String,
    pub packet: Value,
    pub expected_outputs: Vec<String>,
    pub lease: Option<LeaseRecord>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClaimedWork {
    pub lease: LeaseRecord,
    pub packet: Value,
    pub expected_outputs: Vec<String>,
}

pub fn leases_path_for_state(state: &Path) -> PathBuf {
    state.with_file_name(format!(
        "{}_leases.json",
        state.file_stem().unwrap_or_default().to_string_lossy()
    ))
}

fn lock_board(state: &Path) -> Result<(PathBuf, FileLock), String> {
    let root = live::allowed_kgraph_root(state)?;
    let path = leases_path_for_state(state);
    let lock = path.with_extension("json.lock");
    for file in [&path, &lock] {
        live::ensure_inside(file, &root)?;
        if file.is_symlink() {
            return Err("Lease files cannot be symlinks".into());
        }
    }
    Ok((path, file_lock(&lock)?))
}

// The colon is invalid in a node path. Flattening preserves old board files.
#[derive(Serialize, Deserialize, Default)]
struct Leases {
    #[serde(default, rename = ":history")]
    history: BTreeMap<String, ClaimHistory>,
    #[serde(flatten)]
    records: BTreeMap<String, LeaseRecord>,
}

#[derive(Serialize, Deserialize, Default)]
struct ClaimHistory {
    generation: u64,
    retired_workers: BTreeSet<String>,
    expired: bool,
}

const GENERATION_PREFIX: &str = "lease-generation:";

impl LeaseRecord {
    /// Pass this reference as the worker argument to fence a mutation to this claim.
    pub fn worker_reference(&self) -> String {
        format!("{GENERATION_PREFIX}{}:{}", self.generation, self.worker_id)
    }
}

impl Leases {
    fn retire(&mut self, node: &str, expired: bool) {
        if let Some(old) = self.records.remove(node) {
            let history = self.history.entry(node.to_owned()).or_default();
            history.generation = history.generation.max(old.generation);
            history.retired_workers.insert(old.worker_id);
            history.expired = expired;
        }
    }

    fn prune(&mut self, path: &Path, now: DateTime<Utc>) -> Result<(), String> {
        let expired: Vec<_> = self
            .records
            .iter()
            .filter(|(_, l)| !active(l, now))
            .map(|(node, _)| node.clone())
            .collect();
        if !expired.is_empty() {
            for node in expired {
                self.retire(&node, true);
            }
            write_json(path, self, true)?;
        }
        Ok(())
    }

    fn check_owner(&self, node: &str, worker: &str) -> Result<Option<&LeaseRecord>, String> {
        let history = self.history.get(node);
        let Some(lease) = self.records.get(node) else {
            if history.is_some_and(|h| h.expired) {
                return Err(format!("Lease for {node} expired or is no longer active"));
            }
            return Ok(None);
        };
        if worker == lease.worker_reference() {
            return Ok(Some(lease));
        }
        if worker != lease.worker_id {
            return Err(format!(
                "Lease for {node} belongs to another worker or generation"
            ));
        }
        if history.is_some_and(|h| h.retired_workers.contains(worker)) {
            return Err(format!(
                "Worker ID was reused for {node}; supply lease-generation:<generation>:<worker>"
            ));
        }
        Ok(Some(lease))
    }
}

fn load_leases(path: &Path) -> Result<Leases, String> {
    match live::json_file(path, None) {
        Ok((v, _, _)) => serde_json::from_value(v).map_err(|e| e.to_string()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Leases::default()),
        Err(e) => Err(e.to_string()),
    }
}

fn active(lease: &LeaseRecord, now: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(&lease.expires_at).is_ok_and(|end| end > now)
}

fn check_node(node: &str) -> Result<(), String> {
    if live::valid_node_path(node) {
        Ok(())
    } else {
        Err(format!("Invalid node path: {node}"))
    }
}

fn expiry(now: DateTime<Utc>, worker: &str, ttl_s: u64) -> Result<DateTime<Utc>, String> {
    if worker.trim().is_empty() || ttl_s == 0 {
        return Err("A non-empty worker ID and positive lease TTL are required".into());
    }
    i64::try_from(ttl_s)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .and_then(|ttl| now.checked_add_signed(ttl))
        .filter(|end| chrono::Datelike::year(end) <= 9999)
        .ok_or_else(|| "Lease TTL exceeds the RFC 3339 date range".into())
}

fn packet_paths(record: &Value) -> impl Iterator<Item = &str> {
    record["packet_path"]
        .as_str()
        .into_iter()
        .chain(
            live::array(&record["packet_paths"])
                .iter()
                .filter_map(Value::as_str),
        )
        .filter(|s| !s.trim().is_empty())
}

type Packet = (String, PathBuf, Value);

fn discover(
    state: &Path,
    report: Option<&Path>,
    project: Option<&Path>,
) -> Result<Vec<Packet>, String> {
    let optional = |p: &Path| match live::json_file(p, None) {
        Ok((v, _, _)) => Ok(v),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e.to_string()),
    };
    let absolute_state = std::path::absolute(state).map_err(|e| e.to_string())?;
    let lexical_root = absolute_state
        .ancestors()
        .skip(1)
        .find(|p| p.file_name().is_some_and(|n| n == ".kgraph"))
        .ok_or("Expected state inside .kgraph")?;
    let root = lexical_root
        .parent()
        .ok_or("Missing project directory")?
        .canonicalize()
        .map_err(|e| e.to_string())?
        .join(".kgraph");
    let data = optional(state)?;
    let nodes = &data["nodes"];
    let read_packet = |raw: &str| -> Result<(PathBuf, Value), String> {
        let path = live::resolve_packet(raw, state, report.unwrap_or(state), project, &mut |_| {})?;
        let path = std::path::absolute(path).map_err(|e| e.to_string())?;
        let path = path
            .strip_prefix(lexical_root)
            .map_or_else(|_| path.clone(), |rel| root.join(rel));
        let packet = crate::artifact::read_json_inside(&root, &path, live::PACKET_LIMIT)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((path, packet))
    };
    let mut found = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let waiting: BTreeSet<_> = live::pairs(nodes)
        .filter(|(_, r)| r["status"] == "awaiting_subagents")
        .map(|(n, _)| n.as_str())
        .collect();
    for &node in &waiting {
        check_node(node)?;
        if let Some(raw) = packet_paths(&nodes[node]).next() {
            let (path, packet) = read_packet(raw)?;
            if !seen.insert(path.clone()) {
                return Err(format!(
                    "Multiple waiting nodes reference {}",
                    path.display()
                ));
            }
            found.insert(node.to_owned(), (path, packet));
        }
    }
    let report_only = live::pairs(nodes).next().is_none();
    if report_only || found.len() < waiting.len() {
        let overview = report.map(optional).transpose()?.unwrap_or(Value::Null);
        // The state is authoritative. Reports from older runs may still name
        // a packet whose node has already passed.
        if report_only
            && overview["status"]
                .as_str()
                .is_some_and(|s| s != "awaiting_subagents")
        {
            return Ok(Vec::new());
        }
        let primary = packet_paths(&overview)
            .next()
            .or_else(|| packet_paths(&data).next());
        let fallback = live::active_gate(&data, &overview, None).map(|(n, _)| n);
        let mut unnamed = BTreeMap::new();
        for raw in packet_paths(&overview).chain(packet_paths(&data)) {
            let (path, packet) = read_packet(raw)?;
            if seen.contains(&path) {
                continue;
            }
            let hint = packet["node_path"].as_str().or_else(|| {
                if Some(raw) == primary {
                    fallback.as_deref().or(overview["node_path"].as_str())
                } else {
                    None
                }
            });
            if let Some(node) = hint {
                check_node(node)?;
                if (report_only || waiting.contains(node)) && !found.contains_key(node) {
                    seen.insert(path.clone());
                    found.insert(node.to_owned(), (path, packet));
                }
            } else {
                unnamed.insert(path, packet);
            }
        }
        // Older packets may omit node_path. A single remaining pair is
        // unambiguous after matching the primary packet and state records.
        unnamed.retain(|path, _| !seen.contains(path));
        let mut missing = waiting.iter().filter(|node| !found.contains_key(**node));
        if let (Some(&node), None) = (missing.next(), missing.next()) {
            if unnamed.len() == 1 {
                found.insert(node.to_owned(), unnamed.pop_first().unwrap());
            }
        }
    }
    Ok(found
        .into_iter()
        .map(|(node, (path, packet))| (node, path, packet))
        .collect())
}

pub fn discover_waiting_packets(
    state: &Path,
    report: Option<&Path>,
    project: Option<&Path>,
) -> Result<Vec<(String, PathBuf, Value)>, String> {
    let (_path, _guard) = lock_board(state)?;
    discover(state, report, project)
}

fn outputs(packet: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    live::array(&packet["outputs"])
        .iter()
        .chain([&packet["output_path"], &packet["approval_path"]])
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty() && seen.insert(*s))
        .map(str::to_owned)
        .collect()
}

pub fn list_workboard(
    state: &Path,
    report: Option<&Path>,
    project: Option<&Path>,
) -> Result<Vec<WorkItem>, String> {
    let (path, _guard) = lock_board(state)?;
    let packets = discover(state, report, project)?;
    let mut leases = load_leases(&path)?;
    leases.prune(&path, Utc::now())?;
    Ok(packets
        .into_iter()
        .map(|(node_path, path, packet)| WorkItem {
            lease: leases
                .records
                .remove(&node_path)
                .filter(|l| Path::new(&l.packet_path) == path),
            node_path,
            packet_path: path.display().to_string(),
            expected_outputs: outputs(&packet),
            packet,
        })
        .collect())
}

fn progress(
    state: &Path,
    node: &str,
    worker: &str,
    status: &str,
    detail: &str,
) -> Result<(), String> {
    live::update_node_progress(
        state,
        node,
        Some("lease"),
        &json!({"kind":"worker","status":status,"agent":worker,"detail":detail}),
        false,
    )
    .map(|_| ())
}

pub fn claim_work(
    state: &Path,
    report: Option<&Path>,
    project: Option<&Path>,
    worker_id: &str,
    ttl_s: u64,
    preferred_node: Option<&str>,
    worktree_path: Option<PathBuf>,
) -> Result<Option<ClaimedWork>, String> {
    if worker_id.starts_with(GENERATION_PREFIX) {
        return Err(format!("Worker IDs cannot start with {GENERATION_PREFIX}"));
    }
    if let Some(node) = preferred_node {
        check_node(node)?;
    }
    let (path, _guard) = lock_board(state)?;
    let packets = discover(state, report, project)?;
    let mut leases = load_leases(&path)?;
    let now = Utc::now();
    let end = expiry(now, worker_id, ttl_s)?;
    let waiting: BTreeMap<_, _> = packets
        .iter()
        .map(|(n, p, _)| (n.as_str(), p.as_path()))
        .collect();
    leases.prune(&path, now)?;
    let stale: Vec<_> = leases
        .records
        .iter()
        .filter(|(n, v)| waiting.get(n.as_str()) != Some(&Path::new(&v.packet_path)))
        .map(|(n, _)| n.clone())
        .collect();
    for node in &stale {
        leases.retire(node, true);
    }
    let next = packets.into_iter().find(|(n, _, _)| {
        preferred_node.is_none_or(|preferred| n == preferred)
            && leases
                .records
                .get(n)
                .is_none_or(|l| preferred_node.is_some() && l.worker_id == worker_id)
    });
    let Some((node, packet_path, packet)) = next else {
        if !stale.is_empty() {
            write_json(&path, &leases, true)?;
        }
        return Ok(None);
    };
    let history = leases.history.entry(node.clone()).or_default();
    history.generation = history
        .generation
        .max(leases.records.get(&node).map_or(0, |l| l.generation))
        .checked_add(1)
        .ok_or("Lease generation overflow")?;
    history.expired = false;
    let lease = leases
        .records
        .entry(node.clone())
        .or_insert_with(|| LeaseRecord {
            node_path: node.clone(),
            packet_path: packet_path.display().to_string(),
            worker_id: worker_id.to_owned(),
            generation: history.generation,
            leased_at: now.to_rfc3339(),
            expires_at: String::new(),
            heartbeat_count: 0,
            worktree_path: None,
            metadata: Map::new(),
        });
    lease.generation = history.generation;
    lease.expires_at = end.to_rfc3339();
    if worktree_path.is_some() {
        lease.worktree_path = worktree_path;
    }
    let lease = lease.clone();
    progress(
        state,
        &node,
        worker_id,
        "running",
        &format!("Claimed by {worker_id}"),
    )?;
    write_json(&path, &leases, true)?;
    Ok(Some(ClaimedWork {
        lease,
        expected_outputs: outputs(&packet),
        packet,
    }))
}

pub fn heartbeat_lease(
    state: &Path,
    node_path: &str,
    worker_id: &str,
    ttl_s: u64,
) -> Result<LeaseRecord, String> {
    check_node(node_path)?;
    let (path, _guard) = lock_board(state)?;
    let mut leases = load_leases(&path)?;
    let now = Utc::now();
    leases.prune(&path, now)?;
    let end = expiry(now, worker_id, ttl_s)?;
    leases.check_owner(node_path, worker_id)?;
    let lease = leases
        .records
        .get_mut(node_path)
        .ok_or_else(|| format!("Lease for {node_path} is no longer active"))?;
    lease.expires_at = end.to_rfc3339();
    lease.heartbeat_count = lease
        .heartbeat_count
        .checked_add(1)
        .ok_or("Heartbeat count overflow")?;
    let lease = lease.clone();
    write_json(&path, &leases, true)?;
    Ok(lease)
}

fn finish(
    state: &Path,
    node: &str,
    worker: &str,
    status: &str,
    detail: &str,
) -> Result<bool, String> {
    check_node(node)?;
    let (path, _guard) = lock_board(state)?;
    let mut leases = load_leases(&path)?;
    leases.prune(&path, Utc::now())?;
    let Some(lease) = leases.check_owner(node, worker)? else {
        return Ok(false);
    };
    progress(state, node, &lease.worker_id, status, detail)?;
    leases.retire(node, false);
    write_json(&path, &leases, true)?;
    Ok(true)
}

pub fn release_lease(
    state: &Path,
    node_path: &str,
    worker_id: &str,
    reason: Option<&str>,
) -> Result<bool, String> {
    finish(
        state,
        node_path,
        worker_id,
        "awaiting_subagents",
        reason.unwrap_or("Released"),
    )
}

pub fn complete_lease(
    state: &Path,
    node_path: &str,
    worker_id: &str,
    summary: Option<&str>,
) -> Result<bool, String> {
    finish(
        state,
        node_path,
        worker_id,
        "passed",
        summary.unwrap_or("Completed"),
    )
}

pub fn read_active_leases(state: &Path) -> Vec<LeaseRecord> {
    if !leases_path_for_state(state).is_file() {
        return Vec::new();
    }
    lock_board(state)
        .and_then(|(path, _guard)| load_leases(&path))
        .map(|leases| {
            let now = Utc::now();
            leases
                .records
                .into_values()
                .filter(|l| active(l, now))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use tempfile::{tempdir, TempDir};

    fn board(nodes: &[&str]) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let state = dir
            .path()
            .canonicalize()
            .unwrap()
            .join(".kgraph/nested/demo_state.json");
        let mut records = json!({});
        for (i, node) in nodes.iter().enumerate() {
            let name = format!("packet-{i}.json");
            write_json(&state.with_file_name(&name), &json!({"outputs":["artifact.json"], "output_path":"artifact.json", "approval_path":"approval.json"}), false).unwrap();
            records[node] = json!({"status":"awaiting_subagents", "packet_path":name});
        }
        write_json(&state, &json!({"nodes":records}), false).unwrap();
        (dir, state)
    }

    #[test]
    fn simultaneous_workers_claim_each_packet_once() {
        let (_dir, state) = board(&["stage/z", "stage/a", "other"]);
        let start = Barrier::new(8);
        let claimed = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|id| {
                    let start = &start;
                    let state = &state;
                    scope.spawn(move || {
                        start.wait();
                        claim_work(state, None, None, &format!("worker-{id}"), 60, None, None)
                            .unwrap()
                    })
                })
                .collect();
            jobs.into_iter()
                .filter_map(|j| j.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(claimed.len(), 3);
        let nodes: BTreeSet<_> = claimed.iter().map(|c| c.lease.node_path.as_str()).collect();
        assert_eq!(nodes, BTreeSet::from(["stage/z", "stage/a", "other"]));
        let items = list_workboard(&state, None, None).unwrap();
        assert_eq!(items[0].node_path, "other");
        assert_eq!(
            items[0].expected_outputs,
            ["artifact.json", "approval.json"]
        );
        assert!(items.iter().all(|i| i.lease.is_some()));
    }

    #[test]
    fn owner_heartbeat_release_and_completion() {
        let (_dir, state) = board(&["z", "a"]);
        assert!(claim_work(&state, None, None, " ", 1, None, None).is_err());
        for ttl in [0, u64::MAX] {
            assert!(claim_work(&state, None, None, "one", ttl, None, None).is_err());
        }
        assert!(claim_work(&state, None, None, "one", 10, Some("../z"), None).is_err());
        let first = claim_work(
            &state,
            None,
            None,
            "one",
            30,
            None,
            Some(PathBuf::from("wt")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.lease.node_path, "a");
        assert!(claim_work(&state, None, None, "two", 30, Some("a"), None)
            .unwrap()
            .is_none());
        assert!(heartbeat_lease(&state, "a", "two", 30).is_err());
        assert!(release_lease(&state, "a", "two", None).is_err());
        assert!(complete_lease(&state, "a", "two", None).is_err());
        let heart = heartbeat_lease(&state, "a", "one", 60).unwrap();
        assert_eq!(heart.heartbeat_count, 1);
        assert!(heart.expires_at > first.lease.expires_at);
        let again = claim_work(&state, None, None, "one", 60, Some("a"), None)
            .unwrap()
            .unwrap();
        assert_eq!(again.lease.worktree_path, first.lease.worktree_path);
        assert_eq!(again.lease.leased_at, first.lease.leased_at);
        assert_eq!(again.lease.heartbeat_count, 1);
        let item = || live::read_node_progress(&state, &mut vec![])["a"]["items"]["lease"].clone();
        assert_eq!(item()["status"], "running");
        assert!(release_lease(&state, "a", "one", Some("Review needed")).unwrap());
        assert_eq!(item()["status"], "awaiting_subagents");
        assert_eq!(item()["detail"], "Review needed");
        assert!(!release_lease(&state, "a", "one", None).unwrap());
        assert!(claim_work(&state, None, None, "two", 30, Some("a"), None)
            .unwrap()
            .is_some());
        assert!(complete_lease(&state, "a", "two", Some("Reviewed")).unwrap());
        assert_eq!(item()["status"], "passed");
        assert_eq!(item()["detail"], "Reviewed");
        assert_eq!(item()["agent"], "two");
        assert!(read_active_leases(&state).is_empty());
    }

    #[test]
    fn ttl_expires_without_writes_and_crashed_worker_is_replaced() {
        let (_dir, state) = board(&["a", "b"]);
        let cached = crate::snapshot::cached_town_snapshot(
            None,
            Some(state.clone()),
            None,
            None,
            None,
            None,
        );
        assert!(cached().unwrap().get("leases").is_none());
        claim_work(&state, None, None, "crashed", 1, Some("a"), None).unwrap();
        claim_work(&state, None, None, "healthy", 60, Some("b"), None).unwrap();
        assert_eq!(cached().unwrap()["leases"].as_array().unwrap().len(), 2);
        std::thread::sleep(std::time::Duration::from_millis(1050));
        assert_eq!(cached().unwrap()["leases"].as_array().unwrap().len(), 1);
        assert!(heartbeat_lease(&state, "a", "crashed", 5).is_err());
        let replacement = claim_work(&state, None, None, "new", 30, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(replacement.lease.node_path, "a");
        assert!(complete_lease(&state, "a", "crashed", None).is_err());
        assert_eq!(read_active_leases(&state).len(), 2);
        let mut doc = live::json_file(&state, None).unwrap().0;
        doc["nodes"]["a"]["status"] = json!("passed");
        write_json(&state, &doc, false).unwrap();
        assert!(claim_work(&state, None, None, "other", 5, None, None)
            .unwrap()
            .is_none());
        let remaining = read_active_leases(&state);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].worker_id, "healthy");
    }

    #[cfg(unix)]
    #[test]
    fn workboard_rejects_packet_and_parent_symlinks_even_inside_root() {
        use std::os::unix::fs::symlink;
        let (_dir, state) = board(&["a"]);
        let nested = state.parent().unwrap();
        symlink(nested.join("packet-0.json"), nested.join("link.json")).unwrap();
        symlink(nested, nested.join("alias")).unwrap();
        for raw in ["link.json", "alias/packet-0.json"] {
            write_json(
                &state,
                &json!({"nodes":{"a":{"status":"awaiting_subagents","packet_path":raw}}}),
                false,
            )
            .unwrap();
            assert!(list_workboard(&state, None, None).is_err(), "{raw}");
        }
    }

    #[test]
    fn expired_and_superseded_claims_cannot_mutate_replacement() {
        let (_dir, state) = board(&["a"]);
        let claim = |worker| {
            claim_work(&state, None, None, worker, 60, Some("a"), None)
                .unwrap()
                .unwrap()
                .lease
        };
        let first = claim("one");
        let path = leases_path_for_state(&state);
        let mut leases = load_leases(&path).unwrap();
        leases.records.get_mut("a").unwrap().expires_at = "2000-01-01T00:00:00Z".into();
        write_json(&path, &leases, true).unwrap();
        for result in [
            complete_lease(&state, "a", "one", None),
            release_lease(&state, "a", "one", None),
        ] {
            assert!(result.unwrap_err().contains("expired"));
        }
        assert!(load_leases(&path).unwrap().records.is_empty());
        assert!(heartbeat_lease(&state, "a", "one", 60).is_err());
        let replacement = claim("one");
        assert!(replacement.generation > first.generation);
        for stale in [&first.worker_reference(), &first.worker_id] {
            assert!(heartbeat_lease(&state, "a", stale, 60).is_err());
            assert!(release_lease(&state, "a", stale, None).is_err());
            assert!(complete_lease(&state, "a", stale, None).is_err());
        }
        let current = replacement.worker_reference();
        assert_eq!(
            heartbeat_lease(&state, "a", &current, 60)
                .unwrap()
                .heartbeat_count,
            1
        );
        assert!(complete_lease(&state, "a", &current, None).unwrap());
        assert!(claim("two").generation > replacement.generation);
    }

    #[test]
    fn finalization_keeps_ownership_when_progress_cannot_be_written() {
        let (_dir, state) = board(&["a"]);
        let claim = claim_work(&state, None, None, "one", 60, None, None)
            .unwrap()
            .unwrap();
        let progress = live::progress_path_for_state(&state);
        let saved = std::fs::read(&progress).unwrap();
        std::fs::write(&progress, "{").unwrap();
        assert!(release_lease(&state, "a", "one", None).is_err());
        assert!(complete_lease(&state, "a", "one", None).is_err());
        assert_eq!(
            read_active_leases(&state)[0].generation,
            claim.lease.generation
        );
        std::fs::write(progress, saved).unwrap();
        assert!(complete_lease(&state, "a", "one", None).unwrap());
    }

    #[test]
    fn legacy_boards_migrate_without_resetting_ownership() {
        let (_dir, state) = board(&["a"]);
        let claim = claim_work(&state, None, None, "one", 60, None, None)
            .unwrap()
            .unwrap();
        let mut legacy = json!({"a":claim.lease});
        legacy["a"].as_object_mut().unwrap().remove("generation");
        write_json(&leases_path_for_state(&state), &legacy, true).unwrap();
        assert_eq!(
            heartbeat_lease(&state, "a", "one", 60).unwrap().generation,
            0
        );
        assert!(release_lease(&state, "a", "one", None).unwrap());
        let next = claim_work(&state, None, None, "two", 60, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(next.lease.generation, 1);
    }

    #[test]
    fn report_fallback_and_packet_validation() {
        let (dir, state) = board(&["a", "b"]);
        let report = state.with_file_name("report.json");
        write_json(&state,&json!({"nodes":{"a":{"status":"awaiting_subagents"},"b":{"status":"awaiting_subagents"}}}),false).unwrap();
        write_json(
            &state.with_file_name("packet-1.json"),
            &json!({"node_path":"b"}),
            false,
        )
        .unwrap();
        write_json(&report,&json!({"node_path":"a","packet_path":"packet-0.json","packet_paths":["packet-1.json","packet-0.json"],"telemetry_summary":{"nodes":2}}),false).unwrap();
        let packets = discover_waiting_packets(&state, Some(&report), None).unwrap();
        assert_eq!(
            packets.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        write_json(
            &state.with_file_name("packet-1.json"),
            &json!({"output_path":"b.json"}),
            false,
        )
        .unwrap();
        let legacy = discover_waiting_packets(&state, Some(&report), None).unwrap();
        assert_eq!(legacy[1].0, "b");
        assert_eq!(legacy[1].2["output_path"], "b.json");
        let snapshot = crate::snapshot::build_town_snapshot_from_graph(
            None,
            Some(&state),
            Some(&report),
            None,
            None,
            None,
            None,
        );
        assert_eq!(snapshot["telemetry_summary"], json!({"nodes":2}));
        assert!(snapshot.get("leases").is_none());
        let outside = dir.path().join("outside.json");
        write_json(&outside, &json!({}), false).unwrap();
        for raw in [outside.to_str().unwrap(), "../packet-0.json"] {
            write_json(
                &state,
                &json!({"nodes":{"a":{"status":"awaiting_subagents","packet_path":raw}}}),
                false,
            )
            .unwrap();
            assert!(discover_waiting_packets(&state, None, None).is_err());
        }
        write_json(
            &state,
            &json!({"nodes":{"a":{"status":"awaiting_subagents","packet_path":"large.json"}}}),
            false,
        )
        .unwrap();
        write_json(
            &state.with_file_name("large.json"),
            &json!({"data":"x".repeat(live::PACKET_LIMIT)}),
            false,
        )
        .unwrap();
        assert!(discover_waiting_packets(&state, None, None).is_err());
    }
}
