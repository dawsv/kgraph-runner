//! Versioned execution spans, locked JSONL history, Chrome traces and cross-run analytics.

use crate::{files, types::AgentUsage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeTelemetry {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub deps: Vec<String>,
    pub resource: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub start_offset_ms: f64,
    pub end_offset_ms: f64,
    pub duration_ms: f64,
    pub queue_wait_ms: f64,
    pub resource_wait_ms: f64,
    /// Actual waits, including child resources reserved by an exclusive container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_waits: Vec<ResourceWait>,
    pub cached: bool,
    pub cache_miss_reason: Option<String>,
    pub attempts: usize,
    pub error_signatures: Vec<String>,
    pub repaired: bool,
    pub circuit_broken: bool,
    pub agent_usage: Option<AgentUsage>,
    /// Preserves provider changes and failed-attempt spend across successful retries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempt_usage: Vec<AttemptUsage>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResourceWait {
    pub resource: String,
    pub start_offset_ms: f64,
    pub duration_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AttemptUsage {
    pub attempt: usize,
    pub failed: bool,
    pub usage: AgentUsage,
    /// Individual model invocations within this attempt, when available.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage_records: Vec<AgentUsage>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CriticalPath {
    pub nodes: Vec<String>,
    pub duration_ms: f64,
    pub share_of_wall_pct: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RunTelemetry {
    pub version: u32,
    pub run_id: String,
    pub workflow_ref: Option<String>,
    pub state_path: String,
    pub git_head: Option<String>,
    pub git_dirty: Option<bool>,
    pub started_at: String,
    pub finished_at: String,
    pub wall_time_ms: f64,
    pub total_work_ms: f64,
    pub parallelism_factor: f64,
    pub status: String,
    pub exit_code: i32,
    pub nodes_total: usize,
    pub nodes_passed: usize,
    pub nodes_cached: usize,
    pub nodes_failed: usize,
    pub cache_hit_rate_pct: f64,
    pub total_attempts: usize,
    pub repaired_nodes: usize,
    pub circuit_broken_nodes: usize,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub critical_path: CriticalPath,
    pub nodes: Vec<NodeTelemetry>,
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

fn failed(node: &NodeTelemetry) -> bool {
    node.circuit_broken
        || matches!(
            node.status.as_str(),
            "failed" | "circuit_broken" | "budget_exceeded"
        )
}

fn passed(node: &NodeTelemetry) -> bool {
    node.cached || node.status == "passed"
}

fn container(node: &NodeTelemetry) -> bool {
    matches!(
        node.kind.as_str(),
        "subgraph" | "loop_subgraph" | "splitter"
    )
}

impl RunTelemetry {
    pub(crate) fn summarize(&mut self) {
        self.nodes.sort_by(|a, b| {
            a.start_offset_ms
                .total_cmp(&b.start_offset_ms)
                .then(a.path.cmp(&b.path))
        });
        self.nodes_total = self.nodes.len();
        self.nodes_passed = self.nodes.iter().filter(|n| n.status == "passed").count();
        self.nodes_cached = self.nodes.iter().filter(|n| n.cached).count();
        self.nodes_failed = self.nodes.iter().filter(|n| failed(n)).count();
        self.total_attempts = self.nodes.iter().map(|n| n.attempts).sum();
        self.repaired_nodes = self.nodes.iter().filter(|n| n.repaired).count();
        self.circuit_broken_nodes = self.nodes.iter().filter(|n| n.circuit_broken).count();
        self.cache_hit_rate_pct = pct(
            self.nodes_cached,
            self.nodes
                .iter()
                .filter(|n| n.cached || n.cache_miss_reason.is_some())
                .count(),
        );
        self.total_work_ms = total_work_ms(&self.nodes);
        self.parallelism_factor = self.total_work_ms / self.wall_time_ms.max(0.001);
        let usages = self.nodes.iter().filter_map(|n| n.agent_usage.as_ref());
        self.total_tokens = usages
            .clone()
            .map(AgentUsage::total_tokens)
            .fold(0, u64::saturating_add);
        self.total_cost_usd = usages.map(|u| u.cost_usd).sum();
        self.critical_path = compute_critical_path(&self.nodes, self.wall_time_ms);
    }
}

fn span_ms(n: &NodeTelemetry) -> f64 {
    if n.duration_ms > 0.0 {
        n.duration_ms
    } else {
        (n.end_offset_ms - n.start_offset_ms).max(0.0)
    }
}

/// Sum task and container work without charging a parent for time already
/// attributed to its children. Input spans are in start order.
fn total_work_ms(nodes: &[NodeTelemetry]) -> f64 {
    let mut inclusive = 0.;
    let containers: HashSet<&str> = nodes
        .iter()
        .filter(|n| container(n))
        .map(|n| n.path.as_str())
        .collect();
    let mut covered_until = HashMap::new();
    let mut nested = 0.;
    for n in nodes {
        inclusive += span_ms(n);
        if let Some((parent, _)) = n.path.rsplit_once('/') {
            if containers.contains(parent) {
                let end = covered_until.entry(parent).or_insert(0.0_f64);
                nested += (n.end_offset_ms - n.start_offset_ms.max(*end)).max(0.0);
                *end = n.end_offset_ms.max(*end);
            }
        }
    }
    (inclusive - nested).max(0.)
}

/// Resolve sibling dependencies against actual executions. For repeated loop paths, only
/// the latest completed occurrence can satisfy a dependency in a later iteration.
fn dependency_edges(nodes: &[&NodeTelemetry]) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut indices: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        indices.entry(&n.path).or_default().push(i);
    }
    for occurrences in indices.values_mut() {
        occurrences.sort_by(|a, b| nodes[*a].end_offset_ms.total_cmp(&nodes[*b].end_offset_ms));
    }
    let mut edges = vec![Vec::new(); nodes.len()];
    let mut indegree = vec![0; nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        for dep in &n.deps {
            let sibling = n
                .path
                .rsplit_once('/')
                .map(|(prefix, _)| format!("{prefix}/{dep}"));
            let Some(matches) = sibling
                .as_ref()
                .and_then(|p| indices.get(p.as_str()))
                .or_else(|| {
                    (sibling.is_none() || dep.contains('/'))
                        .then(|| indices.get(dep.as_str()))
                        .flatten()
                })
            else {
                continue;
            };
            let parent = if matches.len() == 1 {
                matches.first()
            } else {
                let k = matches.partition_point(|j| nodes[*j].end_offset_ms <= n.start_offset_ms);
                k.checked_sub(1).map(|k| &matches[k])
            };
            if let Some(&j) = parent.filter(|&&j| j != i) {
                edges[j].push(i);
                indegree[i] += 1;
            }
        }
    }
    (edges, indegree)
}

fn longest_chain<'a>(nodes: &[&'a NodeTelemetry]) -> (Vec<&'a NodeTelemetry>, f64) {
    let (edges, mut indegree) = dependency_edges(nodes);
    let mut ready: VecDeque<_> = (0..nodes.len()).filter(|&i| indegree[i] == 0).collect();
    let mut lengths: Vec<_> = nodes.iter().map(|n| span_ms(n)).collect();
    let mut previous = vec![None; nodes.len()];
    let mut terminal: Option<usize> = None;
    while let Some(i) = ready.pop_front() {
        if edges[i].is_empty() && terminal.is_none_or(|t| lengths[i] > lengths[t]) {
            terminal = Some(i);
        }
        for &j in &edges[i] {
            let candidate = lengths[i] + span_ms(nodes[j]);
            if previous[j].is_none() || candidate > lengths[j] {
                lengths[j] = candidate;
                previous[j] = Some(i);
            }
            indegree[j] -= 1;
            if indegree[j] == 0 {
                ready.push_back(j);
            }
        }
    }
    let Some(mut i) =
        terminal.or_else(|| (0..nodes.len()).max_by(|&a, &b| lengths[a].total_cmp(&lengths[b])))
    else {
        return (Vec::new(), 0.0);
    };
    let duration = lengths[i];
    let mut chain = Vec::new();
    loop {
        chain.push(nodes[i]);
        let Some(p) = previous[i] else { break };
        i = p;
    }
    chain.reverse();
    (chain, duration)
}

fn expand_chain(
    chain: &[&NodeTelemetry],
    children: &HashMap<&str, Vec<&NodeTelemetry>>,
    repeated: &HashSet<&str>,
    output: &mut Vec<String>,
) {
    for node in chain {
        let nested = children.get(node.path.as_str()).filter(|_| container(node));
        let Some(nested) = nested else {
            output.push(node.path.clone());
            continue;
        };
        let mut group: Vec<_> = nested
            .iter()
            .copied()
            .filter(|child| {
                !repeated.contains(node.path.as_str())
                    || (child.start_offset_ms >= node.start_offset_ms
                        && child.end_offset_ms <= node.end_offset_ms)
            })
            .collect();
        if group.is_empty() {
            output.push(node.path.clone());
        } else if node.kind == "loop_subgraph" && node.attempts > 1 {
            // Each iteration executes the same direct children before verification.
            group.sort_by(|a, b| a.start_offset_ms.total_cmp(&b.start_offset_ms));
            let mut occurrences = HashMap::new();
            let mut iterations: BTreeMap<usize, Vec<_>> = BTreeMap::new();
            for child in group {
                let index = occurrences.entry(&child.path).or_insert(0);
                iterations.entry(*index).or_default().push(child);
                *index += 1;
            }
            for iteration in iterations.values() {
                expand_chain(&longest_chain(iteration).0, children, repeated, output);
            }
        } else {
            expand_chain(&longest_chain(&group).0, children, repeated, output);
        }
    }
}

/// Rank dependencies using inclusive container durations. Expand winning
/// containers into their internal blocking chains for leaf attribution, while
/// retaining the original duration (including container overhead).
pub fn compute_critical_path(nodes: &[NodeTelemetry], wall_time_ms: f64) -> CriticalPath {
    let mut containers = HashSet::new();
    let mut repeated = HashSet::new();
    for n in nodes.iter().filter(|n| container(n)) {
        if !containers.insert(n.path.as_str()) {
            repeated.insert(n.path.as_str());
        }
    }
    let mut children: HashMap<&str, Vec<_>> = HashMap::new();
    let mut roots = Vec::new();
    for n in nodes {
        let mut path = n.path.as_str();
        let parent = loop {
            let Some((prefix, _)) = path.rsplit_once('/') else {
                break None;
            };
            if containers.contains(prefix) {
                break Some(prefix);
            }
            path = prefix;
        };
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(n);
        } else {
            roots.push(n);
        }
    }
    let (chain, duration_ms) = longest_chain(&roots);
    let mut paths = Vec::new();
    expand_chain(&chain, &children, &repeated, &mut paths);
    CriticalPath {
        nodes: paths,
        duration_ms,
        share_of_wall_pct: if wall_time_ms > 0.0 {
            100.0 * duration_ms / wall_time_ms
        } else {
            0.0
        },
    }
}

fn history_lock(path: &Path) -> Result<files::FileLock, String> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    files::file_lock(&PathBuf::from(name))
}

// Locate an unterminated final record without rereading the whole history.
fn prepare_append(file: &mut File, bytes: &mut Vec<u8>) -> std::io::Result<u64> {
    let len = file.metadata()?.len();
    let mut end = len;
    let mut block = [0; 8192];
    while end > 0 {
        let start = end.saturating_sub(block.len() as u64);
        let chunk = &mut block[..(end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(chunk)?;
        if let Some(pos) = chunk.iter().rposition(|&b| b == b'\n') {
            end = start + pos as u64 + 1;
            break;
        }
        end = start;
    }
    if end == len {
        return Ok(len);
    }
    file.seek(SeekFrom::Start(end))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    if serde_json::from_slice::<RunTelemetry>(&tail).is_ok() {
        bytes.insert(0, b'\n');
        Ok(len)
    } else {
        file.set_len(end)?;
        Ok(end)
    }
}

pub fn append_run_telemetry(path: &Path, run: &RunTelemetry) -> Result<(), String> {
    let path = files::resolve_path(path)?;
    let mut bytes = serde_json::to_vec(run).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let _guard = history_lock(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let old_len =
        prepare_append(&mut file, &mut bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Err(e) = file.write_all(&bytes).and_then(|()| files::sync_file(&file)) {
        file.set_len(old_len)
            .map_err(|rollback| format!("{}: {e}; rollback: {rollback}", path.display()))?;
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(())
}

pub fn read_telemetry_runs(
    path: &Path,
    workflow_filter: Option<&str>,
    last: Option<usize>,
) -> Result<Vec<RunTelemetry>, String> {
    let path = files::resolve_path(path)?;
    let file = match File::open(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.map_err(|e| format!("{}: {e}", path.display()))?,
    };
    let _guard = history_lock(&path)?;
    let mut result = VecDeque::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let error = |e: String| format!("{} line {}: {e}", path.display(), i + 1);
        let line = line.map_err(|e| error(e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let run: RunTelemetry = serde_json::from_str(&line).map_err(|e| error(e.to_string()))?;
        if workflow_filter.is_some() && workflow_filter != run.workflow_ref.as_deref() {
            continue;
        }
        result.push_back(run);
        if last.is_some_and(|limit| result.len() > limit) {
            result.pop_front();
        }
    }
    Ok(result.into())
}

pub fn export_chrome_trace(run: &RunTelemetry) -> Value {
    let mut events = Vec::new();
    for (lane, n) in run.nodes.iter().enumerate() {
        let complete = |name: &str, cat: &str, start: f64, duration: f64, args: Value| json!({"ph":"X","pid":1,"tid":lane+1,"name":name,"cat":cat,"ts":start*1000.0,"dur":duration*1000.0,"args":args});
        for wait in &n.resource_waits {
            events.push(complete(
                "resource_wait",
                "resource_wait",
                wait.start_offset_ms,
                wait.duration_ms,
                json!({"node":n.path,"resource":wait.resource}),
            ));
        }
        if n.resource_waits.is_empty() && n.resource_wait_ms > 0.0 {
            events.push(complete(
                "resource_wait",
                "resource_wait",
                (n.start_offset_ms - n.resource_wait_ms).max(0.0),
                n.resource_wait_ms,
                json!({"node":n.path,"resource":n.resource}),
            ));
        }
        events.push(complete(&n.path,&n.kind,n.start_offset_ms,span_ms(n),
            json!({"status":n.status,"cached":n.cached,"attempts":n.attempts,"cost_usd":n.agent_usage.as_ref().map_or(0.0,|u| u.cost_usd),"resource":n.resource,"resource_wait_ms":n.resource_wait_ms})));
    }
    json!({"traceEvents":events,"displayTimeUnit":"ms"})
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TelemetryAnalysis {
    pub runs_total: usize,
    pub runs_passed: usize,
    pub pass_rate_pct: f64,
    /// Percentage of runs that passed without a retry.
    pub one_shot_pass_rate_pct: f64,
    pub repair_rate_pct: f64,
    pub circuit_break_rate_pct: f64,
    pub avg_wall_time_ms: f64,
    pub p50_wall_time_ms: f64,
    pub p90_wall_time_ms: f64,
    pub avg_parallelism_factor: f64,
    pub cache_hit_rate_pct: f64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub failure_burn_tokens: u64,
    pub failure_burn_usd: f64,
    pub nodes: BTreeMap<String, NodeAnalytics>,
    pub models: BTreeMap<String, ModelAnalytics>,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeAnalytics {
    pub executions: usize,
    pub pass_rate_pct: f64,
    pub cached_count: usize,
    pub cache_hit_rate_pct: f64,
    pub miss_reasons: BTreeMap<String, usize>,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub avg_ms: f64,
    pub avg_resource_wait_ms: f64,
    pub critical_path_pct: f64,
    pub flakiness_score: f64,
    pub top_error_signatures: Vec<(String, usize)>,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub failure_burn_tokens: u64,
    pub failure_burn_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelAnalytics {
    pub calls: usize,
    pub tokens: u64,
    pub cost_usd: f64,
    pub failure_burn_usd: f64,
    pub failure_burn_tokens: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recommendation {
    pub severity: String,
    pub category: String,
    pub node: Option<String>,
    pub message: String,
    pub impact: String,
}

impl TelemetryAnalysis {
    fn recommend(
        &mut self,
        severity: &str,
        category: &str,
        node: Option<&str>,
        message: String,
        impact: String,
    ) {
        self.recommendations.push(Recommendation {
            severity: severity.into(),
            category: category.into(),
            node: node.map(str::to_owned),
            message,
            impact,
        });
    }
}

#[derive(Default)]
struct Samples {
    latencies: Vec<f64>,
    wait_ms: f64,
    critical_runs: usize,
    passes: usize,
    repairs: usize,
    transitions: usize,
    outcomes: usize,
    previous_pass: Option<bool>,
    signatures: BTreeMap<String, usize>,
    resources: BTreeMap<String, f64>,
    stats: NodeAnalytics,
}

fn most_common(counts: &BTreeMap<String, usize>) -> Option<&str> {
    counts
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(s, _)| s.as_str())
}

// Linear interpolation between adjacent order statistics, including small samples.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pos = q * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    sorted[lo] + pos.fract() * (sorted[pos.ceil() as usize] - sorted[lo])
}

fn account_usage(
    models: &mut BTreeMap<String, ModelAnalytics>,
    stats: &mut NodeAnalytics,
    u: &AgentUsage,
    calls: usize,
    lost: bool,
) {
    let model = format!(
        "{}/{}",
        u.provider.as_deref().unwrap_or("unknown"),
        u.model.as_deref().unwrap_or("unknown")
    );
    let m = models.entry(model).or_default();
    let tokens = u.total_tokens();
    m.calls += calls;
    m.tokens = m.tokens.saturating_add(tokens);
    m.cost_usd += u.cost_usd;
    stats.total_tokens = stats.total_tokens.saturating_add(tokens);
    stats.total_cost_usd += u.cost_usd;
    if lost {
        m.failure_burn_tokens = m.failure_burn_tokens.saturating_add(tokens);
        m.failure_burn_usd += u.cost_usd;
        stats.failure_burn_tokens = stats.failure_burn_tokens.saturating_add(tokens);
        stats.failure_burn_usd += u.cost_usd;
    }
}

fn significant_burn(tokens: u64, cost: f64, total_tokens: u64, total_cost: f64) -> bool {
    tokens >= 1000
        || cost >= 0.1
        || (cost > 0.0 && cost >= 0.25 * total_cost)
        || (tokens > 0 && tokens as f64 >= 0.25 * total_tokens as f64)
}

// Independent work exists when no dependency chain contains every non-cached
// sibling. Preserve edges through cached nodes and collapse repeated loop paths.
fn independent_work(nodes: &[NodeTelemetry]) -> bool {
    let mut scopes: HashMap<&str, HashMap<&str, &NodeTelemetry>> = HashMap::new();
    for n in nodes {
        let (scope, id) = n.path.rsplit_once('/').unwrap_or(("", &n.path));
        scopes
            .entry(scope)
            .or_default()
            .entry(id)
            .and_modify(|prev| {
                if prev.cached {
                    *prev = n;
                }
            })
            .or_insert(n);
    }
    scopes.values().any(|group| {
        let total = group.values().filter(|n| !n.cached).count();
        if total < 2 {
            return false;
        }
        let mut remaining = HashMap::new();
        let mut depth = HashMap::new();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut ready = Vec::new();
        for (&id, n) in group {
            let deps = n.deps.iter().filter(|d| group.contains_key(d.as_str()));
            let count = deps.clone().count();
            remaining.insert(id, count);
            depth.insert(id, usize::from(!n.cached));
            if count == 0 {
                ready.push(id);
            }
            for dep in deps {
                children.entry(dep).or_default().push(id);
            }
        }
        let mut visited = 0;
        let mut longest = 0;
        while let Some(id) = ready.pop() {
            visited += 1;
            let chain = depth[id];
            longest = longest.max(chain);
            for child in children.get(id).into_iter().flatten() {
                let length = depth.get_mut(child).unwrap();
                *length = (*length).max(chain + usize::from(!group[child].cached));
                let count = remaining.get_mut(child).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.push(child);
                }
            }
        }
        visited == group.len() && longest < total
    })
}

pub fn analyze_telemetry(runs: &[RunTelemetry]) -> TelemetryAnalysis {
    let mut a = TelemetryAnalysis {
        runs_total: runs.len(),
        ..Default::default()
    };
    let mut samples: BTreeMap<&str, Samples> = BTreeMap::new();
    let mut walls = Vec::new();
    let (mut one_shot, mut repaired, mut broken, mut hits, mut lookups, mut underused) =
        (0, 0, 0, 0, 0, 0);
    for run in runs {
        let ok = run.status == "passed";
        a.runs_passed += usize::from(ok);
        one_shot += usize::from(ok && run.nodes.iter().all(|n| n.attempts <= 1 && !n.repaired));
        repaired += usize::from(run.nodes.iter().any(|n| n.repaired));
        broken += usize::from(
            run.status == "circuit_broken" || run.nodes.iter().any(|n| n.circuit_broken),
        );
        a.total_tokens = a.total_tokens.saturating_add(run.total_tokens);
        a.total_cost_usd += run.total_cost_usd;
        a.avg_parallelism_factor += run.parallelism_factor;
        walls.push(run.wall_time_ms);
        if run.parallelism_factor < 1.25 && independent_work(&run.nodes) {
            underused += 1;
        }
        let mut critical: HashSet<&str> =
            run.critical_path.nodes.iter().map(String::as_str).collect();
        for n in &run.nodes {
            let s = samples.entry(&n.path).or_default();
            s.latencies.push(span_ms(n));
            s.wait_ms += n.resource_wait_ms;
            for wait in &n.resource_waits {
                *s.resources.entry(wait.resource.clone()).or_default() += wait.duration_ms;
            }
            if n.resource_waits.is_empty() && n.resource_wait_ms > 0.0 {
                if let Some(r) = &n.resource {
                    *s.resources.entry(r.clone()).or_default() += n.resource_wait_ms;
                }
            }
            s.passes += usize::from(passed(n));
            s.repairs += usize::from(n.repaired);
            s.critical_runs += usize::from(critical.remove(n.path.as_str()));
            if !n.cached && (passed(n) || failed(n)) {
                let ok = passed(n);
                s.transitions += usize::from(s.previous_pass.is_some_and(|prev| prev != ok));
                s.previous_pass = Some(ok);
                s.outcomes += 1;
            }
            hits += usize::from(n.cached);
            lookups += usize::from(n.cached || n.cache_miss_reason.is_some());
            s.stats.cached_count += usize::from(n.cached);
            if let Some(reason) = &n.cache_miss_reason {
                *s.stats.miss_reasons.entry(reason.clone()).or_default() += 1;
            }
            for sig in &n.error_signatures {
                *s.signatures.entry(sig.clone()).or_default() += 1;
            }
            if n.attempt_usage.is_empty() {
                if let Some(u) = &n.agent_usage {
                    account_usage(&mut a.models, &mut s.stats, u, n.attempts.max(1), failed(n));
                }
            } else {
                for attempt in &n.attempt_usage {
                    let records = if attempt.usage_records.is_empty() {
                        std::slice::from_ref(&attempt.usage)
                    } else {
                        &attempt.usage_records
                    };
                    for usage in records {
                        account_usage(
                            &mut a.models,
                            &mut s.stats,
                            usage,
                            1,
                            failed(n) || attempt.failed,
                        );
                    }
                }
            }
        }
    }
    walls.sort_by(f64::total_cmp);
    a.pass_rate_pct = pct(a.runs_passed, runs.len());
    a.one_shot_pass_rate_pct = pct(one_shot, runs.len());
    a.repair_rate_pct = pct(repaired, runs.len());
    a.circuit_break_rate_pct = pct(broken, runs.len());
    a.cache_hit_rate_pct = pct(hits, lookups);
    a.avg_parallelism_factor /= runs.len().max(1) as f64;
    a.avg_wall_time_ms = walls.iter().sum::<f64>() / runs.len().max(1) as f64;
    a.p50_wall_time_ms = percentile(&walls, 0.5);
    a.p90_wall_time_ms = percentile(&walls, 0.9);
    for (path, mut s) in samples {
        let n = &mut s.stats;
        s.latencies.sort_by(f64::total_cmp);
        n.executions = s.latencies.len();
        let count = n.executions as f64;
        n.pass_rate_pct = pct(s.passes, n.executions);
        n.cache_hit_rate_pct = pct(
            n.cached_count,
            n.cached_count + n.miss_reasons.values().sum::<usize>(),
        );
        n.avg_ms = s.latencies.iter().sum::<f64>() / count;
        n.p50_ms = percentile(&s.latencies, 0.5);
        n.p90_ms = percentile(&s.latencies, 0.9);
        n.p99_ms = percentile(&s.latencies, 0.99);
        n.max_ms = *s.latencies.last().unwrap();
        n.avg_resource_wait_ms = s.wait_ms / count;
        n.critical_path_pct = pct(s.critical_runs, runs.len());
        n.flakiness_score = (s.repairs as f64 / count
            + s.transitions as f64 / s.outcomes.saturating_sub(1).max(1) as f64)
            .min(1.0);
        n.top_error_signatures = s.signatures.into_iter().collect();
        n.top_error_signatures
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        n.top_error_signatures.truncate(5);
        a.failure_burn_tokens = a.failure_burn_tokens.saturating_add(n.failure_burn_tokens);
        a.failure_burn_usd += n.failure_burn_usd;
        if n.critical_path_pct >= 50.0
            && n.p50_ms >= 10.0
            && (n.p50_ms >= 1000.0 || n.p50_ms >= 0.25 * a.p50_wall_time_ms)
        {
            a.recommend(
                "high",
                "critical_path",
                Some(path),
                format!("Profile {path} before optimizing off-path work."),
                format!(
                    "Median {:.1} ms, critical in {:.0}% of runs",
                    n.p50_ms, n.critical_path_pct
                ),
            );
        }
        if let Some(reason) = most_common(&n.miss_reasons) {
            if n.executions >= 3 && n.cache_hit_rate_pct < 25.0 {
                a.recommend(
                    "medium",
                    "cache_thrash",
                    Some(path),
                    format!(
                        "Inspect caching for {path}; the most frequent miss reason is {reason}."
                    ),
                    format!(
                        "{:.1}% cache hits in {} executions",
                        n.cache_hit_rate_pct, n.executions
                    ),
                );
            }
        }
        if n.flakiness_score >= 0.25 {
            let sig = n
                .top_error_signatures
                .first()
                .map_or("unclassified", |v| v.0.as_str());
            a.recommend(
                "high",
                "flakiness",
                Some(path),
                format!("Reproduce retries and outcome changes for {path}, starting with {sig}."),
                format!(
                    "{} repairs, {} pass/fail transitions",
                    s.repairs, s.transitions
                ),
            );
        }
        if n.avg_resource_wait_ms >= 100.0
            || (n.avg_resource_wait_ms >= 10.0 && n.avg_resource_wait_ms >= 0.2 * n.avg_ms)
        {
            a.recommend(
                "medium",
                "resource_contention",
                Some(path),
                format!(
                    "Shorten the exclusive section for {} in {path}.",
                    s.resources
                        .iter()
                        .max_by(|a, b| a.1.total_cmp(b.1))
                        .map_or("the shared resource", |(name, _)| name.as_str())
                ),
                format!("Mean lock wait {:.1} ms", n.avg_resource_wait_ms),
            );
        }
        if significant_burn(
            n.failure_burn_tokens,
            n.failure_burn_usd,
            n.total_tokens,
            n.total_cost_usd,
        ) {
            a.recommend(
                "high",
                "failure_burn",
                Some(path),
                format!("Review failed agent calls for {path} before retrying."),
                format!(
                    "{} tokens and ${:.4} spent on failures",
                    n.failure_burn_tokens, n.failure_burn_usd
                ),
            );
        }
        a.nodes.insert(path.into(), s.stats);
    }
    let model_burn: Vec<_> = a
        .models
        .iter()
        .filter(|(_, m)| {
            significant_burn(
                m.failure_burn_tokens,
                m.failure_burn_usd,
                m.tokens,
                m.cost_usd,
            )
        })
        .map(|(name, m)| (name.clone(), m.failure_burn_tokens, m.failure_burn_usd))
        .collect();
    for (model, tokens, cost) in model_burn {
        a.recommend(
            "high",
            "failure_burn",
            None,
            format!("Review prompts and retry limits for {model}."),
            format!("{tokens} failed tokens, ${cost:.4}"),
        );
    }
    if underused > 0 {
        a.recommend(
            "low",
            "parallelism",
            None,
            "Check worker limits and exclusive resources for independent siblings.".into(),
            format!("{underused} runs had independent work and parallelism below 1.25"),
        );
    }
    a
}

fn render(a: &TelemetryAnalysis, markdown: bool) -> String {
    let heading = |s: &str| {
        if markdown {
            format!("## {s}")
        } else {
            s.to_owned()
        }
    };
    let mut out = format!("{}\n\nRuns: {} | Passed: {:.1}% | One-shot: {:.1}% | Repaired: {:.1}% | Circuit breaks: {:.1}%\nWall time (avg/p50/p90): {:.1}/{:.1}/{:.1} ms | Parallelism: {:.2} | Cache hits: {:.1}%\nAgent usage: {} tokens, ${:.4} | Failure burn: {} tokens, ${:.4}\n",
        heading("Execution telemetry"),a.runs_total,a.pass_rate_pct,a.one_shot_pass_rate_pct,a.repair_rate_pct,a.circuit_break_rate_pct,a.avg_wall_time_ms,a.p50_wall_time_ms,a.p90_wall_time_ms,a.avg_parallelism_factor,a.cache_hit_rate_pct,a.total_tokens,a.total_cost_usd,a.failure_burn_tokens,a.failure_burn_usd);
    let _ = writeln!(out, "\n{}\n", heading("Nodes"));
    for (path, n) in &a.nodes {
        let _ = writeln!(out,"- {path}: {} executions, {:.1}% passed, p50/p90/p99 {:.1}/{:.1}/{:.1} ms, lock wait {:.1} ms, {:.1}% critical, {:.1}% cache hits",n.executions,n.pass_rate_pct,n.p50_ms,n.p90_ms,n.p99_ms,n.avg_resource_wait_ms,n.critical_path_pct,n.cache_hit_rate_pct);
    }
    let _ = writeln!(out, "\n{}\n", heading("Models"));
    for (name, m) in &a.models {
        let _ = writeln!(
            out,
            "- {name}: {} calls, {} tokens, ${:.4}, failure burn ${:.4}",
            m.calls, m.tokens, m.cost_usd, m.failure_burn_usd
        );
    }
    let _ = writeln!(out, "\n{}\n", heading("Recommendations"));
    if a.recommendations.is_empty() {
        out.push_str("No recommendations.\n");
    }
    for r in &a.recommendations {
        let _ = writeln!(out, "- [{}] {} {}", r.severity, r.message, r.impact);
    }
    out
}

pub fn render_analysis_text(analysis: &TelemetryAnalysis) -> String {
    render(analysis, false)
}
pub fn render_analysis_markdown(analysis: &TelemetryAnalysis) -> String {
    render(analysis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::{Command, Stdio},
    };
    use tempfile::tempdir;

    fn node(path: &str, duration_ms: f64, deps: &[&str]) -> NodeTelemetry {
        NodeTelemetry {
            path: path.into(),
            kind: "worker".into(),
            status: "passed".into(),
            deps: deps.iter().map(|d| (*d).into()).collect(),
            duration_ms,
            attempts: 1,
            cache_miss_reason: Some("no_prior_state".into()),
            ..Default::default()
        }
    }

    #[test]
    fn diamond_unsorted_and_scoped_dependencies() {
        let nodes = vec![
            node("stage/join", 20., &["left", "right"]),
            node("stage/right", 70., &["root"]),
            node("root", 90., &[]),
            node("stage/left", 40., &["root"]),
            node("stage/root", 10., &[]),
        ];
        let path = compute_critical_path(&nodes, 125.);
        assert_eq!(path.nodes, ["stage/root", "stage/right", "stage/join"]);
        assert_eq!(path.duration_ms, 100.);
        assert_eq!(path.share_of_wall_pct, 80.);
        let partial = NodeTelemetry {
            start_offset_ms: 4.,
            end_offset_ms: 12.,
            ..node("alone", 0., &["absent"])
        };
        assert_eq!(compute_critical_path(&[partial], 0.).duration_ms, 8.);
        assert_eq!(compute_critical_path(&[], 0.), CriticalPath::default());
    }

    #[test]
    fn nested_parallel_work_counts_overlapping_children_and_exclusive_parent_time() {
        let mut nodes = vec![
            node("stage", 100., &[]),
            node("stage/a", 60., &[]),
            node("stage/b", 75., &[]),
        ];
        nodes[0].kind = "subgraph".into();
        nodes[1].start_offset_ms = 10.;
        nodes[2].start_offset_ms = 20.;
        for n in &mut nodes {
            n.end_offset_ms = n.start_offset_ms + n.duration_ms;
        }
        let mut run = RunTelemetry {
            wall_time_ms: 100.,
            nodes,
            ..Default::default()
        };
        run.summarize();
        // The container contributes 10 ms before its children and 5 ms after them.
        assert_eq!(run.total_work_ms, 150.);
        assert_eq!(run.parallelism_factor, 1.5);
    }

    #[test]
    fn nested_critical_path_attributes_winning_leaves_without_double_counting() {
        for kind in ["subgraph", "loop_subgraph", "splitter"] {
            let mut stage = node("stage", 90., &["prepare"]);
            stage.kind = kind.into();
            let mut run = RunTelemetry {
                wall_time_ms: 110.,
                nodes: vec![
                    node("prepare", 10., &[]),
                    stage,
                    node("stage/slow_child", 80., &[]),
                    node("stage/fast_child", 15., &[]),
                    node("publish", 10., &["stage"]),
                ],
                ..Default::default()
            };
            for winner in ["stage/slow_child", "stage/slow_child/leaf"] {
                if winner.ends_with("/leaf") {
                    run.nodes
                        .iter_mut()
                        .find(|n| n.path == "stage/slow_child")
                        .unwrap()
                        .kind = "subgraph".into();
                    run.nodes.push(node(winner, 75., &[]));
                }
                run.summarize();
                assert_eq!(run.critical_path.nodes, ["prepare", winner, "publish"]);
                assert_eq!(run.critical_path.duration_ms, 110.);
                let analysis = analyze_telemetry(std::slice::from_ref(&run));
                assert_eq!(analysis.nodes[winner].critical_path_pct, 100.);
                assert_eq!(analysis.nodes["stage/fast_child"].critical_path_pct, 0.);
                assert!(analysis
                    .recommendations
                    .iter()
                    .any(|r| r.category == "critical_path" && r.node.as_deref() == Some(winner)));
            }
        }
    }

    #[test]
    fn repeated_paths_bind_to_the_completed_iteration() {
        let mut first = node("loop/a", 75., &[]);
        first.end_offset_ms = 75.;
        let mut second = node("loop/a", 15., &[]);
        second.start_offset_ms = 80.;
        second.end_offset_ms = 95.;
        let mut b = node("loop/b", 70., &["a"]);
        b.start_offset_ms = 95.;
        let path = compute_critical_path(&[first, second, b], 165.);
        assert_eq!(path.duration_ms, 85.);
        assert_eq!(path.nodes, ["loop/a", "loop/b"]);
    }

    #[test]
    fn jsonl_process_append_filter_and_last() {
        const KEY: &str = "KGRAPH_TEST_TELEMETRY_FILE";
        if let Some(path) = std::env::var_os(KEY) {
            let workflow = std::env::var("KGRAPH_TEST_TELEMETRY_WORKFLOW").unwrap();
            for i in 0..8 {
                append_run_telemetry(
                    Path::new(&path),
                    &RunTelemetry {
                        version: 1,
                        run_id: format!("{workflow}-{i}"),
                        workflow_ref: Some(workflow.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            return;
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("history/events.jsonl");
        assert!(read_telemetry_runs(&path, None, None).unwrap().is_empty());
        assert!(!path.parent().unwrap().exists());
        let mut children: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "telemetry::tests::jsonl_process_append_filter_and_last",
                    ])
                    .env(KEY, &path)
                    .env("KGRAPH_TEST_TELEMETRY_WORKFLOW", name)
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(read_telemetry_runs(&path, None, None).unwrap().len(), 24);
        let latest = read_telemetry_runs(&path, Some("a"), Some(2)).unwrap();
        assert_eq!(
            latest.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            ["a-6", "a-7"]
        );
        assert!(read_telemetry_runs(&path, Some("missing"), None)
            .unwrap()
            .is_empty());
        assert!(read_telemetry_runs(&path, None, Some(0))
            .unwrap()
            .is_empty());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b" \n\n{broken\n").unwrap();
        assert!(read_telemetry_runs(&path, None, None)
            .unwrap_err()
            .contains("line 27"));
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 27);
    }

    #[test]
    fn append_discards_only_the_torn_unterminated_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let before = RunTelemetry {
            run_id: "before".into(),
            ..Default::default()
        };
        let after = RunTelemetry {
            run_id: "after".into(),
            ..Default::default()
        };
        append_run_telemetry(&path, &before).unwrap();
        let mut torn = br#"{"run_id":""#.to_vec();
        torn.extend(vec![b'x'; 16384]);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&torn)
            .unwrap();
        append_run_telemetry(&path, &after).unwrap();
        assert_eq!(
            read_telemetry_runs(&path, None, None).unwrap(),
            [before, after.clone()]
        );
        fs::write(&path, torn).unwrap();
        append_run_telemetry(&path, &after).unwrap();
        assert_eq!(read_telemetry_runs(&path, None, None).unwrap(), [after]);
    }

    #[test]
    fn append_preserves_an_imported_final_line_without_newline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history");
        let run = RunTelemetry {
            version: 1,
            ..Default::default()
        };
        fs::write(&path, serde_json::to_vec(&run).unwrap()).unwrap();
        append_run_telemetry(&path, &run).unwrap();
        assert_eq!(
            read_telemetry_runs(&path, None, None).unwrap(),
            [run.clone(), run]
        );
    }

    #[test]
    fn chrome_trace_reports_microseconds_and_separate_wait_spans() {
        let n = NodeTelemetry {
            start_offset_ms: 12.5,
            resource_wait_ms: 7.5,
            resource: Some("gpu".into()),
            ..node("build", 25., &[])
        };
        let trace = export_chrome_trace(&RunTelemetry {
            nodes: vec![n, node("lint", 1., &[])],
            ..Default::default()
        });
        let events = trace["traceEvents"].as_array().unwrap();
        assert_eq!(trace["displayTimeUnit"], "ms");
        assert_eq!(events.len(), 3);
        assert!(events
            .iter()
            .all(|e| e["ph"] == "X" && e["pid"].is_number() && e["tid"].is_number()));
        assert_eq!(events[0]["name"], "resource_wait");
        assert_eq!(events[0]["ts"], 5000.);
        assert_eq!(events[0]["dur"], 7500.);
        assert_eq!(events[1]["ts"], 12500.);
        assert_eq!(events[1]["dur"], 25000.);
        assert_eq!(events[0]["tid"], events[1]["tid"]);
        assert_eq!(events[1]["args"]["resource_wait_ms"], 7.5);
        assert_eq!(events[1]["args"]["cached"], false);
    }

    #[test]
    fn analytics_attribute_burn_and_recommend_specific_changes() {
        let mut runs = Vec::new();
        for i in 0..4 {
            let mut n = node("build", 1000.0 + 100. * i as f64, &[]);
            n.agent_usage = Some(AgentUsage {
                provider: Some("local".into()),
                model: Some("swift".into()),
                input_tokens: 900,
                output_tokens: 100,
                cost_usd: 0.5,
                ..Default::default()
            });
            n.resource = Some("gpu".into());
            n.resource_wait_ms = 250.;
            n.cache_miss_reason = Some("input_hash_changed".into());
            if i == 1 {
                n.status = "circuit_broken".into();
                n.circuit_broken = true;
                n.attempts = 2;
                n.error_signatures = vec!["invalid".into(), "invalid".into()];
            }
            if i == 2 {
                n.attempts = 2;
                n.repaired = true;
                n.error_signatures.push("invalid".into());
                let u = AgentUsage {
                    input_tokens: 450,
                    output_tokens: 50,
                    cost_usd: 0.25,
                    ..n.agent_usage.clone().unwrap()
                };
                n.attempt_usage = vec![
                    AttemptUsage {
                        attempt: 1,
                        usage_records: vec![],
                        failed: true,
                        usage: AgentUsage {
                            model: Some("retry-model".into()),
                            ..u.clone()
                        },
                    },
                    AttemptUsage {
                        attempt: 2,
                        usage_records: vec![],
                        failed: false,
                        usage: u,
                    },
                ];
            }
            let mut run = RunTelemetry {
                status: if i == 1 { "circuit_broken" } else { "passed" }.into(),
                wall_time_ms: 1500.,
                nodes: vec![n, node("audit", 20., &[])],
                ..Default::default()
            };
            run.summarize();
            runs.push(run);
        }
        let a = analyze_telemetry(&runs);
        assert_eq!(a.runs_total, 4);
        assert_eq!(a.pass_rate_pct, 75.);
        assert_eq!(a.one_shot_pass_rate_pct, 50.);
        assert_eq!(a.repair_rate_pct, 25.);
        assert_eq!(a.circuit_break_rate_pct, 25.);
        assert_eq!(a.total_tokens, 4000);
        assert_eq!(a.failure_burn_tokens, 1500);
        assert_eq!(a.failure_burn_usd, 0.75);
        assert_eq!(a.models["local/swift"].calls, 5);
        assert_eq!(a.models["local/retry-model"].failure_burn_usd, 0.25);
        let n = &a.nodes["build"];
        assert_eq!(n.p50_ms, 1150.);
        assert_eq!(n.p90_ms, 1270.);
        assert_eq!(n.p99_ms, 1297.);
        assert_eq!(n.avg_resource_wait_ms, 250.);
        assert_eq!(n.critical_path_pct, 100.);
        assert!(n.flakiness_score >= 0.25 && n.flakiness_score <= 1.);
        assert_eq!(n.top_error_signatures, vec![("invalid".into(), 3)]);
        for category in [
            "critical_path",
            "cache_thrash",
            "flakiness",
            "resource_contention",
            "failure_burn",
            "parallelism",
        ] {
            assert!(
                a.recommendations.iter().any(|r| r.category == category),
                "{category}"
            );
        }
        let text = render_analysis_text(&a);
        for evidence in [
            "input_hash_changed",
            "gpu",
            "local/swift",
            "invalid",
            "1500 tokens",
        ] {
            assert!(text.contains(evidence), "{evidence}");
        }
        assert!(render_analysis_markdown(&a).contains("## Recommendations"));
    }

    #[test]
    fn empty_history_and_linear_nested_graph_do_not_suggest_parallelism() {
        let a = analyze_telemetry(&[]);
        assert!(a.recommendations.is_empty());
        assert!(render_analysis_text(&a).contains("No recommendations"));
        let nodes = vec![
            node("a", 2., &[]),
            node("b", 3., &["a"]),
            node("c", 4., &["b"]),
            node("b/x", 2., &[]),
        ];
        assert!(!independent_work(&nodes));
        let mut cached_bridge = nodes.clone();
        cached_bridge[1].cached = true;
        assert!(!independent_work(&cached_bridge));
        let mut split = nodes;
        split.push(node("d", 8., &["a"]));
        assert!(independent_work(&split));
    }
}
