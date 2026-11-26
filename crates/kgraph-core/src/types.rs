//! Shared core types for KGraph Runner DAG execution, caching, and reporting.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Thread-safe workflow execution context (JSON object map).
#[derive(Clone, Default)]
pub struct Context(pub Arc<RwLock<Map<String, Value>>>);

impl Context {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(Map::new())))
    }

    pub fn from_map(map: Map<String, Value>) -> Self {
        Self(Arc::new(RwLock::new(map)))
    }

    pub fn from_value(value: Value) -> Result<Self, String> {
        match value {
            Value::Object(map) => Ok(Self::from_map(map)),
            _ => Err("Context must be a JSON object".to_string()),
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    pub fn insert(&self, key: impl Into<String>, value: Value) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.into(), value);
    }

    pub fn snapshot_map(&self) -> Map<String, Value> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn shallow_clone(&self) -> Self {
        Self::from_map(self.snapshot_map())
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Context").field(&self.snapshot_map()).finish()
    }
}

/// Canonical task kind matching Python v0.2.0 (`code | worker | verifier | gate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Code,
    Worker,
    Verifier,
    Gate,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Code => "code",
            TaskKind::Worker => "worker",
            TaskKind::Verifier => "verifier",
            TaskKind::Gate => "gate",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "code" => Some(TaskKind::Code),
            "worker" => Some(TaskKind::Worker),
            "verifier" => Some(TaskKind::Verifier),
            "gate" => Some(TaskKind::Gate),
            _ => None,
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Efficiency telemetry counters tracked on `GraphFailure` and `GraphRunner`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EfficiencyCounts {
    #[serde(default)]
    pub script_verifiers_passed: usize,
    #[serde(default)]
    pub agent_dispatches: usize,
    #[serde(default)]
    pub precheck_short_circuits: usize,
}

impl EfficiencyCounts {
    pub fn add_assign(&mut self, other: &EfficiencyCounts) {
        self.script_verifiers_passed += other.script_verifiers_passed;
        self.agent_dispatches += other.agent_dispatches;
        self.precheck_short_circuits += other.precheck_short_circuits;
    }
}

/// Verification result tuple `(ok, error_signature, detail)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOutcome {
    pub ok: bool,
    pub error_signature: String,
    pub detail: String,
}

impl VerifyOutcome {
    pub fn pass(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            error_signature: String::new(),
            detail: detail.into(),
        }
    }

    pub fn fail(sig: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            error_signature: sig.into(),
            detail: detail.into(),
        }
    }
}

/// Structured runner failure matching Python `GraphFailure`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("[{node_path}] {reason}")]
pub struct GraphFailure {
    pub node_path: String,
    pub reason: String,
    pub circuit_broken: bool,
    pub budget_exceeded: bool,
    pub awaiting_subagents: bool,
    pub packet_path: Option<String>,
    pub packet_paths: Vec<String>,
    pub efficiency: EfficiencyCounts,
}

impl GraphFailure {
    pub fn new(node_path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            node_path: node_path.into(),
            reason: reason.into(),
            circuit_broken: false,
            budget_exceeded: false,
            awaiting_subagents: false,
            packet_path: None,
            packet_paths: Vec::new(),
            efficiency: EfficiencyCounts::default(),
        }
    }

    pub fn with_packets(mut self, packet_path: Option<String>, packet_paths: Vec<String>) -> Self {
        let mut deduped = Vec::new();
        if let Some(p) = packet_path {
            if !deduped.contains(&p) {
                deduped.push(p);
            }
        }
        for p in packet_paths {
            if !deduped.contains(&p) {
                deduped.push(p);
            }
        }
        self.packet_path = deduped.first().cloned();
        self.packet_paths = deduped;
        self
    }

    pub fn status_str(&self) -> &'static str {
        if self.awaiting_subagents {
            "awaiting_subagents"
        } else if self.circuit_broken {
            "circuit_broken"
        } else if self.budget_exceeded {
            "budget_exceeded"
        } else {
            "failed"
        }

    }

    pub fn exit_code(&self) -> i32 {
        if self.circuit_broken {
            2
        } else if self.budget_exceeded {
            3
        } else if self.awaiting_subagents {
            4
        } else {
            1
        }
    }
}

/// Outcome returned by a `TaskNode::run_fn`.
pub type TaskResult = Result<Value, TaskRunError>;

/// Error returned by a task callback (either a control-flow `GraphFailure` or generic failure).
#[derive(Debug, Clone, thiserror::Error)]
pub enum TaskRunError {
    #[error("{0}")]
    Graph(Box<GraphFailure>),
    #[error("{0}")]
    Timeout(String),
    #[error("{signature}: {detail}")]
    Other { signature: String, detail: String },
}

impl From<GraphFailure> for TaskRunError {
    fn from(failure: GraphFailure) -> Self {
        Self::Graph(Box::new(failure))
    }
}

impl TaskRunError {
    pub fn other(detail: impl Into<String>) -> Self {
        Self::Other {
            signature: "exception:Error".to_string(),
            detail: detail.into(),
        }
    }

    pub fn with_sig(signature: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Other {
            signature: signature.into(),
            detail: detail.into(),
        }
    }
}

pub type RunFn = Arc<dyn Fn(&Context, usize) -> TaskResult + Send + Sync>;
pub type InputHashFn = Arc<dyn Fn(&Context) -> Result<String, String> + Send + Sync>;
pub type OutputsFn = Arc<dyn Fn(&Context) -> Result<Vec<PathBuf>, String> + Send + Sync>;
pub type VerifyFn = Arc<dyn Fn(&Context, &Value) -> VerifyOutcome + Send + Sync>;
pub type RepairFn = Arc<dyn Fn(&Context, usize, &str, &str) -> Result<(), String> + Send + Sync>;
pub type EfficiencyFn = Arc<dyn Fn(&Value) -> EfficiencyCounts + Send + Sync>;
pub type LoopVerifyFn = Arc<dyn Fn(&Context) -> Result<VerifyOutcome, String> + Send + Sync>;
pub type LoopRepairFn =
    Arc<dyn Fn(&Context, usize, &str, &str) -> Result<Vec<String>, GraphFailure> + Send + Sync>;
pub type SplitterItemsFn = Arc<dyn Fn(&Context) -> Result<Vec<Value>, String> + Send + Sync>;
pub type SplitterChildBuilderFn = Arc<dyn Fn(&Value, &Context) -> Result<Node, String> + Send + Sync>;
pub type EventHook = Arc<dyn Fn(&Value) + Send + Sync>;
pub type LoggerFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Optional visual retry routing metadata attached to a `LoopSubgraphNode`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryRoute {
    pub from: String,
    pub to: String,
    pub label: String,
}

#[derive(Clone)]
pub struct TaskNode {
    pub id: String,
    pub kind: TaskKind,
    pub run_fn: RunFn,
    pub input_hash_fn: InputHashFn,
    pub outputs_fn: OutputsFn,
    pub verify_fn: Option<VerifyFn>,
    pub repair_fn: Option<RepairFn>,
    pub max_retries: usize,
    pub timeout_s: Option<f64>,
    pub deps: Vec<String>,
    pub cacheable: bool,
    pub resource: Option<String>,
    pub efficiency_fn: Option<EfficiencyFn>,
    pub qa_checklist_path: Option<PathBuf>,
}

impl TaskNode {
    pub fn new(id: impl Into<String>, kind: TaskKind, run_fn: RunFn, input_hash_fn: InputHashFn) -> Self {
        Self {
            id: id.into(),
            kind,
            run_fn,
            input_hash_fn,
            outputs_fn: Arc::new(|_| Ok(Vec::new())),
            verify_fn: None,
            repair_fn: None,
            max_retries: 0,
            timeout_s: None,
            deps: Vec::new(),
            cacheable: true,
            resource: None,
            efficiency_fn: None,
            qa_checklist_path: None,
        }
    }
}

#[derive(Clone)]
pub struct SubgraphNode {
    pub id: String,
    pub nodes: Vec<Node>,
    pub deps: Vec<String>,
    pub resource: Option<String>,
    pub qa_checklist_path: Option<PathBuf>,
}

impl SubgraphNode {
    pub fn new(id: impl Into<String>, nodes: Vec<Node>) -> Self {
        Self {
            id: id.into(),
            nodes,
            deps: Vec::new(),
            resource: None,
            qa_checklist_path: None,
        }
    }
}

#[derive(Clone)]
pub struct LoopSubgraphNode {
    pub id: String,
    pub nodes: Vec<Node>,
    pub verify_fn: LoopVerifyFn,
    pub repair_fn: Option<LoopRepairFn>,
    pub max_retries: usize,
    pub deps: Vec<String>,
    pub resource: Option<String>,
    pub retry_route: Option<RetryRoute>,
    pub qa_checklist_path: Option<PathBuf>,
}

impl LoopSubgraphNode {
    pub fn new(id: impl Into<String>, nodes: Vec<Node>, verify_fn: LoopVerifyFn) -> Self {
        Self {
            id: id.into(),
            nodes,
            verify_fn,
            repair_fn: None,
            max_retries: 2,
            deps: Vec::new(),
            resource: None,
            retry_route: None,
            qa_checklist_path: None,
        }
    }
}

#[derive(Clone)]
pub struct SplitterNode {
    pub id: String,
    pub items_fn: SplitterItemsFn,
    pub child_builder_fn: SplitterChildBuilderFn,
    pub deps: Vec<String>,
    pub max_workers: usize,
    pub resource: Option<String>,
    pub qa_checklist_path: Option<PathBuf>,
}

impl SplitterNode {
    pub fn new(
        id: impl Into<String>,
        items_fn: SplitterItemsFn,
        child_builder_fn: SplitterChildBuilderFn,
    ) -> Self {
        Self {
            id: id.into(),
            items_fn,
            child_builder_fn,
            deps: Vec::new(),
            max_workers: 4,
            resource: None,
            qa_checklist_path: None,
        }
    }
}

#[derive(Clone)]
pub enum Node {
    Task(TaskNode),
    Subgraph(SubgraphNode),
    LoopSubgraph(LoopSubgraphNode),
    Splitter(SplitterNode),
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Task(n) => &n.id,
            Node::Subgraph(n) => &n.id,
            Node::LoopSubgraph(n) => &n.id,
            Node::Splitter(n) => &n.id,
        }
    }

    pub fn deps(&self) -> &[String] {
        match self {
            Node::Task(n) => &n.deps,
            Node::Subgraph(n) => &n.deps,
            Node::LoopSubgraph(n) => &n.deps,
            Node::Splitter(n) => &n.deps,
        }
    }

    pub fn resource(&self) -> Option<&str> {
        match self {
            Node::Task(n) => n.resource.as_deref(),
            Node::Subgraph(n) => n.resource.as_deref(),
            Node::LoopSubgraph(n) => n.resource.as_deref(),
            Node::Splitter(n) => n.resource.as_deref(),
        }
    }

    pub fn kind_str(&self) -> &'static str {
        match self {
            Node::Task(n) => n.kind.as_str(),
            Node::Subgraph(_) => "subgraph",
            Node::LoopSubgraph(_) => "loop_subgraph",
            Node::Splitter(_) => "splitter",
        }
    }

    pub fn node_type_name(&self) -> &'static str {
        match self {
            Node::Task(_) => "TaskNode",
            Node::Subgraph(_) => "SubgraphNode",
            Node::LoopSubgraph(_) => "LoopSubgraphNode",
            Node::Splitter(_) => "SplitterNode",
        }
    }

    pub fn snapshot_node_type(&self) -> &'static str {
        match self {
            Node::Task(_) => "task",
            Node::Subgraph(_) => "subgraph",
            Node::LoopSubgraph(_) => "loop_subgraph",
            Node::Splitter(_) => "splitter",
        }
    }

    pub fn qa_checklist_path(&self) -> Option<&PathBuf> {
        match self {
            Node::Task(n) => n.qa_checklist_path.as_ref(),
            Node::Subgraph(n) => n.qa_checklist_path.as_ref(),
            Node::LoopSubgraph(n) => n.qa_checklist_path.as_ref(),
            Node::Splitter(n) => n.qa_checklist_path.as_ref(),
        }
    }
}

/// Dry-run plan entry returned by `GraphRunner::plan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub path: String,
    pub id: String,
    pub node_type: String,
    pub kind: String,
    pub deps: Vec<String>,
    pub resource: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Persisted state file schema (`version: 3`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphState {
    pub version: u32,
    pub nodes: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for GraphState {
    fn default() -> Self {
        Self {
            version: 3,
            nodes: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

/// Helper to discover a unique declared `qa_checklist_path` across top-level nodes.
pub fn graph_qa_checklist(nodes: &[Node]) -> Option<PathBuf> {
    let mut found: Option<&PathBuf> = None;
    for node in nodes {
        if let Some(p) = node.qa_checklist_path() {
            match found {
                None => found = Some(p),
                Some(prev) if prev == p => {}
                Some(_) => return None,
            }
        }
    }
    found.cloned()
}
