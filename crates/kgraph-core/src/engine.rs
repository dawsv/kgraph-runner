//! Dependency-ordered workflow execution, artifact caching, and run telemetry.
#![allow(clippy::result_large_err)] // The public API returns a structured GraphFailure by value.

use crate::{
    files, hash,
    telemetry::{self, AttemptUsage, NodeTelemetry, ResourceWait, RunTelemetry},
    types::*,
};
use chrono::{SecondsFormat, Utc};
use fs2::FileExt;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs::{self, File},
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex, MutexGuard},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct RunnerConfig {
    pub state_path: PathBuf,
    /// Append one version-1 JSONL record per completed run. Defaults to in-memory telemetry.
    pub telemetry_path: Option<PathBuf>,
    pub retry_nodes: HashSet<String>,
    pub force: bool,
    pub until: Option<String>,
    pub max_worker_calls: Option<usize>,
    /// Optional cumulative USD cost ceiling across all agent calls in a single run.
    pub max_cost_usd: Option<f64>,
    /// Optional cumulative token ceiling (`input + output + cached`) across a single run.
    pub max_tokens: Option<u64>,
    /// Optional wall-clock duration ceiling in seconds for the entire run.
    pub max_wall_time_s: Option<f64>,
    /// Cleanup or escalation nodes executed unconditionally after the main graph completes.
    pub finally_nodes: Vec<Node>,
    pub max_parallel_nodes: usize,
    pub logger: Option<LoggerFn>,
    pub on_event: Option<EventHook>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            state_path: "graph_state.json".into(),
            telemetry_path: None,
            retry_nodes: HashSet::new(),
            force: false,
            until: None,
            max_worker_calls: None,
            max_cost_usd: None,
            max_tokens: None,
            max_wall_time_s: None,
            finally_nodes: Vec::new(),
            max_parallel_nodes: 1,
            logger: None,
            on_event: None,
        }
    }
}

impl RunnerConfig {
    pub fn new(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: state_path.into(),
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct Runtime {
    state: GraphState,
    checkpoint: Option<Checkpoint>,
    write_buf: Vec<u8>,
    retry_nodes: HashSet<String>,
    worker_calls: usize,
    total_tokens: u64,
    total_cost_usd: f64,
    run_started: Option<Instant>,
    efficiency: EfficiencyCounts,
    trace: Vec<Value>,
    until_path: Option<String>,
    reached: Option<String>,
    last: Option<String>,
}

type FileSignature = (u64, u64, i128, u64);

fn file_signature(meta: &fs::Metadata) -> Option<FileSignature> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((
            meta.dev(),
            meta.ino(),
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            meta.len(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

struct Checkpoint {
    path: PathBuf,
    lock: File,
    last_written_sig: Option<FileSignature>,
    // A peer may have written nodes that are absent from this runner's local state.
    merged: Option<GraphState>,
}

struct Unlock<'a>(&'a File);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

struct CompletedRun {
    telemetry: RunTelemetry,
    summarized: bool,
    git_dirty_cwd: Option<PathBuf>,
}

impl CompletedRun {
    fn summary(&mut self) -> &RunTelemetry {
        if !self.summarized {
            self.telemetry.summarize();
            self.summarized = true;
        }
        &self.telemetry
    }

    fn resolve_dirty(&mut self) {
        if let Some(cwd) = self.git_dirty_cwd.take() {
            self.telemetry.git_dirty =
                git_field(&cwd, &["status", "--porcelain"]).map(|s| !s.is_empty());
        }
    }
}

pub struct GraphRunner {
    config: RunnerConfig,
    last_telemetry: Mutex<CompletedRun>,
    runtime: Mutex<Runtime>,
    resources: Mutex<HashMap<String, Arc<ResourceLock>>>,
    log_lock: Mutex<()>,
}

struct RunClock {
    start: Instant,
    nodes: Mutex<Vec<NodeTelemetry>>,
}

#[derive(Clone, Copy)]
struct NodeScope<'a> {
    ctx: &'a Context,
    held: &'a BTreeSet<String>,
    track_stages: bool,
    in_finally: bool,
    clock: &'a RunClock,
}

static DEFAULT_TRANSIENT_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:\b429\b|\b529\b|rate[-_ ]?limit|too many requests|overloaded|temporarily unavailable|service unavailable|econnreset|etimedout|connection reset|timeout)")
        .expect("default transient regex")
});

fn is_transient_failure(policy: &RetryPolicy, outcome: &VerifyOutcome) -> bool {
    if policy.transient_retries == 0 {
        return false;
    }
    let haystack = format!("{} {}", outcome.error_signature, outcome.detail);
    if let Some(custom) = policy.transient_error_regex.as_deref() {
        Regex::new(custom)
            .map(|re| re.is_match(&haystack))
            .unwrap_or(false)
    } else {
        DEFAULT_TRANSIENT_REGEX.is_match(&haystack)
    }
}

fn backoff_duration(policy: &RetryPolicy, retry_index: usize) -> Option<Duration> {
    let base = policy.backoff_s?;
    if !base.is_finite() || base <= 0.0 {
        return None;
    }
    let cap = policy
        .max_backoff_s
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(60.0);
    let factor = 2f64.powi(retry_index.min(16) as i32);
    let mut secs = (base * factor).min(cap);
    if policy.jitter {
        // Light deterministic spread (±15%) keyed by retry index + nanos to avoid thundering herds.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let bucket = ((nanos as usize).wrapping_add(retry_index * 31) % 31) as f64 / 100.0; // 0.00..=0.30
        secs *= 0.85 + bucket;
    }
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

/// Detect either immediate stagnation (`A -> A`) or ping-pong oscillation (`A -> B -> A`).
fn detect_breaker(history: &[String], current: &str, oscillation: bool) -> Option<String> {
    if history.last().is_some_and(|p| p == current) {
        return Some(format!("2x '{current}'"));
    }
    if oscillation {
        if let Some(pos) = history.iter().position(|s| s == current) {
            let mut cycle: Vec<&str> = history[pos..].iter().map(String::as_str).collect();
            cycle.push(current);
            return Some(format!("oscillation: {}", cycle.join(" -> ")));
        }
    }
    None
}

fn snapshot_protected(protected: &ProtectedPaths) -> Result<String, String> {
    let mut parts = Vec::new();
    if !protected.paths.is_empty() {
        parts.push(hash::hash_paths(
            &protected.paths,
            Some(&protected.base_dir),
            None,
            &[],
            false,
        )?);
    }
    if !protected.globs.is_empty() {
        parts.push(hash::hash_globs(
            &protected.globs,
            Some(&protected.base_dir),
            None,
            &[],
            false,
        )?);
    }
    Ok(parts.join(":"))
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

thread_local! {
    static ACTIVE_NODE_PATH: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

// Synchronous nested graphs share a thread. Restore the enclosing scope on
// every exit, including unwinding from a user callback.
struct NodePathGuard(Option<String>);

impl NodePathGuard {
    fn enter(path: &str) -> Self {
        Self(ACTIVE_NODE_PATH.replace(Some(path.to_owned())))
    }
}

impl Drop for NodePathGuard {
    fn drop(&mut self) {
        ACTIVE_NODE_PATH.set(self.0.take());
    }
}

pub(crate) fn active_node_path() -> Option<String> {
    ACTIVE_NODE_PATH.with_borrow(Clone::clone)
}

fn git_field(cwd: &Path, args: &[&str]) -> Option<String> {
    let result = std::process::Command::new("git")
        .current_dir(cwd)
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_string())
}

// A linked worktree keeps HEAD in its own gitdir and branch refs in commondir.
fn read_git_head(marker: &Path) -> Option<String> {
    let dir = if marker.is_dir() {
        marker.to_owned()
    } else {
        let pointer = fs::read_to_string(marker).ok()?;
        marker
            .parent()?
            .join(pointer.trim_end().strip_prefix("gitdir: ")?)
    };
    let head = fs::read_to_string(dir.join("HEAD")).ok()?;
    let valid_oid =
        |s: &str| (s.len() == 40 || s.len() == 64) && s.bytes().all(|c| c.is_ascii_hexdigit());
    let head = head.trim();
    if valid_oid(head) {
        return Some(head.to_ascii_lowercase());
    }
    let name = head.strip_prefix("ref: refs/")?;
    if !name.split('/').all(|part| !matches!(part, "" | "." | "..")) {
        return None;
    }
    let reference = head.strip_prefix("ref: ")?;
    let common = match fs::read_to_string(dir.join("commondir")) {
        Ok(path) => dir.join(path.trim_end()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => dir.clone(),
        Err(_) => return None,
    };
    let base = if ["bisect/", "worktree/", "rewritten/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        &dir
    } else {
        &common
    };
    match fs::read_to_string(base.join(reference)) {
        Ok(oid) => return valid_oid(oid.trim()).then(|| oid.trim().to_ascii_lowercase()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return None,
    }
    fs::read_to_string(common.join("packed-refs"))
        .ok()?
        .lines()
        .find_map(|line| {
            let (oid, name) = line.split_once(' ')?;
            (name == reference && valid_oid(oid)).then(|| oid.to_ascii_lowercase())
        })
}

pub fn spawn_git_dirty_probe() -> Option<thread::JoinHandle<Option<bool>>> {
    let (_, dirty_cwd) = git_provenance(Some(String::new()), None);
    let cwd = dirty_cwd?;
    thread::Builder::new()
        .spawn(move || git_field(&cwd, &["status", "--porcelain"]).map(|s| !s.is_empty()))
        .ok()
}

fn git_provenance(head: Option<String>, dirty: Option<bool>) -> (Option<String>, Option<PathBuf>) {
    if head.is_some() && dirty.is_some() {
        return (head, None);
    }
    let Ok(cwd) = std::env::current_dir() else {
        return (head, None);
    };
    // Explicit repository selection and discovery overrides are handled by Git itself.
    let customized = [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_CEILING_DIRECTORIES",
        "GIT_NAMESPACE",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some());
    let marker = cwd
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|p| p.exists());
    if marker.is_none() && !customized {
        return (head, None);
    }
    let head = head.or_else(|| {
        if !customized {
            if let Some(oid) = marker.as_deref().and_then(read_git_head) {
                return Some(oid);
            }
        }
        git_field(&cwd, &["rev-parse", "--verify", "HEAD"])
    });
    (head, dirty.is_none().then_some(cwd))
}

fn context_usage(ctx: &Context, path: &str) -> Option<AgentUsage> {
    let map = ctx.0.read().unwrap_or_else(|e| e.into_inner());
    let value = map.get("__kgraph_usage")?.get(path)?;
    serde_json::from_value(value.clone()).ok()
}

fn usage_increment(
    previous: Option<&AgentUsage>,
    current: Option<&AgentUsage>,
) -> Option<AgentUsage> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }
    let mut delta = current.clone();
    if let Some(prev) = previous.filter(|p| {
        p.provider == current.provider
            && p.model == current.model
            && p.input_tokens <= current.input_tokens
            && p.output_tokens <= current.output_tokens
            && p.cached_tokens <= current.cached_tokens
            && p.tool_calls <= current.tool_calls
            && p.cost_usd <= current.cost_usd
    }) {
        delta.input_tokens -= prev.input_tokens;
        delta.output_tokens -= prev.output_tokens;
        delta.cached_tokens -= prev.cached_tokens;
        delta.tool_calls -= prev.tool_calls;
        delta.cost_usd -= prev.cost_usd;
    }
    Some(delta)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct ResourceState {
    owner: Option<(ThreadId, usize)>,
    callback_active: bool,
}

#[derive(Default)]
struct ResourceLock {
    state: Mutex<ResourceState>,
    available: Condvar,
}

impl ResourceLock {
    fn acquire(&self, blocking: bool, mut on_wait: impl FnMut(Instant, Duration)) -> bool {
        let id = thread::current().id();
        let mut state = lock(&self.state);
        loop {
            if !state.callback_active {
                match &mut state.owner {
                    None => {
                        state.owner = Some((id, 1));
                        return true;
                    }
                    Some((current, depth)) if *current == id => {
                        *depth += 1;
                        return true;
                    }
                    _ => (),
                }
            }
            if !blocking {
                return false;
            }
            let wait_start = Instant::now();
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
            on_wait(wait_start, wait_start.elapsed());
        }
    }
    fn release(&self) {
        let mut state = lock(&self.state);
        let (current, depth) = state.owner.as_mut().expect("resource was acquired");
        debug_assert_eq!(*current, thread::current().id());
        *depth -= 1;
        if *depth == 0 {
            state.owner = None;
            self.available.notify_all();
        }
    }
}

// A timed callback may outlive both its task and an enclosing exclusive stage.
struct CallbackReservation(Vec<Arc<ResourceLock>>);
impl CallbackReservation {
    fn new(resources: &[Arc<ResourceLock>]) -> Self {
        for resource in resources {
            let mut state = lock(&resource.state);
            debug_assert_eq!(state.owner.map(|o| o.0), Some(thread::current().id()));
            debug_assert!(!state.callback_active);
            state.callback_active = true;
        }
        Self(resources.to_vec())
    }
}
impl Drop for CallbackReservation {
    fn drop(&mut self) {
        for resource in &self.0 {
            lock(&resource.state).callback_active = false;
            resource.available.notify_all();
        }
    }
}

#[derive(Default)]
struct Resources(Vec<Arc<ResourceLock>>);
impl Drop for Resources {
    fn drop(&mut self) {
        for resource in self.0.iter().rev() {
            resource.release();
        }
    }
}

fn static_children(node: &Node) -> Option<&[Node]> {
    match node {
        Node::Subgraph(n) => Some(&n.nodes),
        Node::LoopSubgraph(n) => Some(&n.nodes),
        _ => None,
    }
}

fn static_resources(node: &Node, names: &mut BTreeSet<String>) {
    if let Some(name) = node.resource().filter(|s| !s.is_empty()) {
        names.insert(name.into());
    }
    for child in static_children(node).unwrap_or_default() {
        static_resources(child, names);
    }
}

fn read_state(path: &Path) -> Result<GraphState, String> {
    if !path.try_exists().map_err(|e| e.to_string())? {
        return Ok(GraphState::default());
    }
    let mut map = files::read_object(path)?;
    map.entry("version").or_insert(json!(3));
    serde_json::from_value(Value::Object(map))
        .map_err(|e| format!("Invalid state in {}: {e}", path.display()))
}

// Atomic replacement must target the backing file, and every alias must take
// the same advisory lock. A link may point at a state file yet to be created.
fn state_destination(path: &Path) -> Result<PathBuf, String> {
    let mut target = path.to_owned();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_symlink() => {
                let next = std::fs::read_link(&target).map_err(|e| e.to_string())?;
                target = target.with_file_name(next);
            }
            Ok(_) => return Ok(target),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(e) => return Err(format!("{}: {e}", target.display())),
        }
    }
    Err(format!(
        "Too many symbolic links resolving {}",
        path.display()
    ))
}

fn path_for(prefix: &str, id: &str) -> String {
    if prefix.is_empty() {
        id.into()
    } else {
        format!("{prefix}/{id}")
    }
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}

fn duration(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.).round() / 1000.
}

fn callback<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "callback panicked".into())
    })
}

fn metadata<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    callback(f)?
}

fn node_index(nodes: &[Node]) -> Result<HashMap<&str, usize>, String> {
    let mut ids = HashMap::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        if ids.insert(n.id(), i).is_some() {
            return Err(format!("Duplicate node ID '{}'", n.id()));
        }
    }
    Ok(ids)
}

fn ordered_nodes(nodes: &[Node]) -> Result<Vec<&Node>, String> {
    fn visit<'a>(
        i: usize,
        nodes: &'a [Node],
        ids: &HashMap<&str, usize>,
        visited: &mut [u8],
        order: &mut Vec<&'a Node>,
    ) -> Result<(), String> {
        let n = &nodes[i];
        match visited[i] {
            2 => return Ok(()),
            1 => return Err(format!("Cycle detected in graph at node '{}'", n.id())),
            _ => visited[i] = 1,
        }
        for dep in n.deps() {
            let &j = ids
                .get(dep.as_str())
                .ok_or_else(|| format!("Node '{}' depends on unknown sibling '{dep}'", n.id()))?;
            visit(j, nodes, ids, visited, order)?;
        }
        visited[i] = 2;
        order.push(n);
        Ok(())
    }
    let ids = node_index(nodes)?;
    let mut order = Vec::with_capacity(nodes.len());
    let mut visited = vec![0; nodes.len()];
    for i in 0..nodes.len() {
        visit(i, nodes, &ids, &mut visited, &mut order)?;
    }
    Ok(order)
}

fn node_failure_status(error: &GraphFailure) -> &'static str {
    if error.budget_exceeded && !error.circuit_broken && !error.awaiting_subagents {
        "failed"
    } else {
        error.status_str()
    }
}

fn extend(mut entry: Value, fields: Value) -> Value {
    if let Value::Object(map) = fields {
        entry.as_object_mut().expect("record object").extend(map);
    }
    entry
}

impl GraphRunner {
    pub fn new(mut config: RunnerConfig) -> Result<Self, String> {
        config.max_parallel_nodes = config.max_parallel_nodes.max(1);
        let runtime = Runtime {
            state: read_state(&config.state_path)?,
            retry_nodes: std::mem::take(&mut config.retry_nodes),
            ..Default::default()
        };
        Ok(Self {
            last_telemetry: Mutex::new(CompletedRun {
                git_dirty_cwd: None,
                summarized: true,
                telemetry: RunTelemetry {
                    version: 1,
                    state_path: config.state_path.display().to_string(),
                    ..Default::default()
                },
            }),
            config,
            runtime: Mutex::new(runtime),
            resources: Default::default(),
            log_lock: Mutex::new(()),
        })
    }

    pub fn state(&self) -> GraphState {
        lock(&self.runtime).state.clone()
    }
    pub fn trace(&self) -> Vec<Value> {
        lock(&self.runtime).trace.clone()
    }
    pub fn worker_calls_used(&self) -> usize {
        lock(&self.runtime).worker_calls
    }
    pub fn efficiency(&self) -> EfficiencyCounts {
        lock(&self.runtime).efficiency.clone()
    }
    pub fn reached_path(&self) -> Option<String> {
        lock(&self.runtime).reached.clone()
    }
    pub fn last_completed_stage(&self) -> Option<String> {
        lock(&self.runtime).last.clone()
    }

    fn log(&self, message: String) {
        let _guard = lock(&self.log_lock);
        if let Some(logger) = &self.config.logger {
            logger(&message);
        } else {
            println!("{message}");
        }
    }

    fn emit(&self, name: &str, fields: Value) {
        if let Some(hook) = &self.config.on_event {
            let event = extend(json!({"event": name, "timestamp": now()}), fields);
            let _ = callback(|| hook(&event));
        }
    }

    fn record(&self, path: &str, entry: Value, trace: bool) -> Result<(), GraphFailure> {
        let mut rt = lock(&self.runtime);
        let Runtime {
            state,
            checkpoint,
            write_buf,
            trace: events,
            ..
        } = &mut *rt;
        let save = || -> Result<(), String> {
            if checkpoint.is_none() {
                let path = state_destination(
                    &std::path::absolute(&self.config.state_path).map_err(|e| e.to_string())?,
                )?;
                let mut lock_path = path.as_os_str().to_owned();
                lock_path.push(".lock");
                let lock = files::open_lock_file(Path::new(&lock_path))?;
                *checkpoint = Some(Checkpoint {
                    path,
                    lock,
                    last_written_sig: None,
                    merged: None,
                });
            }
            let checkpoint = checkpoint.as_mut().unwrap();
            checkpoint
                .lock
                .lock_exclusive()
                .map_err(|e| e.to_string())?;
            let _guard = Unlock(&checkpoint.lock);
            match fs::metadata(&checkpoint.path) {
                Ok(meta)
                    if checkpoint.last_written_sig.is_some()
                        && file_signature(&meta) == checkpoint.last_written_sig => {}
                Ok(_) => checkpoint.merged = Some(read_state(&checkpoint.path)?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => checkpoint.merged = None,
                Err(e) => return Err(e.to_string()),
            }
            let disk = checkpoint.merged.as_mut().unwrap_or(state);
            let previous = disk.nodes.insert(path.into(), entry);
            write_buf.clear();
            let result = serde_json::to_writer_pretty(&mut *write_buf, &disk)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    write_buf.push(b'\n');
                    files::write_bytes(&checkpoint.path, write_buf)
                });
            if let Err(e) = result {
                if let Some(previous) = previous {
                    disk.nodes.insert(path.into(), previous);
                } else {
                    disk.nodes.remove(path);
                }
                checkpoint.last_written_sig = None;
                return Err(e);
            }
            checkpoint.last_written_sig = fs::metadata(&checkpoint.path)
                .ok()
                .and_then(|m| file_signature(&m));
            if let Some(merged) = &checkpoint.merged {
                state.nodes.insert(path.into(), merged.nodes[path].clone());
            }
            if trace {
                events.push(extend(json!({"node":path}), state.nodes[path].clone()));
            }
            Ok(())
        };
        save().map_err(|e| GraphFailure::new(path, e))
    }

    pub fn topo_sort(&self, nodes: &[Node]) -> Result<Vec<Node>, String> {
        Ok(ordered_nodes(nodes)?.into_iter().cloned().collect())
    }

    fn resolve_until(
        &self,
        nodes: &[Node],
        prefix: &str,
        validate: bool,
    ) -> Result<Option<String>, GraphFailure> {
        let Some(request) = self.config.until.as_deref() else {
            return Ok(None);
        };
        let fail = |s| GraphFailure::new("--until", s);
        if request
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(fail(format!("Invalid stage path '{request}'")));
        }
        fn collect<'a>(
            nodes: &'a [Node],
            prefix: &str,
            validate: bool,
            stages: &mut Vec<(String, &'a Node)>,
            seen: &mut HashSet<String>,
        ) -> Result<(), GraphFailure> {
            if validate {
                ordered_nodes(nodes).map_err(|e| GraphFailure::new(prefix, e))?;
            }
            for n in nodes {
                let p = path_for(prefix, n.id());
                if !seen.insert(p.clone()) {
                    return Err(GraphFailure::new(
                        "--until",
                        format!("Ambiguous stage path '{p}'"),
                    ));
                }
                stages.push((p.clone(), n));
                if let Some(children) = static_children(n) {
                    collect(children, &p, validate, stages, seen)?;
                }
            }
            Ok(())
        }
        let mut stages = Vec::new();
        collect(nodes, prefix, validate, &mut stages, &mut HashSet::new())?;
        let matches: Vec<_> = stages
            .iter()
            .filter(|(p, n)| {
                if request.contains('/') {
                    p == request
                } else {
                    n.id() == request
                }
            })
            .map(|(p, _)| p.clone())
            .collect();
        if matches.len() > 1 {
            return Err(fail(format!(
                "Ambiguous stage '{request}'. Choose from {}.",
                matches.join(", ")
            )));
        }
        let path = matches.first().map(String::as_str).unwrap_or(request);
        for (p, n) in stages {
            if matches!(n, Node::Splitter(_)) && path.starts_with(&format!("{p}/")) {
                return Err(fail(format!(
                    "Cannot stop inside runtime splitter '{p}'. Stop at the splitter itself."
                )));
            }
        }
        matches.into_iter().next().map(Some).ok_or_else(|| {
            fail(format!(
                "Unknown stage '{request}'. Use a unique stage ID or full path."
            ))
        })
    }

    pub fn freshness_reason(
        &self,
        full_path: &str,
        local_id: &str,
        in_hash: &str,
        outputs: &[PathBuf],
        cacheable: bool,
    ) -> (bool, String) {
        if !cacheable {
            return (false, "not_cacheable".into());
        }
        if self.config.force {
            return (false, "forced".into());
        }
        let record = {
            let rt = lock(&self.runtime);
            if rt.retry_nodes.contains(local_id)
                || rt.retry_nodes.contains(full_path)
                || rt.retry_nodes.iter().any(|p| {
                    full_path
                        .strip_prefix(p)
                        .is_some_and(|s| s.starts_with('/'))
                })
            {
                return (false, "retry_requested".into());
            }
            let Some(rec) = rt
                .state
                .nodes
                .get(full_path)
                .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
            else {
                return (false, "no_prior_state".into());
            };
            if rec["status"] != "passed" {
                return (
                    false,
                    format!("prior_status:{}", rec["status"].as_str().unwrap_or("None")),
                );
            }
            if rec["input_hash"] != in_hash {
                return (false, "input_hash_changed".into());
            }
            if outputs.is_empty() {
                return (true, "fresh".into());
            }
            rec.get("output_hashes").cloned()
        };
        for p in outputs {
            if !p.exists() {
                return (false, format!("missing_output:{}", p.display()));
            }
        }
        for p in outputs {
            if let Some(sha) = record
                .as_ref()
                .and_then(|hashes| hashes.get(p.to_string_lossy().as_ref()))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                if p.is_file() && hash::sha256_file(p).as_deref() != Ok(sha) {
                    return (false, format!("output_hash_changed:{}", p.display()));
                }
            }
        }
        (true, "fresh".into())
    }

    pub fn plan(
        &self,
        root_nodes: &[Node],
        ctx: &Context,
        prefix: &str,
    ) -> Result<Vec<PlanEntry>, GraphFailure> {
        let until = self.resolve_until(root_nodes, prefix, true)?;
        let mut entries = Vec::with_capacity(root_nodes.len());
        let ordered = ordered_nodes(root_nodes).map_err(|e| GraphFailure::new(prefix, e))?;
        self.plan_nodes(
            ordered,
            &ctx.shallow_clone(),
            prefix,
            until.as_deref(),
            &mut entries,
        )?;
        Ok(entries)
    }

    fn plan_nodes<'a>(
        &self,
        nodes: impl IntoIterator<Item = &'a Node>,
        ctx: &Context,
        prefix: &str,
        until: Option<&str>,
        entries: &mut Vec<PlanEntry>,
    ) -> Result<bool, GraphFailure> {
        for n in nodes {
            let path = path_for(prefix, n.id());
            let mut status = "container";
            let mut reason = None;
            let mut detail = None;
            let mut children = Vec::new();
            let mut ordered = Vec::new();
            let mut skipped = false;
            if let Some(when) = n.when_fn() {
                match callback(|| when(ctx)).map_err(|e| GraphFailure::new(&path, e))? {
                    Ok(false) => {
                        skipped = true;
                        status = "skipped";
                        reason = Some("condition_false".into());
                        detail = n
                            .when_expr()
                            .map(|expr| format!("Skipped by condition: {expr}"));
                    }
                    Ok(true) => (),
                    Err(error) => {
                        detail = Some(format!("Condition deferred: {error}"));
                    }
                }
            }
            if !skipped {
                match n {
                    Node::Task(task) => {
                        let inputs =
                            callback(|| Ok(((task.input_hash_fn)(ctx)?, (task.outputs_fn)(ctx)?)))
                                .map_err(|e| GraphFailure::new(&path, e))?;
                        let (fresh, cache_reason) = match inputs {
                            Ok((hash, outputs)) => {
                                self.freshness_reason(&path, n.id(), &hash, &outputs, task.cacheable)
                            }
                            Err(error) => {
                                detail = Some(error);
                                (false, "inputs_unavailable".into())
                            }
                        };
                        status = if fresh { "cached" } else { "will_run" };
                        reason = Some(cache_reason);
                    }
                    Node::Splitter(split) => match callback(|| build_children(split, ctx))
                        .map_err(|e| GraphFailure::new(&path, e))?
                    {
                        Ok(items) => {
                            node_index(&items).map_err(|e| GraphFailure::new(&path, e))?;
                            children = items;
                        }
                        Err(error) => detail = Some(error),
                    },
                    _ => {
                        ordered = ordered_nodes(static_children(n).unwrap_or_default())
                            .map_err(|e| GraphFailure::new(&path, e))?
                    }
                }
            }
            let stop = Some(path.as_str()) == until;
            let child_prefix = (!children.is_empty() || !ordered.is_empty()).then(|| path.clone());
            entries.push(PlanEntry {
                path,
                id: n.id().into(),
                node_type: n.node_type_name().into(),
                kind: n.kind_str().into(),
                deps: n.deps().to_vec(),
                resource: n.resource().map(String::from),
                status: status.into(),
                reason,
                detail,
            });
            if let Some(prefix) = child_prefix {
                if self.plan_nodes(children.iter().chain(ordered), ctx, &prefix, until, entries)? {
                    return Ok(true);
                }
            }
            if stop {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn run(
        &self,
        root_nodes: &[Node],
        ctx: &Context,
        prefix: &str,
    ) -> Result<Context, GraphFailure> {
        {
            let mut rt = lock(&self.runtime);
            rt.until_path = None;
            rt.reached = None;
            rt.last = None;
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);
        let string_field = |key| ctx.get(key).and_then(|v| v.as_str().map(str::to_owned));
        let git_dirty = ctx.get("git_dirty").and_then(|v| v.as_bool());
        let (git_head, git_dirty_cwd) = git_provenance(string_field("git_head"), git_dirty);
        let dirty_worker = git_dirty_cwd
            .as_ref()
            .filter(|_| self.config.telemetry_path.is_some())
            .and_then(|cwd| {
                let cwd = cwd.clone();
                thread::Builder::new()
                    .spawn(move || {
                        git_field(&cwd, &["status", "--porcelain"]).map(|s| !s.is_empty())
                    })
                    .ok()
            });
        let started_at = now();
        let clock = RunClock {
            start: Instant::now(),
            nodes: Mutex::new(Vec::with_capacity(root_nodes.len())),
        };
        {
            let mut rt = lock(&self.runtime);
            rt.total_tokens = 0;
            rt.total_cost_usd = 0.0;
            rt.run_started = Some(clock.start);
        }
        let mut run = RunTelemetry {
            version: 1,
            run_id: format!(
                "{}-{:x}-{:x}",
                started_at,
                std::process::id(),
                RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            workflow_ref: string_field("workflow_ref"),
            state_path: self.config.state_path.display().to_string(),
            git_head,
            git_dirty,
            started_at,
            ..Default::default()
        };
        self.emit("run_start", json!({"status":"running","error":""}));
        let mut result = self
            .resolve_until(root_nodes, prefix, false)
            .and_then(|until| {
                lock(&self.runtime).until_path = until;
                self.run_nodes(
                    root_nodes,
                    prefix,
                    NodeScope {
                        ctx,
                        track_stages: true,
                        in_finally: false,
                        held: &BTreeSet::new(),
                        clock: &clock,
                    },
                    clock.start,
                )
            });
        if !self.config.finally_nodes.is_empty() {
            let (main_status, main_error) = match &result {
                Err(e) => (e.status_str(), e.to_string()),
                Ok(()) if self.reached_path().is_some() => ("stopped", String::new()),
                Ok(()) => ("passed", String::new()),
            };
            ctx.insert("__kgraph_run_status", json!(main_status));
            ctx.insert("__kgraph_failure", json!(main_error));
            let saved_reached = {
                let mut rt = lock(&self.runtime);
                rt.until_path = None;
                rt.reached.take()
            };
            let finally_result = self.run_nodes(
                &self.config.finally_nodes,
                "finally",
                NodeScope {
                    ctx,
                    track_stages: false,
                    in_finally: true,
                    held: &BTreeSet::new(),
                    clock: &clock,
                },
                Instant::now(),
            );
            lock(&self.runtime).reached = saved_reached;
            if let Err(fe) = finally_result {
                if result.is_ok() {
                    result = Err(fe);
                } else {
                    self.log(format!("[finally:err]  {fe}"));
                }
            }
        }
        let (status, exit_code, error) = match &result {
            Err(e) => (e.status_str(), e.exit_code(), e.to_string()),
            Ok(()) if self.reached_path().is_some() => ("stopped", 0, String::new()),
            Ok(()) => ("passed", 0, String::new()),
        };
        run.wall_time_ms = millis(clock.start.elapsed());
        run.finished_at = now();
        run.nodes = clock.nodes.into_inner().unwrap_or_else(|e| e.into_inner());
        run.status = status.into();
        run.exit_code = exit_code;
        let git_dirty_cwd = if let Some(worker) = dirty_worker {
            run.git_dirty = worker.join().ok().flatten();
            None
        } else {
            git_dirty_cwd
        };
        let mut completed = CompletedRun {
            telemetry: run,
            summarized: false,
            git_dirty_cwd,
        };
        if let Some(path) = &self.config.telemetry_path {
            if same_report_target(path, &self.config.state_path) == Ok(false) {
                completed.resolve_dirty();
                let _ = telemetry::append_run_telemetry(path, completed.summary());
            }
        }
        *lock(&self.last_telemetry) = completed;
        self.emit("run_end", json!({"status":status,"error":error}));
        result.map(|()| ctx.clone())
    }

    /// Snapshot the most recently completed run, optionally supplying its workflow reference.
    pub fn build_run_telemetry(
        &self,
        status: &str,
        exit_code: i32,
        workflow_ref: Option<&str>,
    ) -> RunTelemetry {
        self.build_run_telemetry_with_git_dirty(status, exit_code, workflow_ref, None)
    }

    /// Snapshot the most recently completed run using a precomputed `git_dirty` probe when available.
    pub fn build_run_telemetry_with_git_dirty(
        &self,
        status: &str,
        exit_code: i32,
        workflow_ref: Option<&str>,
        git_dirty_override: Option<Option<bool>>,
    ) -> RunTelemetry {
        let mut completed = lock(&self.last_telemetry);
        if completed.git_dirty_cwd.is_some() {
            if let Some(dirty) = git_dirty_override {
                completed.git_dirty_cwd = None;
                completed.telemetry.git_dirty = dirty;
            } else {
                completed.resolve_dirty();
            }
        }
        let mut result = completed.summary().clone();
        result.status = status.into();
        result.exit_code = exit_code;
        if let Some(workflow) = workflow_ref {
            result.workflow_ref = Some(workflow.into());
        }
        result
    }

    fn run_nodes(
        &self,
        nodes: &[Node],
        prefix: &str,
        scope: NodeScope<'_>,
        scope_ready: Instant,
    ) -> Result<(), GraphFailure> {
        let ordered = ordered_nodes(nodes).map_err(|e| GraphFailure::new(prefix, e))?;
        let execute = |i: usize, ready: Instant| {
            let n = ordered[i];
            if prefix.is_empty() {
                self.execute(n, n.id(), scope, ready)
            } else {
                self.execute(n, &path_for(prefix, n.id()), scope, ready)
            }
        };
        if !scope.held.is_empty()
            || self.config.max_parallel_nodes == 1
            || lock(&self.runtime).until_path.is_some()
        {
            let mut finished = HashMap::with_capacity(ordered.len());
            for (i, node) in ordered.iter().enumerate() {
                let ready = node
                    .deps()
                    .iter()
                    .map(|d| finished[d.as_str()])
                    .max()
                    .unwrap_or(scope_ready);
                execute(i, ready)?;
                finished.insert(node.id(), Instant::now());
                if self.reached_path().is_some() {
                    break;
                }
            }
            return Ok(());
        }
        let ids: HashMap<_, _> = ordered
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id(), i))
            .collect();
        let deps = ordered
            .iter()
            .map(|node| node.deps().iter().map(|dep| ids[dep.as_str()]).collect())
            .collect();
        schedule(
            deps,
            scope_ready,
            self.config.max_parallel_nodes,
            false,
            if prefix.is_empty() { "root" } else { prefix },
            execute,
        )
    }

    fn execute(
        &self,
        node: &Node,
        path: &str,
        scope: NodeScope<'_>,
        ready_at: Instant,
    ) -> Result<(), GraphFailure> {
        let NodeScope {
            ctx,
            held,
            track_stages,
            in_finally,
            clock,
        } = scope;
        let _node_scope = NodePathGuard::enter(path);
        let kind = node.kind_str();
        let mut acquired = Resources::default();
        let mut start = Instant::now();
        let mut span = NodeTelemetry {
            path: path.into(),
            kind: kind.into(),
            deps: node.deps().to_vec(),
            resource: node.resource().map(str::to_owned),
            ..Default::default()
        };
        let mut skipped_by_when = false;
        let result = (|| {
            if !in_finally {
                self.check_budgets(path)?;
            }
            if let Some(when) = node.when_fn() {
                let should_run =
                    metadata(|| when(ctx)).map_err(|e| GraphFailure::new(path, e))?;
                if !should_run {
                    skipped_by_when = true;
                    start = Instant::now();
                    span.started_at = now();
                    let detail = node
                        .when_expr()
                        .map(|e| format!("Skipped by condition: {e}"))
                        .unwrap_or_else(|| "Skipped by condition".into());
                    self.record(
                        path,
                        json!({"kind":kind,"status":"skipped","detail":detail,"updated_at":now()}),
                        true,
                    )?;
                    self.log(format!("[graph:skip]   {path} — {detail}"));
                    self.emit(
                        "node_skipped",
                        json!({"node":path,"kind":kind,"detail":detail}),
                    );
                    if track_stages {
                        let mut rt = lock(&self.runtime);
                        rt.last = Some(path.into());
                        if rt.until_path.as_deref() == Some(path) {
                            rt.reached = Some(path.into());
                        }
                    }
                    return Ok(());
                }
            }
            let mut names = BTreeSet::new();
            // Only exclusive stages reserve their whole static subtree. A resource-free
            // container can schedule independent children on different threads.
            if node.resource().is_some_and(|s| !s.is_empty()) {
                static_resources(node, &mut names);
            }
            for name in &names {
                let resource = lock(&self.resources)
                    .entry(name.clone())
                    .or_default()
                    .clone();
                // A dynamic splitter can discover a lower-ranked name after we enter.
                // Reentry or immediate acquisition is safe, waiting out of order is not.
                let blocking = held.contains(name) || !held.last().is_some_and(|last| name <= last);
                let acquired_lock = resource.acquire(blocking, |wait_start, elapsed| {
                    let duration_ms = millis(elapsed);
                    span.resource_wait_ms += duration_ms;
                    span.resource_waits.push(ResourceWait {
                        resource: name.clone(),
                        start_offset_ms: millis(wait_start - clock.start),
                        duration_ms,
                    });
                });
                if !acquired_lock {
                    start = Instant::now();
                    span.started_at = now();
                    return Err(GraphFailure::new(
                        path,
                        format!(
                            "Cannot wait for resource '{name}' while holding '{}'",
                            held.last().unwrap()
                        ),
                    ));
                }
                acquired.0.push(resource);
            }
            names.extend(held.iter().cloned());
            let simple = matches!(node, Node::Subgraph(_) | Node::Splitter(_));
            start = Instant::now();
            span.started_at = now();
            if simple {
                span.attempts = 1;
                self.emit("node_start", json!({"node":path,"kind":kind,"attempt":1}));
            }
            match node {
                Node::Task(n) => self.run_task(n, path, ctx, &names, in_finally, &mut span)?,
                Node::Subgraph(n) => {
                    self.log(format!("[subgraph:enter] {path}"));
                    self.run_nodes(
                        &n.nodes,
                        path,
                        NodeScope {
                            held: &names,
                            ..scope
                        },
                        Instant::now(),
                    )?;
                    if self.reached_path().is_some() {
                        return Ok(());
                    }
                    self.log(format!("[subgraph:done]  {path}"));
                }
                Node::LoopSubgraph(n) => {
                    self.run_loop(
                        n,
                        path,
                        NodeScope {
                            held: &names,
                            ..scope
                        },
                        &mut span,
                    )?;
                    if self.reached_path().is_some() {
                        return Ok(());
                    }
                }
                Node::Splitter(n) => {
                    let children = metadata(|| build_children(n, ctx))
                        .map_err(|e| GraphFailure::new(path, e))?;
                    node_index(&children).map_err(|e| GraphFailure::new(path, e))?;
                    self.log(format!(
                        "[splitter]     {path} ({} branch(es), workers={})",
                        children.len(),
                        n.max_workers
                    ));
                    let branches_ready = Instant::now();
                    let exec = |i: usize, ready: Instant| {
                        self.execute(
                            &children[i],
                            &path_for(path, children[i].id()),
                            NodeScope {
                                track_stages: false,
                                held: &names,
                                ..scope
                            },
                            ready,
                        )
                    };
                    if !names.is_empty() || n.max_workers <= 1 || children.len() <= 1 {
                        for i in 0..children.len() {
                            exec(i, branches_ready)?;
                        }
                    } else {
                        schedule(
                            vec![Vec::new(); children.len()],
                            branches_ready,
                            n.max_workers,
                            true,
                            path,
                            exec,
                        )?;
                    }
                }
            }
            if simple {
                self.emit("node_passed", json!({"node":path,"kind":kind,"attempts":1,"duration_s":duration(start),"cache_miss_reason":null}));
            }
            if track_stages {
                let mut rt = lock(&self.runtime);
                rt.last = Some(path.into());
                if rt.until_path.as_deref() == Some(path) {
                    rt.reached = Some(path.into());
                }
            }
            Ok(())
        })();
        let end = Instant::now();
        span.finished_at = now();
        span.start_offset_ms = millis(start - clock.start);
        span.end_offset_ms = millis(end - clock.start);
        span.duration_ms = millis(end - start);
        span.queue_wait_ms = millis(start.saturating_duration_since(ready_at));
        span.status = match &result {
            Err(e) => node_failure_status(e),
            Ok(()) if skipped_by_when => "skipped",
            Ok(()) if span.cached => "cached",
            Ok(())
                if !matches!(node, Node::Task(_))
                    && self.reached_path().is_some_and(|p| p != path) =>
            {
                "stopped"
            }
            Ok(()) => "passed",
        }
        .into();
        span.circuit_broken = result.as_ref().is_err_and(|e| e.circuit_broken);
        span.repaired = span.status == "passed" && !span.error_signatures.is_empty();
        lock(&clock.nodes).push(span);
        if let Err(e) = &result {
            self.emit(
                &format!("node_{}", node_failure_status(e)),
                json!({"node":path, "kind":kind, "detail":e.reason,
                "packet_path":e.packet_path,"packet_paths":e.packet_paths}),
            );
        }
        result
    }

    fn check_budgets(&self, path: &str) -> Result<(), GraphFailure> {
        let rt = lock(&self.runtime);
        if let (Some(max_s), Some(started)) = (self.config.max_wall_time_s, rt.run_started) {
            let elapsed = started.elapsed().as_secs_f64();
            if elapsed >= max_s {
                let mut error = GraphFailure::new(
                    path,
                    format!("Wall-time budget exceeded ({elapsed:.2}s/{max_s:.2}s)"),
                );
                error.budget_exceeded = true;
                return Err(error);
            }
        }
        if let Some(max_tokens) = self.config.max_tokens {
            if rt.total_tokens >= max_tokens {
                let mut error = GraphFailure::new(
                    path,
                    format!("Token budget exceeded ({}/{max_tokens})", rt.total_tokens),
                );
                error.budget_exceeded = true;
                return Err(error);
            }
        }
        if let Some(max_cost) = self.config.max_cost_usd {
            if rt.total_cost_usd >= max_cost {
                let mut error = GraphFailure::new(
                    path,
                    format!(
                        "Cost budget exceeded (${:.4}/${:.4})",
                        rt.total_cost_usd, max_cost
                    ),
                );
                error.budget_exceeded = true;
                return Err(error);
            }
        }
        Ok(())
    }

    fn record_run_usage(&self, usage: &AgentUsage) {
        let mut rt = lock(&self.runtime);
        rt.total_tokens = rt.total_tokens.saturating_add(usage.total_tokens());
        rt.total_cost_usd += usage.cost_usd.max(0.0);
    }

    fn worker_call(&self, path: &str) -> Result<(), GraphFailure> {
        let mut rt = lock(&self.runtime);
        if let Some(max) = self.config.max_worker_calls {
            if rt.worker_calls >= max {
                let mut error = GraphFailure::new(
                    path,
                    format!("Worker call budget exceeded ({}/{max})", rt.worker_calls),
                );
                error.budget_exceeded = true;
                return Err(error);
            }
        }
        rt.worker_calls += 1;
        Ok(())
    }

    fn run_task(
        &self,
        n: &TaskNode,
        path: &str,
        ctx: &Context,
        held: &BTreeSet<String>,
        in_finally: bool,
        span: &mut NodeTelemetry,
    ) -> Result<(), GraphFailure> {
        let started = Instant::now();
        let input_hash =
            metadata(|| (n.input_hash_fn)(ctx)).map_err(|e| GraphFailure::new(path, e))?;
        let outputs = metadata(|| (n.outputs_fn)(ctx)).map_err(|e| GraphFailure::new(path, e))?;
        let (fresh, reason) =
            self.freshness_reason(path, &n.id, &input_hash, &outputs, n.cacheable);
        if fresh {
            span.cached = true;
            self.log(format!("[graph:cached] {path}"));
            lock(&self.runtime)
                .trace
                .push(json!({"node":path,"status":"cached","input_hash":input_hash}));
            if self.config.on_event.is_some() {
                self.emit(
                    "node_cached",
                    json!({"node":path,"kind":n.kind,"input_hash":input_hash}),
                );
            }
            return Ok(());
        }
        span.cache_miss_reason = Some(reason.clone());
        let fields = json!({"kind":n.kind,"input_hash":input_hash,"cache_miss_reason":reason,"outputs":outputs});
        let record_error = |attempt: usize, detail: String| -> GraphFailure {
            let failure = json!({"status":"failed","attempts":attempt,"duration_s":duration(started),"error_signature":"exception:Error","detail":detail,"updated_at":now()});
            self.record(path, extend(fields.clone(), failure), true)
                .err()
                .unwrap_or_else(|| GraphFailure::new(path, detail))
        };
        let resources: Vec<_> = if n.timeout_s.is_some() && !held.is_empty() {
            let locks = lock(&self.resources);
            held.iter().map(|name| locks[name].clone()).collect()
        } else {
            Vec::new()
        };
        let mut last_context_usage = None;
        let mut seen_signatures: Vec<String> = Vec::new();
        let mut repair_attempt: usize = 0;
        let mut transient_used: usize = 0;
        let mut total_attempts: usize = 0;
        let protected = n.protected_paths.as_ref().filter(|p| !p.is_empty());
        loop {
            if !in_finally {
                self.check_budgets(path)?;
            }
            let count = total_attempts + 1;
            if n.kind == TaskKind::Worker {
                self.worker_call(path)?;
            }
            let protected_before = match protected {
                Some(spec) => Some(
                    metadata(|| snapshot_protected(spec))
                        .map_err(|e| GraphFailure::new(path, format!("Protected paths: {e}")))?,
                ),
                None => None,
            };
            let timestamp = now();
            self.record(path, extend(fields.clone(), json!({"status":"running","attempts":total_attempts,"started_at":timestamp,"updated_at":timestamp})), false)?;
            self.emit(
                "node_start",
                json!({"node":path,"kind":n.kind,"attempt":count}),
            );
            let mut detached = None;
            span.attempts = count;
            let result = invoke(n, ctx, path, repair_attempt, &resources, &mut detached);
            let direct = result
                .as_ref()
                .ok()
                .and_then(|value| value.get("__kgraph_usage"))
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let usage_records = result
                .as_ref()
                .ok()
                .and_then(|value| value.get("__kgraph_usage_records"))
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
            let outcome = result.and_then(|value| {
                callback(|| {
                    let outcome = n
                        .verify_fn
                        .as_ref()
                        .map_or_else(|| VerifyOutcome::pass("ok"), |verify| verify(ctx, &value));
                    if let Some(fn_) = &n.efficiency_fn {
                        let counts = fn_(&value);
                        lock(&self.runtime).efficiency.add_assign(&counts);
                    }
                    outcome
                })
                .map_err(TaskRunError::other)
            });
            // Returned usage is per call. The context fallback is a node-level
            // snapshot and can persist through verification and subsequent retries.
            let current = context_usage(ctx, path);
            let usage =
                direct.or_else(|| usage_increment(last_context_usage.as_ref(), current.as_ref()));
            last_context_usage = current;
            if let Some(usage) = usage {
                self.record_run_usage(&usage);
                span.agent_usage
                    .get_or_insert_with(Default::default)
                    .add_assign(&usage);
                span.attempt_usage.push(AttemptUsage {
                    attempt: count,
                    failed: !outcome.as_ref().is_ok_and(|o| o.ok),
                    usage,
                    usage_records,
                });
            }
            let mut outcome = match outcome {
                Ok(verified) => verified,
                Err(TaskRunError::Graph(e)) => {
                    let mut e = *e;
                    span.error_signatures.push(e.status_str().into());
                    let primary = e.packet_path.take();
                    let paths = std::mem::take(&mut e.packet_paths);
                    let e = e.with_packets(primary, paths);
                    lock(&self.runtime).efficiency.add_assign(&e.efficiency);
                    self.record(path, extend(fields.clone(), json!({"status":node_failure_status(&e),"attempts":count,"detail":e.reason,
                        "packet_path":e.packet_path,"packet_paths":e.packet_paths,"updated_at":now()})),true)?;
                    return Err(e);
                }
                Err(TaskRunError::Timeout(detail)) => VerifyOutcome::fail("timeout", detail),
                Err(TaskRunError::Other { signature, detail }) => {
                    VerifyOutcome::fail(signature, detail)
                }
            };
            if outcome.ok {
                if let (Some(spec), Some(before)) = (protected, protected_before.as_deref()) {
                    let after = metadata(|| snapshot_protected(spec))
                        .map_err(|e| GraphFailure::new(path, format!("Protected paths: {e}")))?;
                    if after != before {
                        outcome = VerifyOutcome::fail(
                            "protected_path_modified",
                            "Protected files were modified or deleted during execution",
                        );
                    }
                }
            }
            if outcome.ok {
                let (out_paths, hashes, final_hash) = metadata(|| {
                    let paths = (n.outputs_fn)(ctx)?;
                    let mut hashes = Map::new();
                    for p in &paths {
                        if p.is_file() {
                            hashes.insert(
                                p.to_string_lossy().into_owned(),
                                Value::String(hash::sha256_file(p)?),
                            );
                        }
                    }
                    Ok((paths, hashes, (n.input_hash_fn)(ctx)?))
                })
                .map_err(|e| {
                    span.error_signatures.push("exception:Error".into());
                    record_error(count, e)
                })?;
                let dur = duration(started);
                self.record(path, extend(fields, json!({"status":"passed","input_hash":final_hash,"attempts":count,"duration_s":dur,
                    "detail":outcome.detail,"outputs":out_paths,"output_hashes":hashes,"updated_at":now()})),true)?;
                {
                    let mut rt = lock(&self.runtime);
                    rt.retry_nodes.remove(path);
                    rt.retry_nodes.remove(&n.id);
                    if n.kind == TaskKind::Verifier {
                        rt.efficiency.script_verifiers_passed += 1;
                    }
                }
                self.log(format!(
                    "[graph:ran]    {path} (attempts={count}, {dur:.2}s) — {}",
                    outcome.detail
                ));
                self.emit("node_passed",json!({"node":path,"kind":n.kind,"attempts":count,"duration_s":dur,"cache_miss_reason":reason}));
                return Ok(());
            }
            span.error_signatures.push(outcome.error_signature.clone());
            total_attempts += 1;
            if transient_used < n.retry_policy.transient_retries
                && is_transient_failure(&n.retry_policy, &outcome)
            {
                let delay = backoff_duration(&n.retry_policy, transient_used)
                    .unwrap_or_else(|| Duration::from_secs_f64(0.5 * 2f64.powi(transient_used.min(6) as i32)));
                transient_used += 1;
                self.log(format!(
                    "[graph:backoff] {path} transient failure ({}/{}, sleeping {:.2}s): {}",
                    transient_used,
                    n.retry_policy.transient_retries,
                    delay.as_secs_f64(),
                    outcome.detail
                ));
                self.emit(
                    "node_transient_retry",
                    json!({"node":path,"kind":n.kind,"attempt":count,"transient_attempt":transient_used,"backoff_s":delay.as_secs_f64(),"error_signature":outcome.error_signature,"detail":outcome.detail}),
                );
                if let Some(worker) = detached {
                    let _ = worker.join();
                }
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                continue;
            }
            let breaker = detect_breaker(
                &seen_signatures,
                &outcome.error_signature,
                n.retry_policy.oscillation_breaker,
            );
            if breaker.is_some() || repair_attempt == n.max_retries {
                return self.verification_failed(
                    path,
                    fields,
                    count,
                    started,
                    &outcome,
                    breaker.as_deref(),
                );
            }
            self.log(format!(
                "[graph:retry]  {path} attempt {count} failed ({}): {}",
                outcome.error_signature, outcome.detail
            ));
            self.emit("node_retry",json!({"node":path,"kind":n.kind,"attempt":count,"error_signature":outcome.error_signature,"detail":outcome.detail}));
            if let Some(worker) = detached {
                // Repair and retry use the same exclusive resources as the callback.
                let _ = worker.join();
            }
            if let Some(delay) = backoff_duration(&n.retry_policy, repair_attempt) {
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
            if let Some(repair) = &n.repair_fn {
                metadata(|| repair(ctx, count, &outcome.error_signature, &outcome.detail))
                    .map_err(|e| {
                        span.error_signatures.push("exception:Error".into());
                        record_error(count, e)
                    })?;
            }
            seen_signatures.push(outcome.error_signature);
            repair_attempt += 1;
        }
    }

    fn run_loop(
        &self,
        n: &LoopSubgraphNode,
        path: &str,
        scope: NodeScope<'_>,
        span: &mut NodeTelemetry,
    ) -> Result<(), GraphFailure> {
        let ctx = scope.ctx;
        let started = Instant::now();
        let mut seen_signatures: Vec<String> = Vec::new();
        let mut repair_attempt: usize = 0;
        let mut transient_used: usize = 0;
        let mut total_attempts: usize = 0;
        let protected = n.protected_paths.as_ref().filter(|p| !p.is_empty());
        loop {
            if !scope.in_finally {
                self.check_budgets(path)?;
            }
            let count = total_attempts + 1;
            span.attempts = count;
            let protected_before = match protected {
                Some(spec) => Some(
                    metadata(|| snapshot_protected(spec))
                        .map_err(|e| GraphFailure::new(path, format!("Protected paths: {e}")))?,
                ),
                None => None,
            };
            self.log(format!(
                "[loop_sg:iter] {path} (attempt {count}/{})",
                n.max_retries + 1
            ));
            self.emit(
                "node_start",
                json!({"node":path,"kind":"loop_subgraph","attempt":count}),
            );
            self.run_nodes(&n.nodes, path, scope, Instant::now())?;
            if self.reached_path().is_some() {
                return Ok(());
            }
            let mut outcome =
                metadata(|| (n.verify_fn)(ctx)).map_err(|e| GraphFailure::new(path, e))?;
            if outcome.ok {
                if let (Some(spec), Some(before)) = (protected, protected_before.as_deref()) {
                    let after = metadata(|| snapshot_protected(spec))
                        .map_err(|e| GraphFailure::new(path, format!("Protected paths: {e}")))?;
                    if after != before {
                        outcome = VerifyOutcome::fail(
                            "protected_path_modified",
                            "Protected files were modified or deleted during loop iteration",
                        );
                    }
                }
            }
            if outcome.ok {
                let dur = duration(started);
                self.record(path,json!({"kind":"loop_subgraph","status":"passed","attempts":count,"duration_s":dur,
                    "detail":outcome.detail,"updated_at":now()}),true)?;
                self.log(format!(
                    "[loop_sg:PASS] {path} (attempts={count}, {dur:.2}s) — {}",
                    outcome.detail
                ));
                self.emit("node_passed",json!({"node":path,"kind":"loop_subgraph","attempts":count,"duration_s":dur,"cache_miss_reason":null}));
                return Ok(());
            }
            span.error_signatures.push(outcome.error_signature.clone());
            total_attempts += 1;
            if transient_used < n.retry_policy.transient_retries
                && is_transient_failure(&n.retry_policy, &outcome)
            {
                let delay = backoff_duration(&n.retry_policy, transient_used)
                    .unwrap_or_else(|| Duration::from_secs_f64(0.5 * 2f64.powi(transient_used.min(6) as i32)));
                transient_used += 1;
                self.log(format!(
                    "[loop_sg:back] {path} transient failure ({}/{}, sleeping {:.2}s): {}",
                    transient_used,
                    n.retry_policy.transient_retries,
                    delay.as_secs_f64(),
                    outcome.detail
                ));
                self.emit(
                    "node_transient_retry",
                    json!({"node":path,"kind":"loop_subgraph","attempt":count,"transient_attempt":transient_used,"backoff_s":delay.as_secs_f64(),"error_signature":outcome.error_signature,"detail":outcome.detail}),
                );
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                continue;
            }
            let breaker = detect_breaker(
                &seen_signatures,
                &outcome.error_signature,
                n.retry_policy.oscillation_breaker,
            );
            if breaker.is_some() || repair_attempt == n.max_retries {
                return self.verification_failed(
                    path,
                    json!({"kind":"loop_subgraph"}),
                    count,
                    started,
                    &outcome,
                    breaker.as_deref(),
                );
            }
            self.log(format!(
                "[loop_sg:rtry] {path} attempt {count} failed ({}): {}",
                outcome.error_signature, outcome.detail
            ));
            self.emit("node_retry",json!({"node":path,"kind":"loop_subgraph","attempt":count,"error_signature":outcome.error_signature,"detail":outcome.detail}));
            if let Some(delay) = backoff_duration(&n.retry_policy, repair_attempt) {
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
            if let Some(repair) = &n.repair_fn {
                let dirty =
                    callback(|| repair(ctx, count, &outcome.error_signature, &outcome.detail))
                        .map_err(|e| GraphFailure::new(path, e))??;
                let mut rt = lock(&self.runtime);
                for child in dirty {
                    rt.retry_nodes
                        .insert(if child.starts_with(&format!("{path}/")) {
                            child
                        } else {
                            path_for(path, &child)
                        });
                }
            }
            seen_signatures.push(outcome.error_signature);
            repair_attempt += 1;
        }
    }

    fn verification_failed(
        &self,
        path: &str,
        fields: Value,
        count: usize,
        started: Instant,
        outcome: &VerifyOutcome,
        breaker: Option<&str>,
    ) -> Result<(), GraphFailure> {
        let is_loop = fields["kind"] == "loop_subgraph";
        let broken = breaker.is_some();
        self.record(path,extend(fields,json!({"status":if broken {"circuit_broken"} else {"failed"},
            "attempts":count,"duration_s":duration(started),"error_signature":outcome.error_signature,"detail":outcome.detail,"updated_at":now()})),true)?;
        let reason = if let Some(breaker_label) = breaker {
            let label = if is_loop {
                "[loop_sg:BRK] "
            } else {
                "[graph:BREAK] "
            };
            self.log(format!(
                "{label} {path} circuit breaker tripped on {breaker_label}: {}",
                outcome.detail
            ));
            let prefix = if is_loop {
                "Subgraph circuit breaker"
            } else {
                "Circuit breaker"
            };
            format!("{prefix} ({breaker_label}): {}", outcome.detail)
        } else {
            if !is_loop {
                self.log(format!(
                    "[graph:FAIL]   {path} exhausted {count} attempt(s): {}",
                    outcome.detail
                ));
            }
            outcome.detail.clone()
        };
        let mut error = GraphFailure::new(path, reason);
        error.circuit_broken = broken;
        Err(error)
    }

    pub fn write_run_report(
        &self,
        report_path: Option<&Path>,
        overall_status: &str,
        error: &str,
        packet_path: Option<&str>,
        packet_paths: &[String],
        extra_fields: Option<&Map<String, Value>>,
    ) -> Result<PathBuf, String> {
        let state = &self.config.state_path;
        let output = report_path
            .map(Path::to_owned)
            .unwrap_or_else(|| default_report_path(state));
        if same_report_target(&output, state)? {
            return Err(format!(
                "Report path resolves to the state file {}",
                state.display()
            ));
        }
        let (trace, efficiency, worker_calls, requested, reached, last) = {
            let rt = lock(&self.runtime);
            (
                rt.trace.clone(),
                rt.efficiency.clone(),
                rt.worker_calls,
                rt.until_path.clone(),
                rt.reached.clone(),
                rt.last.clone(),
            )
        };
        let packets = GraphFailure::new("", "")
            .with_packets(packet_path.map(String::from), packet_paths.to_vec());
        let mut reached_nodes = Vec::new();
        let mut latest = HashMap::new();
        let mut counts = HashMap::new();
        let mut retried = Vec::new();
        let mut failure_sigs = Vec::new();
        let mut timed = Vec::new();
        for event in &trace {
            let node = event["node"].as_str().unwrap_or_default();
            let status = event["status"].as_str().unwrap_or_default();
            if latest.insert(node, status).is_none() {
                reached_nodes.push(node);
            }
            *counts.entry(status).or_insert(0usize) += 1;
            let attempts = event["attempts"].as_u64().unwrap_or(1);
            if attempts > 1 {
                retried.push(node);
            }
            if matches!(status, "failed" | "circuit_broken") {
                failure_sigs.push(json!({"node":node,"status":status,"error_signature":event["error_signature"],"detail":event["detail"]}));
            }
            if let Some(dur) = event["duration_s"].as_f64() {
                timed.push((
                    dur,
                    json!({"node":node,"duration_s":dur,"attempts":attempts}),
                ));
            }
        }
        timed.sort_by(|a, b| b.0.total_cmp(&a.0));
        let slowest: Vec<_> = timed.into_iter().take(5).map(|(_, entry)| entry).collect();
        let cached_nodes: Vec<_> = reached_nodes
            .iter()
            .filter(|&&node| latest[node] == "cached")
            .copied()
            .collect();
        let count = |s| counts.get(s).copied().unwrap_or_default();
        let mut hints = Vec::new();
        if count("circuit_broken") > 0 {
            hints.push("Circuit breaker tripped on repeated identical error — classify as Graph/Verifier Flaw vs Task Bug before retrying.".into());
        }
        if failure_sigs
            .iter()
            .any(|v| v["error_signature"] == "timeout")
        {
            hints.push(
                "Timeout hit — inspect hung subprocess or adjust node timeout_s / split workload."
                    .into(),
            );
        }
        if retried.len() >= 2 {
            hints.push(format!("Multiple nodes required retries ({}) — consider tightening upstream prompts or adding a pre-verifier.",retried.join(", ")));
        }
        let summary = extend(
            json!({"total_events":trace.len(),"cached":count("cached"),"ran_passed":count("passed"),"failed":count("failed"),
            "circuit_broken":count("circuit_broken"),"retried_nodes":retried,"worker_calls_used":worker_calls,"max_worker_calls":self.config.max_worker_calls}),
            json!(efficiency),
        );
        let telemetry_summary = {
            let mut completed = lock(&self.last_telemetry);
            let run = completed.summary();
            json!({"run_id":run.run_id,"wall_time_ms":run.wall_time_ms,"total_work_ms":run.total_work_ms,
                   "parallelism_factor":run.parallelism_factor,"critical_path":run.critical_path,
                   "cache_hit_rate_pct":run.cache_hit_rate_pct,"total_tokens":run.total_tokens,"total_cost_usd":run.total_cost_usd})
        };
        let mut report = json!({
            "version":1,"status":if overall_status=="passed" && reached.is_some() {"stopped"} else {overall_status},
            "error":error,"awaiting_subagents":overall_status=="awaiting_subagents","packet_path":packets.packet_path,
            "packet_paths":packets.packet_paths,"cached_nodes":cached_nodes,"requested_until":self.config.until,
            "requested_path":requested,"reached_path":reached,"last_completed_stage":last,"generated_at":now(),"state_file":self.config.state_path,
            "scope":{"requested_until":self.config.until,"stop_reached":reached.is_some(),"reached_nodes":reached_nodes},
            "summary":summary,"telemetry_summary":telemetry_summary,"failure_signatures":failure_sigs,"slowest_nodes":slowest,"evolution_hints":hints,
        });
        if let Some(extra) = extra_fields {
            report.as_object_mut().unwrap().extend(extra.clone());
        }
        files::write_json(&output, &report, true)?;
        Ok(output)
    }
}

fn default_report_path(state: &Path) -> PathBuf {
    let stem = state.file_stem().unwrap_or_default().to_string_lossy();
    if stem == "state" {
        state.with_file_name("report.json")
    } else {
        state.with_file_name(format!(
            "{}_report.json",
            stem.strip_suffix("_state").unwrap_or(&stem)
        ))
    }
}

fn same_report_target(report: &Path, state: &Path) -> Result<bool, String> {
    let report = files::resolve_path(report)?;
    let state = files::resolve_path(state)?;
    if report == state {
        return Ok(true);
    }
    // Existing names may refer to one file through case folding or hard links.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (report.metadata(), state.metadata()) {
            return Ok((a.dev(), a.ino()) == (b.dev(), b.ino()));
        }
    }
    #[cfg(windows)]
    if let (Ok(a), Ok(b)) = (report.canonicalize(), state.canonicalize()) {
        return Ok(a == b);
    }
    // The first report can be written before state exists. On platforms with
    // commonly case-insensitive volumes, also protect that future pathname.
    #[cfg(any(windows, target_os = "macos"))]
    if let (Some(a), Some(b)) = (report.to_str(), state.to_str()) {
        return Ok(a.to_lowercase() == b.to_lowercase());
    }
    Ok(false)
}

fn build_children(split: &SplitterNode, ctx: &Context) -> Result<Vec<Node>, String> {
    (split.items_fn)(ctx)?
        .iter()
        .map(|item| (split.child_builder_fn)(item, ctx))
        .collect()
}

fn invoke(
    n: &TaskNode,
    ctx: &Context,
    path: &str,
    attempt: usize,
    resources: &[Arc<ResourceLock>],
    detached: &mut Option<thread::JoinHandle<()>>,
) -> TaskResult {
    let Some(seconds) = n.timeout_s else {
        return callback(|| (n.run_fn)(ctx, attempt)).map_err(TaskRunError::other)?;
    };
    let limit = Duration::try_from_secs_f64(seconds.max(0.0))
        .map_err(|e| TaskRunError::other(e.to_string()))?;
    let function = n.run_fn.clone();
    let ctx = ctx.clone();
    let (send, recv) = mpsc::channel();
    let reservation = CallbackReservation::new(resources);
    let path = path.to_owned();
    let worker = thread::Builder::new()
        .spawn(move || {
            let _node_scope = NodePathGuard::enter(&path);
            let result = callback(|| function(&ctx, attempt))
                .map_err(TaskRunError::other)
                .and_then(|r| r);
            drop(reservation);
            let _ = send.send(result);
        })
        .map_err(|e| TaskRunError::other(e.to_string()))?;
    match recv.recv_timeout(limit) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if !resources.is_empty() {
                *detached = Some(worker);
            }
            Err(TaskRunError::Timeout(format!(
                "exceeded timeout_s={seconds}s"
            )))
        }
        Err(e) => Err(TaskRunError::other(e.to_string())),
    }
}

// The coordinator owns the dependency counters. Workers report completions individually
// so a dependent can start as soon as its final prerequisite completes.
fn schedule(
    deps: Vec<Vec<usize>>,
    scope_ready: Instant,
    limit: usize,
    splitter: bool,
    path: &str,
    execute: impl Fn(usize, Instant) -> Result<(), GraphFailure> + Sync,
) -> Result<(), GraphFailure> {
    let limit = limit.min(deps.len());
    if limit == 0 {
        return Ok(());
    }
    let mut ready = VecDeque::new();
    let mut remaining = Vec::with_capacity(deps.len());
    let mut ready_at = vec![scope_ready; deps.len()];
    let mut dependents = vec![Vec::new(); deps.len()];
    for (i, parents) in deps.into_iter().enumerate() {
        remaining.push(parents.len());
        if parents.is_empty() {
            ready.push_back(i);
        }
        for p in parents {
            dependents[p].push(i);
        }
    }
    let mut errors = BTreeMap::new();
    let (work, jobs) = mpsc::channel::<(usize, Instant)>();
    let jobs = Mutex::new(jobs);
    let (done, completed) = mpsc::channel();
    thread::scope(|workers| {
        for _ in 0..limit {
            let (jobs, execute, done) = (&jobs, &execute, done.clone());
            workers.spawn(move || loop {
                let job = lock(jobs).recv();
                let Ok((i, ready_at)) = job else { break };
                let result = callback(|| execute(i, ready_at))
                    .map_err(|e| GraphFailure::new(path, e))
                    .and_then(|r| r);
                if done.send((i, result, Instant::now())).is_err() {
                    break;
                }
            });
        }
        drop(done);
        let mut active = 0;
        loop {
            while active < limit && (splitter || errors.is_empty()) {
                let Some(i) = ready.pop_front() else { break };
                work.send((i, ready_at[i])).expect("worker pool alive");
                active += 1;
            }
            if active == 0 {
                break;
            }
            let first = completed
                .recv()
                .expect("workers send one completion per job");
            for (i, result, finished) in std::iter::once(first).chain(completed.try_iter()) {
                active -= 1;
                match result {
                    Err(e) => {
                        errors.insert(i, e);
                    }
                    Ok(()) => {
                        for &child in &dependents[i] {
                            ready_at[child] = ready_at[child].max(finished);
                            remaining[child] -= 1;
                            if remaining[child] == 0 {
                                ready.push_back(child);
                            }
                        }
                    }
                }
            }
        }
        drop(work);
    });
    if errors.len() <= 1 {
        return errors.into_values().next().map_or(Ok(()), Err);
    }
    let summary = errors
        .values()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if errors.values().all(|e| e.awaiting_subagents) {
        let packets = errors.into_values().flat_map(|e| e.packet_paths).collect();
        let mut error = GraphFailure::new(path, summary).with_packets(None, packets);
        error.awaiting_subagents = true;
        Err(error)
    } else {
        let group = if splitter {
            "splitter branch(es)"
        } else {
            "parallel sibling(s)"
        };
        Err(GraphFailure::new(
            path,
            format!("{} {group} failed: {summary}", errors.len()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering::SeqCst},
            Barrier, OnceLock, Weak,
        },
    };
    use tempfile::tempdir;

    fn config(dir: &Path) -> RunnerConfig {
        RunnerConfig {
            state_path: dir.join("graph_state.json"),
            logger: Some(Arc::new(|_| {})),
            ..Default::default()
        }
    }
    fn task(id: &str) -> TaskNode {
        TaskNode::new(
            id,
            TaskKind::Code,
            Arc::new(|_, _| Ok(Value::Null)),
            Arc::new(|_| Ok("input".into())),
        )
    }
    fn splitter(children: Vec<Node>) -> Node {
        let len = children.len();
        Node::Splitter(SplitterNode::new(
            "split",
            Arc::new(move |_| Ok((0..len).map(|i| json!(i)).collect())),
            Arc::new(move |i, _| Ok(children[i.as_u64().unwrap() as usize].clone())),
        ))
    }
    fn run(runner: &GraphRunner, nodes: &[Node]) -> Result<Context, GraphFailure> {
        runner.run(nodes, &Context::new(), "")
    }
    fn report(runner: &GraphRunner, status: &str) -> Value {
        let p = runner
            .write_run_report(None, status, "", None, &[], None)
            .unwrap();
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn report_paths_protect_state() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state.json");
        let runner = GraphRunner::new(RunnerConfig::new(&state)).unwrap();
        #[cfg(any(windows, target_os = "macos"))]
        {
            let alias = dir.path().join("STATE.JSON");
            assert!(runner
                .write_run_report(Some(&alias), "passed", "", None, &[], None)
                .is_err());
            assert!(!alias.exists());
        }
        run(&runner, &[Node::Task(task("first"))]).unwrap();
        let saved = fs::read(&state).unwrap();
        let write = |p| runner.write_run_report(p, "passed", "", None, &[], None);
        assert_eq!(write(None).unwrap(), dir.path().join("report.json"));
        assert!(write(Some(&state)).is_err());
        let alias = dir.path().join("missing").join("..").join("state.json");
        assert!(write(Some(&alias)).is_err());
        assert!(!dir.path().join("missing").exists());
        #[cfg(unix)]
        {
            let symlink = dir.path().join("alias.json");
            std::os::unix::fs::symlink(&state, &symlink).unwrap();
            assert!(write(Some(&symlink)).is_err());
            assert!(symlink.is_symlink());
            let hardlink = dir.path().join("linked.json");
            fs::hard_link(&state, &hardlink).unwrap();
            assert!(write(Some(&hardlink)).is_err());
        }
        assert_eq!(fs::read(&state).unwrap(), saved);
        for (name, report) in [
            ("graph_state.json", "graph_report.json"),
            ("session.db", "session_report.json"),
            ("report.json", "report_report.json"),
        ] {
            assert_eq!(default_report_path(Path::new(name)), Path::new(report));
        }
    }

    #[test]
    fn read_only_plan_and_all_cache_reasons() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("new");
        let cfg = config(&root);
        let r = GraphRunner::new(cfg.clone()).unwrap();
        let artifact = dir.path().join("result");
        let mut leaf = task("leaf");
        let output = artifact.clone();
        leaf.outputs_fn = Arc::new(move |_| Ok(vec![output.clone()]));
        let output = artifact.clone();
        leaf.run_fn = Arc::new(move |_, _| {
            fs::write(&output, "result").unwrap();
            Ok(Value::Null)
        });
        let graph = [Node::Task(leaf)];
        let reason = |r: &GraphRunner, hash: &str, cacheable: bool| {
            r.freshness_reason(
                "leaf",
                "leaf",
                hash,
                std::slice::from_ref(&artifact),
                cacheable,
            )
            .1
        };
        assert_eq!(
            r.plan(&graph, &Context::new(), "").unwrap()[0]
                .reason
                .as_deref(),
            Some("no_prior_state")
        );
        assert!(!root.exists());
        run(&r, &graph).unwrap();
        assert_eq!(reason(&r, "input", true), "fresh");
        assert_eq!(reason(&r, "input", false), "not_cacheable");
        assert_eq!(reason(&r, "other", true), "input_hash_changed");
        assert_eq!(
            reason(
                &GraphRunner::new(RunnerConfig {
                    force: true,
                    ..cfg.clone()
                })
                .unwrap(),
                "input",
                true
            ),
            "forced"
        );
        assert_eq!(
            reason(
                &GraphRunner::new(RunnerConfig {
                    retry_nodes: ["leaf".to_string()].into(),
                    ..cfg.clone()
                })
                .unwrap(),
                "input",
                true
            ),
            "retry_requested"
        );
        run(&r, &graph).unwrap();
        fs::write(&artifact, "tampered").unwrap();
        assert_eq!(
            reason(&r, "input", true),
            format!("output_hash_changed:{}", artifact.display())
        );
        fs::remove_file(&artifact).unwrap();
        assert_eq!(
            reason(&r, "input", true),
            format!("missing_output:{}", artifact.display())
        );
        run(&r, &graph).unwrap();
        let summary = report(&r, "passed");
        assert_eq!(summary["summary"]["cached"], 1);
        assert_eq!(summary["cached_nodes"], json!([]));
        let mut state = r.state();
        state.nodes.get_mut("leaf").unwrap()["status"] = json!("running");
        files::write_json(&cfg.state_path, &state, true).unwrap();
        assert_eq!(
            reason(&GraphRunner::new(cfg).unwrap(), "input", true),
            "prior_status:running"
        );
    }

    #[test]
    fn topology_and_until_validate_before_checkpoint() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let ctx = Context::new();
        let r = GraphRunner::new(cfg.clone()).unwrap();
        let one = Node::Task(task("one"));
        let two = Node::Task(task("two"));
        assert_eq!(
            r.topo_sort(&[one.clone(), one.clone()]).err().unwrap(),
            "Duplicate node ID 'one'"
        );
        assert_eq!(
            run(&r, &[splitter(vec![one.clone(), one.clone()])])
                .unwrap_err()
                .reason,
            "Duplicate node ID 'one'"
        );
        let mut bad = task("bad");
        bad.deps = vec!["absent".into()];
        assert_eq!(
            r.plan(&[Node::Task(bad.clone())], &ctx, "")
                .unwrap_err()
                .reason,
            "Node 'bad' depends on unknown sibling 'absent'"
        );
        bad.deps = vec!["bad".into()];
        assert_eq!(
            run(&r, &[Node::Task(bad)]).unwrap_err().reason,
            "Cycle detected in graph at node 'bad'"
        );
        let mut dep = task("stop");
        dep.deps = vec!["one".into()];
        let graph = [
            Node::Subgraph(SubgraphNode::new(
                "parent",
                vec![Node::Task(dep), two, one.clone()],
            )),
            one.clone(),
            splitter(vec![one]),
        ];
        for (requested, part) in [
            ("", "Invalid"),
            ("parent/./one", "Invalid"),
            ("../one", "Invalid"),
            ("x//y", "Invalid"),
            ("one", "Ambiguous"),
            ("other", "Unknown"),
            ("split/child", "Cannot stop"),
        ] {
            let r = GraphRunner::new(RunnerConfig {
                until: Some(requested.into()),
                ..cfg.clone()
            })
            .unwrap();
            assert!(r.plan(&graph, &ctx, "").unwrap_err().reason.contains(part));
        }
        let r = GraphRunner::new(RunnerConfig {
            until: Some("stop".into()),
            max_parallel_nodes: 4,
            ..cfg
        })
        .unwrap();
        let plan = r.plan(&graph, &ctx, "").unwrap();
        assert_eq!(
            plan.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["parent", "parent/one", "parent/stop"]
        );
        run(&r, &graph).unwrap();
        assert_eq!(r.last_completed_stage().as_deref(), Some("parent/stop"));
        let v = report(&r, "passed");
        assert_eq!(v["status"], "stopped");
        assert_eq!(v["requested_path"], "parent/stop");
        assert_eq!(
            v["scope"],
            json!({"requested_until":"stop","stop_reached":true,"reached_nodes":["parent/one","parent/stop"]})
        );
    }

    #[test]
    fn plan_previews_splitter_with_private_context_and_unavailable_inputs() {
        let dir = tempdir().unwrap();
        let r = GraphRunner::new(config(dir.path())).unwrap();
        let ctx = Context::new();
        let split = SplitterNode::new(
            "items",
            Arc::new(|c| {
                c.insert("seen", json!(true));
                Ok(vec![json!("child")])
            }),
            Arc::new(|item, c| {
                assert_eq!(c.get("seen"), Some(json!(true)));
                let mut n = task(item.as_str().unwrap());
                n.input_hash_fn = Arc::new(|_| Err("missing input".into()));
                n.run_fn = Arc::new(|_, _| panic!("unexpected execution"));
                Ok(Node::Task(n))
            }),
        );
        let plan = r.plan(&[Node::Splitter(split)], &ctx, "scope").unwrap();
        assert_eq!(plan[1].path, "scope/items/child");
        assert_eq!(plan[1].reason.as_deref(), Some("inputs_unavailable"));
        assert_eq!(plan[1].detail.as_deref(), Some("missing input"));
        assert!(ctx.get("seen").is_none());
        assert!(r.trace().is_empty());
    }

    #[test]
    fn nested_splitter_single_leaf_retry() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let branches = ["a", "b", "c"].map(|id| {
            Node::Subgraph(SubgraphNode::new(
                id,
                vec![Node::Task(task("make")), Node::Task(task("check"))],
            ))
        });
        let graph = [Node::Subgraph(SubgraphNode::new(
            "parent",
            vec![splitter(branches.into())],
        ))];
        run(&GraphRunner::new(cfg.clone()).unwrap(), &graph).unwrap();
        let path = "parent/split/b/make";
        let r = GraphRunner::new(RunnerConfig {
            retry_nodes: [path.to_owned()].into(),
            ..cfg
        })
        .unwrap();
        run(&r, &graph).unwrap();
        assert_eq!(
            r.trace()
                .iter()
                .filter(|e| e["status"] == "passed")
                .map(|e| e["node"].clone())
                .collect::<Vec<_>>(),
            [json!(path)]
        );
        assert_eq!(r.trace().len(), 6);
    }

    #[test]
    fn loop_targeted_repair_and_circuit_breaker() {
        let dir = tempdir().unwrap();
        let r = GraphRunner::new(config(dir.path())).unwrap();
        let mut loop_node = LoopSubgraphNode::new(
            "loop",
            vec![Node::Task(task("static")), Node::Task(task("fix"))],
            Arc::new(|ctx| {
                Ok(if ctx.get("fixed").is_some() {
                    VerifyOutcome::pass("fixed")
                } else {
                    VerifyOutcome::fail("gap", "negative")
                })
            }),
        );
        loop_node.repair_fn = Some(Arc::new(|ctx, count, sig, detail| {
            assert_eq!((count, sig, detail), (1, "gap", "negative"));
            ctx.insert("fixed", json!(true));
            Ok(vec!["fix".into()])
        }));
        run(&r, &[Node::LoopSubgraph(loop_node)]).unwrap();
        assert_eq!(r.state().nodes["loop"]["attempts"], 2);
        assert_eq!(
            r.state().nodes["loop/fix"]["cache_miss_reason"],
            "retry_requested"
        );
        assert_eq!(report(&r, "passed")["cached_nodes"], json!(["loop/static"]));
        let bad = LoopSubgraphNode::new(
            "bad",
            vec![],
            Arc::new(|_| Ok(VerifyOutcome::fail("same", "never"))),
        );
        assert!(
            run(&r, &[Node::LoopSubgraph(bad)])
                .unwrap_err()
                .circuit_broken
        );
        assert_eq!(r.state().nodes["bad"]["attempts"], 2);
    }

    #[test]
    fn dependents_start_before_unrelated_slow_sibling_finishes() {
        let dir = tempdir().unwrap();
        let r = GraphRunner::new(RunnerConfig {
            max_parallel_nodes: 2,
            ..config(dir.path())
        })
        .unwrap();
        let (start, started) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        let (started, finished) = (Mutex::new(started), Mutex::new(finished));
        let mut slow = task("slow");
        slow.run_fn = Arc::new(move |_, _| {
            start.send(()).unwrap();
            lock(&finished)
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(Value::Null)
        });
        let mut fast = task("fast");
        fast.run_fn = Arc::new(move |_, _| {
            lock(&started).recv_timeout(Duration::from_secs(3)).unwrap();
            Ok(Value::Null)
        });
        let mut dependent = task("dependent");
        dependent.deps = vec!["fast".into()];
        dependent.run_fn = Arc::new(move |_, _| {
            finish.send(()).unwrap();
            Ok(Value::Null)
        });
        run(
            &r,
            &[Node::Task(slow), Node::Task(fast), Node::Task(dependent)],
        )
        .unwrap();
    }

    #[test]
    fn nested_resources_reserve_in_order_and_splitter_honors_child_locks() {
        let dir = tempdir().unwrap();
        let r = GraphRunner::new(RunnerConfig {
            max_parallel_nodes: 4,
            ..config(dir.path())
        })
        .unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let leaf = |id: &str, resource: Option<&str>| {
            let mut n = task(id);
            n.resource = resource.map(String::from);
            let active = active.clone();
            n.run_fn = Arc::new(move |_, _| {
                assert_eq!(active.fetch_add(1, SeqCst), 0);
                thread::sleep(Duration::from_millis(15));
                active.fetch_sub(1, SeqCst);
                Ok(Value::Null)
            });
            Node::Task(n)
        };
        let mut left = SubgraphNode::new("left", vec![leaf("leaf", Some("B"))]);
        left.resource = Some("A".into());
        let mut right = LoopSubgraphNode::new(
            "right",
            vec![leaf("leaf", Some("A"))],
            Arc::new(|_| Ok(VerifyOutcome::pass("ok"))),
        );
        right.resource = Some("B".into());
        let mut container =
            SubgraphNode::new("container", vec![splitter(vec![leaf("leaf", Some("A"))])]);
        container.resource = Some("A".into());
        let nodes = vec![
            Node::Subgraph(left),
            Node::LoopSubgraph(right),
            splitter(vec![leaf("task", Some("A")), Node::Subgraph(container)]),
        ];
        let (send, recv) = mpsc::channel();
        thread::spawn(move || {
            let _ = send.send(run(&r, &nodes));
        });
        recv.recv_timeout(Duration::from_secs(4))
            .expect("resource deadlock")
            .unwrap();
    }

    #[test]
    fn detached_callback_retains_inherited_resource() {
        let dir = tempdir().unwrap();
        let runner = GraphRunner::new(config(dir.path())).unwrap();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let mut timed = task("timed");
        timed.timeout_s = Some(0.02);
        timed.run_fn = Arc::new(move |_, _| {
            lock(&released)
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok(Value::Null)
        });
        let mut stage = SubgraphNode::new("stage", vec![Node::Task(timed)]);
        stage.resource = Some("shared".into());
        let err = run(&runner, &[Node::Subgraph(stage)]).unwrap_err();
        assert!(err.reason.contains("exceeded timeout"));
        run(&runner, &[Node::Task(task("unrelated"))]).unwrap();

        let (enter, entered) = mpsc::channel();
        let mut next = task("next");
        next.resource = Some("shared".into());
        next.run_fn = Arc::new(move |_, _| {
            enter.send(()).unwrap();
            Ok(Value::Null)
        });
        thread::scope(|scope| {
            // Run again on the original scheduler thread to exercise reentry.
            scope.spawn(move || {
                assert_eq!(
                    entered.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                );
                release.send(()).unwrap();
                entered.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            run(&runner, &[Node::Task(next)]).unwrap();
        });
    }

    #[test]
    fn timeout_retry_waits_before_repairing_exclusive_resource() {
        let dir = tempdir().unwrap();
        let (retry, retrying) = mpsc::channel();
        let runner = GraphRunner::new(RunnerConfig {
            on_event: Some(Arc::new(move |e| {
                if e["event"] == "node_retry" {
                    retry.send(()).unwrap();
                }
            })),
            ..config(dir.path())
        })
        .unwrap();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let done = Arc::new(AtomicUsize::new(0));
        let mut n = task("timed");
        n.resource = Some("exclusive".into());
        n.timeout_s = Some(0.1);
        n.max_retries = 1;
        let callback_done = done.clone();
        n.run_fn = Arc::new(move |_, attempt| {
            if attempt == 0 {
                lock(&released)
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                callback_done.store(1, SeqCst);
            } else {
                assert_eq!(callback_done.load(SeqCst), 1);
            }
            Ok(Value::Null)
        });
        let (repair, repaired) = mpsc::channel();
        n.repair_fn = Some(Arc::new(move |_, _, _, _| {
            assert_eq!(done.load(SeqCst), 1);
            repair.send(()).unwrap();
            Ok(())
        }));
        thread::scope(|scope| {
            scope.spawn(move || {
                retrying.recv_timeout(Duration::from_secs(3)).unwrap();
                release.send(()).unwrap();
                repaired.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            run(&runner, &[Node::Task(n)]).unwrap();
        });
        assert_eq!(runner.state().nodes["timed"]["attempts"], 2);
    }

    #[test]
    fn worker_budget_and_detached_timeout() {
        let dir = tempdir().unwrap();
        let cfg = RunnerConfig {
            max_worker_calls: Some(1),
            ..config(dir.path())
        };
        let r = GraphRunner::new(cfg.clone()).unwrap();
        let mut a = task("a");
        a.kind = TaskKind::Worker;
        let mut b = a.clone();
        b.id = "b".into();
        b.deps.push("a".into());
        let nodes = [Node::Task(a), Node::Task(b)];
        assert!(run(&r, &nodes).unwrap_err().budget_exceeded);
        let next = GraphRunner::new(cfg).unwrap();
        run(&next, &nodes).unwrap();
        assert_eq!(next.worker_calls_used(), 1);
        let (send, recv) = mpsc::channel();
        let recv = Mutex::new(recv);
        let mut timed = task("timed");
        timed.timeout_s = Some(0.02);
        timed.run_fn = Arc::new(move |_, _| {
            let _ = lock(&recv).recv_timeout(Duration::from_secs(3));
            Ok(Value::Null)
        });
        let started = Instant::now();
        assert_eq!(
            run(&r, &[Node::Task(timed)]).unwrap_err().reason,
            "exceeded timeout_s=0.02s"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = send.send(());
        assert_eq!(r.state().nodes["timed"]["error_signature"], "timeout");
        let v = report(&r, "failed");
        assert_eq!(v["failure_signatures"][0]["node"], "timed");
        assert!(v["evolution_hints"][0]
            .as_str()
            .unwrap()
            .contains("Timeout hit"));
    }

    #[test]
    fn parallel_failures_drain_and_aggregate_packets_in_declaration_order() {
        for (split, hard) in [(false, false), (true, false), (false, true)] {
            let dir = tempdir().unwrap();
            let r = GraphRunner::new(RunnerConfig {
                max_parallel_nodes: 2,
                ..config(dir.path())
            })
            .unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let mut children = Vec::new();
            for (id, first) in [("a", true), ("b", false)] {
                let barrier = barrier.clone();
                let mut n = task(id);
                n.run_fn = Arc::new(move |_, _| {
                    barrier.wait();
                    let mut err = GraphFailure::new(id, "review").with_packets(
                        Some(if first { "primary" } else { "shared" }.into()),
                        vec![if first { "shared" } else { "other" }.into()],
                    );
                    err.awaiting_subagents = !(first && hard);
                    Err(err.into())
                });
                children.push(Node::Task(n));
            }
            if hard {
                children.push(Node::Task(task("unqueued")));
            }
            let nodes = if split {
                vec![splitter(children)]
            } else {
                children
            };
            let e = run(&r, &nodes).unwrap_err();
            assert_eq!(e.awaiting_subagents, !hard);
            assert_eq!(r.trace().len(), 2);
            if hard {
                assert!(e.reason.contains("2 parallel sibling(s) failed"));
            } else {
                assert_eq!(e.packet_paths, ["primary", "shared", "other"]);
                let p = r
                    .write_run_report(
                        None,
                        "awaiting_subagents",
                        &e.reason,
                        e.packet_path.as_deref(),
                        &e.packet_paths,
                        Some(&Map::from_iter([("workflow_ref".into(), json!("fixture"))])),
                    )
                    .unwrap();
                let v = files::read_object(&p).unwrap();
                assert_eq!(v["packet_paths"], json!(["primary", "shared", "other"]));
                assert_eq!(v["workflow_ref"], "fixture");
            }
        }
    }

    #[test]
    fn checkpoint_reuses_merged_disk_state_and_recovers_from_external_changes() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let runner = GraphRunner::new(cfg.clone()).unwrap();
        runner
            .record("local", json!({"status":"running"}), false)
            .unwrap();
        let peer_state = json!({"version":3,"nodes":{"peer":{"status":"passed"}},"label":"peer"});
        files::write_json(&cfg.state_path, &peer_state, true).unwrap();
        runner
            .record("local", json!({"status":"passed"}), true)
            .unwrap();
        runner
            .record("next", json!({"status":"passed"}), true)
            .unwrap();
        let merged = read_state(&cfg.state_path).unwrap();
        assert_eq!(merged.extra["label"], "peer");
        assert_eq!(merged.nodes["peer"]["status"], "passed");
        assert!(!runner.state().nodes.contains_key("peer"));

        files::write_text(&cfg.state_path, "{").unwrap();
        assert!(runner
            .record("broken", json!({"status":"passed"}), true)
            .is_err());
        assert!(!runner.state().nodes.contains_key("broken"));
        assert_eq!(runner.trace().len(), 2);
        fs::remove_file(&cfg.state_path).unwrap();
        runner
            .record("last", json!({"status":"passed"}), true)
            .unwrap();
        assert_eq!(
            read_state(&cfg.state_path).unwrap().nodes,
            runner.state().nodes
        );
    }

    #[test]
    fn reads_linked_worktree_head_from_common_loose_or_packed_refs() {
        let repo = tempdir().unwrap();
        let root = repo.path();
        let marker = root.join(".git");
        let dir = root.join("common/worktrees/linked");
        let common = root.join("common");
        let oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(common.join("refs/heads")).unwrap();
        fs::write(&marker, "gitdir: common/worktrees/linked\n").unwrap();
        fs::write(dir.join("commondir"), "../..\n").unwrap();
        fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(common.join("refs/heads/main"), oid).unwrap();
        assert_eq!(read_git_head(&marker).as_deref(), Some(oid));
        fs::remove_file(common.join("refs/heads/main")).unwrap();
        fs::write(
            common.join("packed-refs"),
            format!("# pack-refs\n{oid} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(read_git_head(&marker).as_deref(), Some(oid));
        fs::write(dir.join("HEAD"), oid).unwrap();
        assert_eq!(read_git_head(&marker).as_deref(), Some(oid));
    }

    #[test]
    fn independent_runners_merge_disk_state_preserving_extra_fields() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        files::write_json(
            &cfg.state_path,
            &json!({"version":3,"label":"Đà Nẵng","nodes":{}}),
            true,
        )
        .unwrap();
        let a = GraphRunner::new(cfg.clone()).unwrap();
        let b = GraphRunner::new(cfg.clone()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            for (runner, id) in [(a, "một"), (b, "hai")] {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let mut t = task(id);
                    t.run_fn = Arc::new(move |_, _| {
                        barrier.wait();
                        Ok(Value::Null)
                    });
                    run(&runner, &[Node::Task(t)]).unwrap();
                });
            }
        });
        let state = read_state(&cfg.state_path).unwrap();
        assert_eq!(state.extra["label"], "Đà Nẵng");
        assert_eq!(state.nodes["một"]["status"], "passed");
        assert_eq!(state.nodes["hai"]["status"], "passed");
        fs::write(&cfg.state_path, "{}").unwrap();
        assert!(GraphRunner::new(cfg).is_err());
    }

    #[test]
    fn running_outputs_and_post_run_errors_are_persisted() {
        for failure in ["hash", "outputs", "repair"] {
            let dir = tempdir().unwrap();
            let cfg = config(dir.path());
            let state = cfg.state_path.clone();
            let output = dir.path().join("log");
            let file = output.clone();
            let r = GraphRunner::new(cfg).unwrap();
            let mut t = task("t");
            t.outputs_fn = Arc::new(move |ctx| {
                if failure == "outputs" && ctx.get("ran").is_some() {
                    Err("metadata failed".into())
                } else {
                    Ok(vec![file.clone()])
                }
            });
            let file = output.clone();
            t.run_fn = Arc::new(move |ctx, i| {
                let live = &read_state(&state).unwrap().nodes["t"];
                assert_eq!(live["status"], "running");
                assert_eq!(live["attempts"], i);
                assert_eq!(live["outputs"], json!([file]));
                assert_eq!(live["started_at"], live["updated_at"]);
                ctx.insert("ran", json!(true));
                Ok(Value::Null)
            });
            if failure == "hash" {
                t.input_hash_fn = Arc::new(|ctx| {
                    if ctx.get("ran").is_some() {
                        Err("hash failed".into())
                    } else {
                        Ok("input".into())
                    }
                });
            }
            if failure == "repair" {
                t.max_retries = 2;
                t.verify_fn = Some(Arc::new(|_, _| VerifyOutcome::fail("sig", "bad")));
                t.repair_fn = Some(Arc::new(|_, _, _, _| Err("repair failed".into())));
            }
            let e = run(&r, &[Node::Task(t)]).unwrap_err();
            let rec = &r.state().nodes["t"];
            assert_eq!(rec["status"], "failed");
            assert_eq!(rec["attempts"], 1);
            assert_eq!(rec["detail"], e.reason);
            assert_eq!(rec["outputs"], json!([output]));
        }
    }

    #[test]
    fn events_can_reenter_runner_and_observer_panics_are_isolated() {
        let dir = tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new(OnceLock::<Weak<GraphRunner>>::new());
        let (runner_ref, seen) = (owner.clone(), events.clone());
        let hook = Arc::new(move |e: &Value| {
            let r = runner_ref.get().unwrap().upgrade().unwrap();
            assert!(r.runtime.try_lock().is_ok());
            let _ = r.trace();
            lock(&seen).push(e.clone());
            if e["event"] == "node_retry" {
                panic!("observer failed");
            }
        });
        let r = Arc::new(
            GraphRunner::new(RunnerConfig {
                on_event: Some(hook),
                ..config(dir.path())
            })
            .unwrap(),
        );
        owner.set(Arc::downgrade(&r)).unwrap();
        let mut n = task("verify");
        n.kind = TaskKind::Verifier;
        n.max_retries = 1;
        n.run_fn = Arc::new(|_, attempt| Ok(json!(attempt)));
        n.verify_fn = Some(Arc::new(|_, v| {
            if v == 0 {
                VerifyOutcome::fail("retry", "first")
            } else {
                VerifyOutcome::pass("ok")
            }
        }));
        n.efficiency_fn = Some(Arc::new(|_| EfficiencyCounts {
            agent_dispatches: 1,
            ..Default::default()
        }));
        let graph = [Node::Task(n)];
        run(&r, &graph).unwrap();
        run(&r, &graph).unwrap();
        let events = lock(&events);
        assert_eq!(
            events
                .iter()
                .map(|e| e["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "run_start",
                "node_start",
                "node_retry",
                "node_start",
                "node_passed",
                "run_end",
                "run_start",
                "node_cached",
                "run_end"
            ]
        );
        assert_eq!(events[3]["attempt"], 2);
        assert_eq!(events[4]["cache_miss_reason"], "no_prior_state");
        assert!(events
            .iter()
            .all(
                |e| chrono::DateTime::parse_from_rfc3339(e["timestamp"].as_str().unwrap()).is_ok()
            ));
        assert_eq!(r.efficiency().script_verifiers_passed, 1);
        assert_eq!(r.efficiency().agent_dispatches, 2);
    }

    #[test]
    fn dynamic_splitter_cannot_block_in_reverse_lock_order() {
        let dir = tempdir().unwrap();
        let held = Arc::new(Barrier::new(2));
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let mut owner = task("owner");
        owner.resource = Some("A".into());
        let barrier = held.clone();
        owner.run_fn = Arc::new(move |_, _| {
            barrier.wait();
            lock(&released)
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
            Ok(Value::Null)
        });
        let mut child = task("dynamic");
        child.resource = Some("A".into());
        let mut split = SplitterNode::new(
            "runtime",
            Arc::new(move |_| {
                held.wait();
                Ok(vec![Value::Null])
            }),
            Arc::new(move |_, _| Ok(Node::Task(child.clone()))),
        );
        split.resource = Some("B".into());
        let hook = Arc::new(move |e: &Value| {
            if e["node"] == "runtime/dynamic" && e["event"] == "node_failed" {
                let _ = release.send(());
            }
        });
        let r = GraphRunner::new(RunnerConfig {
            max_parallel_nodes: 2,
            on_event: Some(hook),
            ..config(dir.path())
        })
        .unwrap();
        let e = run(&r, &[Node::Task(owner), Node::Splitter(split)]).unwrap_err();
        assert_eq!(e.reason, "Cannot wait for resource 'A' while holding 'B'");
        assert_eq!(r.state().nodes["owner"]["status"], "passed");
    }

    #[test]
    fn terminal_events_include_status_and_packet_fields() {
        let dir = tempdir().unwrap();
        for (status, attempts) in [
            ("failed", 1),
            ("circuit_broken", 2),
            ("awaiting_subagents", 1),
        ] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let observed = seen.clone();
            let r = GraphRunner::new(RunnerConfig {
                on_event: Some(Arc::new(move |e| lock(&observed).push(e.clone()))),
                ..config(dir.path())
            })
            .unwrap();
            let mut leaf = task("build");
            leaf.max_retries = attempts - 1;
            leaf.run_fn = Arc::new(move |_, _| {
                if status == "awaiting_subagents" {
                    let mut error = GraphFailure::new("build", "review");
                    error.awaiting_subagents = true;
                    error.packet_path = Some("packet".into());
                    return Err(error.into());
                }
                Err(TaskRunError::with_sig("compiler", "failed to compile"))
            });
            assert_eq!(
                run(&r, &[Node::Task(leaf)]).unwrap_err().status_str(),
                status
            );
            let events = lock(&seen);
            let last = &events[events.len() - 2];
            assert_eq!(last["event"], format!("node_{status}"));
            assert_eq!(events.last().unwrap()["status"], status);
            let expected = if status == "awaiting_subagents" {
                json!(["packet"])
            } else {
                json!([])
            };
            assert_eq!(last["packet_paths"], expected);
            assert_eq!(r.state().nodes["build"]["attempts"], attempts);
        }
    }

    // Re-enter this test in two processes to exercise OS advisory locking. Each
    // process starts with an old snapshot and waits for its sibling's running write.
    #[test]
    fn telemetry_history_records_retries_cost_cache_and_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let runner = GraphRunner::new(RunnerConfig {
            telemetry_path: Some(path.clone()),
            ..config(dir.path())
        })
        .unwrap();
        let ctx = Context::new();
        ctx.insert("git_head", json!("abc"));
        ctx.insert("git_dirty", json!(false));
        ctx.insert("workflow_ref", json!("demo"));
        let mut worker = task("worker");
        worker.max_retries = 1;
        worker.run_fn = Arc::new(|_, attempt| {
            Ok(
                json!({"ok":attempt==1,"__kgraph_usage":{"model":"m","provider":"p","input_tokens":5,"cost_usd":0.25}}),
            )
        });
        worker.verify_fn = Some(Arc::new(|_, v| {
            if v["ok"] == true {
                VerifyOutcome::pass("ok")
            } else {
                VerifyOutcome::fail("lint", "try again")
            }
        }));
        let nodes = vec![Node::Task(worker)];
        runner.run(&nodes, &ctx, "nested").unwrap();
        let first = runner.build_run_telemetry("passed", 0, None);
        let n = &first.nodes[0];
        assert_eq!(n.path, "nested/worker");
        assert!(n.repaired);
        assert_eq!(n.error_signatures, ["lint"]);
        assert_eq!(n.attempts, 2);
        assert_eq!(n.cache_miss_reason.as_deref(), Some("no_prior_state"));
        assert_eq!(first.git_head.as_deref(), Some("abc"));
        assert_eq!(first.git_dirty, Some(false));
        assert_eq!(first.workflow_ref.as_deref(), Some("demo"));
        assert_eq!(first.total_tokens, 10);
        assert_eq!(first.total_cost_usd, 0.5);
        assert!(n.duration_ms > 0.);
        assert!((n.end_offset_ms - n.start_offset_ms - n.duration_ms).abs() < 0.000001);
        assert!(n.end_offset_ms <= first.wall_time_ms);
        assert!(n.start_offset_ms >= n.queue_wait_ms);
        assert_eq!(
            report(&runner, "passed")["telemetry_summary"]["run_id"],
            first.run_id
        );
        assert_eq!(runner.state().version, 3);

        runner.run(&nodes, &ctx, "nested").unwrap();
        let cached = runner.build_run_telemetry("passed", 0, Some("override"));
        assert_ne!(cached.run_id, first.run_id);
        assert_eq!(cached.workflow_ref.as_deref(), Some("override"));
        assert_eq!(cached.nodes.len(), 1);
        assert_eq!(cached.cache_hit_rate_pct, 100.);
        assert_eq!(cached.nodes[0].attempts, 0);
        assert!(cached.nodes[0].cache_miss_reason.is_none());
        assert_eq!(cached.total_tokens, 0);
        let mut bad = task("bad");
        bad.max_retries = 1;
        bad.run_fn = Arc::new(|ctx, _| {
            ctx.insert(
                "__kgraph_usage",
                json!({"nested/bad":{"input_tokens":300,"cost_usd":0.2}}),
            );
            Err(TaskRunError::other("problem"))
        });
        let e = runner.run(&[Node::Task(bad)], &ctx, "nested").unwrap_err();
        assert!(e.circuit_broken);
        let history = telemetry::read_telemetry_runs(&path, Some("demo"), None).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].run_id, first.run_id);
        assert_eq!(history[2].exit_code, e.exit_code());
        assert_eq!(history[2].status, "circuit_broken");
        assert_eq!(history[2].circuit_broken_nodes, 1);
        assert_eq!(history[2].nodes[0].error_signatures.len(), 2);
        assert_eq!(history[2].total_tokens, 300);
        assert_eq!(history[2].total_cost_usd, 0.2);
        let a = telemetry::analyze_telemetry(&history);
        assert_eq!(a.failure_burn_tokens, 305);
    }

    #[test]
    fn telemetry_measures_exclusive_wait_and_readiness_in_sequential_queue() {
        let dir = tempdir().unwrap();
        let runner = GraphRunner::new(config(dir.path())).unwrap();
        let resource = Arc::new(ResourceLock::default());
        lock(&runner.resources).insert("device".into(), resource.clone());
        assert!(resource.acquire(true, |_, _| panic!("uncontended lock waited")));
        let mut first = task("first");
        first.resource = Some("device".into());
        first.run_fn = Arc::new(|_, _| {
            thread::sleep(Duration::from_millis(20));
            Ok(Value::Null)
        });
        let mut dependent = task("dependent");
        dependent.deps.push("first".into());
        let nodes = [
            Node::Task(first),
            Node::Task(task("independent")),
            Node::Task(dependent),
        ];
        thread::scope(|scope| {
            let worker = scope.spawn(|| run(&runner, &nodes).unwrap());
            let start = Instant::now();
            while Arc::strong_count(&resource) < 3 {
                assert!(start.elapsed() < Duration::from_secs(5));
                thread::yield_now();
            }
            thread::sleep(Duration::from_millis(40));
            resource.release();
            worker.join().unwrap();
        });
        let run = runner.build_run_telemetry("passed", 0, None);
        let first = &run.nodes[0];
        assert!(first.resource_wait_ms >= 15.);
        let wait = &first.resource_waits[0];
        assert_eq!(wait.resource, "device");
        assert!(wait.start_offset_ms + wait.duration_ms <= first.start_offset_ms);
        let chrome = telemetry::export_chrome_trace(&run);
        assert_eq!(chrome["traceEvents"][0]["ts"], wait.start_offset_ms * 1000.);
        assert!(first.queue_wait_ms >= first.resource_wait_ms);
        assert!(first.duration_ms >= 15.);
        let independent = &run.nodes[1];
        let dependent = &run.nodes[2];
        assert!(independent.queue_wait_ms >= first.end_offset_ms);
        assert_eq!(independent.resource_wait_ms, 0.);
        assert!(dependent.queue_wait_ms <= dependent.start_offset_ms - first.end_offset_ms + 0.001);
    }

    #[test]
    fn telemetry_write_errors_preserve_graph_results_and_validation_failures() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let invalid_sink = dir.path().join("directory");
        fs::create_dir(&invalid_sink).unwrap();
        let invalid = GraphRunner::new(RunnerConfig {
            telemetry_path: Some(invalid_sink),
            ..cfg.clone()
        })
        .unwrap();
        run(&invalid, &[Node::Task(task("ok"))]).unwrap();
        let sink = dir.path().join("run.jsonl");
        let r = GraphRunner::new(RunnerConfig {
            telemetry_path: Some(sink.clone()),
            ..cfg
        })
        .unwrap();
        let mut bad = task("bad");
        bad.deps.push("missing".into());
        assert!(run(&r, &[Node::Task(bad)]).is_err());
        let runs = telemetry::read_telemetry_runs(&sink, None, None).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].nodes_total, 0);
    }

    #[test]
    fn cross_process_state_merge() {
        const KEY: &str = "KGRAPH_TEST_MERGE_DIRECTORY";
        if let Some(directory) = std::env::var_os(KEY) {
            let root = PathBuf::from(directory);
            let id = std::env::var("KGRAPH_TEST_MERGE_NODE").unwrap();
            let peer = if id == "one" { "two" } else { "one" };
            let runner = GraphRunner::new(config(&root)).unwrap();
            let mut leaf = task(&id);
            leaf.run_fn = Arc::new(move |_, _| {
                fs::write(root.join(&id), "").unwrap();
                let started = Instant::now();
                while !root.join(peer).exists() {
                    assert!(
                        started.elapsed() < Duration::from_secs(4),
                        "peer did not start"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(Value::Null)
            });
            run(&runner, &[Node::Task(leaf)]).unwrap();
            return;
        }
        let dir = tempdir().unwrap();
        let launch = |id| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::tests::cross_process_state_merge"])
                .env(KEY, dir.path())
                .env("KGRAPH_TEST_MERGE_NODE", id)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut one = launch("one");
        let mut two = launch("two");
        assert!(one.wait().unwrap().success());
        assert!(two.wait().unwrap().success());
        let state = read_state(&config(dir.path()).state_path).unwrap();
        assert_eq!(state.nodes["one"]["status"], "passed");
        assert_eq!(state.nodes["two"]["status"], "passed");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_dangling_state_symlink_preserves_alias() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let alias = dir.path().join("alias.json");
        std::os::unix::fs::symlink("graph_state.json", &alias).unwrap();
        let runner = GraphRunner::new(RunnerConfig {
            state_path: alias.clone(),
            ..cfg.clone()
        })
        .unwrap();
        run(&runner, &[Node::Task(task("alias"))]).unwrap();
        assert!(alias.is_symlink());
        let direct = GraphRunner::new(cfg.clone()).unwrap();
        run(&direct, &[Node::Task(task("direct"))]).unwrap();
        assert_eq!(read_state(&alias).unwrap().nodes.len(), 2);
        assert!(dir.path().join("graph_state.json.lock").exists());
        assert!(!dir.path().join("alias.json.lock").exists());
    }

    #[test]
    fn oscillation_breaker_detects_ab_a_ping_pong_and_can_be_disabled() {
        use std::sync::atomic::Ordering;
        let dir = tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut node = task("osc");
        node.max_retries = 5;
        node.verify_fn = Some(Arc::new(move |_, _| {
            let n = c.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => VerifyOutcome::fail("err_A", "first A"),
                1 => VerifyOutcome::fail("err_B", "flipped to B"),
                2 => VerifyOutcome::fail("err_A", "back to A"),
                _ => VerifyOutcome::pass("recovered"),
            }
        }));
        let runner = GraphRunner::new(config(dir.path())).unwrap();
        let err = run(&runner, &[Node::Task(node.clone())]).unwrap_err();
        assert!(err.circuit_broken);
        assert!(err.reason.contains("oscillation: err_A -> err_B -> err_A"), "{}", err.reason);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // When oscillation_breaker is disabled, A -> B -> A -> pass succeeds.
        calls.store(0, Ordering::SeqCst);
        node.retry_policy.oscillation_breaker = false;
        let dir2 = tempdir().unwrap();
        let runner2 = GraphRunner::new(config(dir2.path())).unwrap();
        run(&runner2, &[Node::Task(node)]).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn transient_retries_do_not_consume_repair_budget_or_trip_circuit_breaker() {
        use std::sync::atomic::Ordering;
        let dir = tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let mut node = task("rate_limited");
        node.max_retries = 0;
        node.retry_policy.transient_retries = 2;
        node.retry_policy.backoff_s = Some(0.001);
        node.retry_policy.jitter = false;
        node.verify_fn = Some(Arc::new(move |_, _| {
            let n = c.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                VerifyOutcome::fail("http_429", "HTTP 429 Too Many Requests")
            } else {
                VerifyOutcome::pass("succeeded after backoff")
            }
        }));
        let runner = GraphRunner::new(config(dir.path())).unwrap();
        run(&runner, &[Node::Task(node)]).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cost_token_and_wall_time_budgets_stop_execution_cleanly() {
        let dir = tempdir().unwrap();
        let mut first = task("first");
        first.run_fn = Arc::new(|_, _| {
            Ok(json!({
                "__kgraph_usage": {"input_tokens": 600, "output_tokens": 500, "cached_tokens": 0, "cost_usd": 0.75}
            }))
        });
        let mut second = task("second");
        second.deps = vec!["first".into()];

        // Token budget exceeded before second node
        let runner = GraphRunner::new(RunnerConfig {
            max_tokens: Some(1000),
            ..config(dir.path())
        })
        .unwrap();
        let err = run(&runner, &[Node::Task(first.clone()), Node::Task(second.clone())]).unwrap_err();
        assert!(err.budget_exceeded);
        assert!(err.reason.contains("Token budget exceeded"), "{}", err.reason);

        // Cost budget exceeded before second node
        let dir2 = tempdir().unwrap();
        let runner2 = GraphRunner::new(RunnerConfig {
            max_cost_usd: Some(0.50),
            ..config(dir2.path())
        })
        .unwrap();
        let err2 = run(&runner2, &[Node::Task(first), Node::Task(second)]).unwrap_err();
        assert!(err2.budget_exceeded);
        assert!(err2.reason.contains("Cost budget exceeded"), "{}", err2.reason);
    }

    #[test]
    fn conditional_when_skips_node_and_finally_runs_on_failure() {
        let dir = tempdir().unwrap();
        let ran = Arc::new(Mutex::new(Vec::<String>::new()));

        let r1 = ran.clone();
        let mut skipped = task("optional");
        skipped.when_fn = Some(Arc::new(|ctx| Ok(ctx.get("enable").and_then(|v| v.as_bool()).unwrap_or(false))));
        skipped.when_expr = Some("ctx.enable".into());
        skipped.run_fn = Arc::new(move |_, _| {
            r1.lock().unwrap().push("optional".into());
            Ok(Value::Null)
        });

        let r2 = ran.clone();
        let mut failing = task("main_work");
        failing.deps = vec!["optional".into()];
        failing.run_fn = Arc::new(move |_, _| {
            r2.lock().unwrap().push("main_work".into());
            Ok(Value::Null)
        });
        failing.verify_fn = Some(Arc::new(|_, _| VerifyOutcome::fail("boom", "main failed")));

        let r3 = ran.clone();
        let mut cleanup = task("cleanup");
        cleanup.run_fn = Arc::new(move |ctx, _| {
            let status = ctx
                .get("__kgraph_run_status")
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            r3.lock().unwrap().push(format!("cleanup:{status}"));
            Ok(Value::Null)
        });

        let runner = GraphRunner::new(RunnerConfig {
            finally_nodes: vec![Node::Task(cleanup)],
            ..config(dir.path())
        })
        .unwrap();
        let plan = runner
            .plan(&[Node::Task(skipped.clone()), Node::Task(failing.clone())], &Context::new(), "")
            .unwrap();
        assert_eq!(plan[0].status, "skipped");
        assert_eq!(plan[0].reason.as_deref(), Some("condition_false"));

        let err = run(&runner, &[Node::Task(skipped), Node::Task(failing)]).unwrap_err();
        assert_eq!(err.node_path, "main_work");
        assert_eq!(
            *ran.lock().unwrap(),
            vec!["main_work".to_string(), "cleanup:failed".to_string()]
        );
        let state = read_state(&config(dir.path()).state_path).unwrap();
        assert_eq!(state.nodes["optional"]["status"], "skipped");
        assert_eq!(state.nodes["finally/cleanup"]["status"], "passed");
    }

    #[test]
    fn protected_paths_detect_reward_hijacking() {
        let dir = tempdir().unwrap();
        let test_file = dir.path().join("protected_test.rs");
        fs::write(&test_file, "assert_eq!(2 + 2, 4);").unwrap();
        let target = test_file.clone();
        let mut node = task("sneaky_agent");
        node.protected_paths = Some(ProtectedPaths {
            base_dir: dir.path().to_path_buf(),
            paths: vec![PathBuf::from("protected_test.rs")],
            globs: vec![],
        });
        node.run_fn = Arc::new(move |_, _| {
            fs::write(&target, "assert_eq!(2 + 2, 5); // weakened test").unwrap();
            Ok(Value::Null)
        });
        let runner = GraphRunner::new(config(dir.path())).unwrap();
        let err = run(&runner, &[Node::Task(node)]).unwrap_err();
        assert!(err.reason.contains("Protected files were modified"), "{}", err.reason);
        let state = read_state(&config(dir.path()).state_path).unwrap();
        assert_eq!(state.nodes["sneaky_agent"]["error_signature"], "protected_path_modified");
    }
}
