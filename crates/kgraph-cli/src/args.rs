use clap::{builder::PossibleValuesParser, Args, Parser, Subcommand};
use kgraph_core::qa_metrics::{GAP_TYPES, MANUAL_QA_VALUES};
use kgraph_town::interactions::PROGRESS_STATUSES;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Run resumable workflows and inspect Hero Town telemetry")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Execute a declarative TOML or JSON workflow.
    Run(Run),
    /// Preview workflow execution and cache decisions.
    Plan {
        #[command(flatten)]
        execution: Execution,
        #[arg(long)]
        json: bool,
    },
    /// Analyze the telemetry ledger across workflow runs.
    #[command(visible_alias = "telemetry")]
    Analyze(Analyze),
    /// List waiting packets and active leases.
    Workboard(Board),
    /// Claim a waiting packet.
    Claim(Claim),
    /// Extend a worker's lease.
    Heartbeat {
        #[command(flatten)]
        lease: Lease,
        #[arg(long, default_value_t = 300)]
        ttl: u64,
    },
    /// Return a leased packet to the workboard.
    Release {
        #[command(flatten)]
        lease: Lease,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Complete a lease and record Hero Town progress.
    Complete {
        #[command(flatten)]
        lease: Lease,
        #[arg(long)]
        summary: Option<String>,
    },
    /// Serve MCP tools over standard input and output.
    Mcp {
        // Internal worker for workflow commands which inherit standard input.
        #[arg(long, hide = true)]
        once: bool,
    },
    /// Scaffold a declarative workflow for a workspace.
    Init {
        #[arg(value_enum, default_value = "ci")]
        template: Template,
        /// Workflow path, relative to --dir unless absolute.
        #[arg(long, default_value = ".kgraph/workflow.toml")]
        output: PathBuf,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Record manual QA and review quality metrics.
    Qa {
        #[command(subcommand)]
        action: Qa,
    },
    /// Serve the live Hero Town visualizer.
    Ui {
        #[command(flatten)]
        town: Town,
        #[command(flatten)]
        serve: Serve,
        #[command(flatten)]
        layout: Layout,
        #[arg(long)]
        read_only: bool,
    },
    /// Serve or list registered runs in the multi-project hub.
    Hub {
        #[command(flatten)]
        serve: Serve,
        #[arg(long)]
        watch_dir: Vec<PathBuf>,
        #[command(subcommand)]
        action: Option<HubAction>,
    },
    /// Write a self-contained Hero Town page.
    Render {
        #[command(flatten)]
        town: Town,
        #[command(flatten)]
        layout: Layout,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        embed: bool,
    },
    /// Publish a workflow snapshot to the hub.
    Push {
        #[command(flatten)]
        town: Town,
        #[command(flatten)]
        publish: Publish,
        #[arg(long, env = "KGRAPH_HUB_URL")]
        hub_url: Option<String>,
    },
    /// Display persisted node states.
    Status {
        state: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Update a live sub-task.
    Progress(Progress),
    /// Request a sub-task decision.
    Ask(Ask),
    /// Read a local or remote sub-task decision.
    Answer {
        state: PathBuf,
        #[arg(long)]
        node: String,
        #[arg(long)]
        item: String,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long, env = "KGRAPH_HUB_URL")]
        hub_url: Option<String>,
        #[command(flatten)]
        poll: Poll,
    },
    /// Retrieve a queued hub response and apply it locally.
    PullResponse {
        state: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long, env = "KGRAPH_HUB_URL")]
        hub_url: Option<String>,
        #[arg(long)]
        report: Option<PathBuf>,
        #[arg(long, conflicts_with = "no_apply")]
        apply: bool,
        #[arg(long)]
        no_apply: bool,
        #[command(flatten)]
        poll: Poll,
    },
    /// Summarize a run report.
    Report {
        report: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Clone, Default)]
pub struct Definition {
    #[arg(long, default_value = "build_graph")]
    pub factory: String,
    #[arg(long)]
    pub ctx: Option<String>,
    #[arg(long)]
    pub ctx_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct Execution {
    pub workflow: PathBuf,
    #[arg(long)]
    pub state: Option<PathBuf>,
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub retry: Vec<String>,
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub definition: Definition,
}

#[derive(Args)]
pub struct Run {
    #[arg(long, conflicts_with = "no_telemetry")]
    pub telemetry: Option<PathBuf>,
    #[arg(long)]
    pub no_telemetry: bool,
    #[command(flatten)]
    pub execution: Execution,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub max_parallel: isize,
    #[arg(long, allow_negative_numbers = true)]
    pub max_workers: Option<isize>,
    #[arg(long, env = "KGRAPH_HUB_URL")]
    pub hub_url: Option<String>,
    #[arg(long, env = "KGRAPH_HUB_TOKEN", hide_env_values = true)]
    pub hub_token: Option<String>,
    #[arg(long)]
    pub quiet: bool,
    #[arg(long)]
    pub watch: bool,
    #[arg(long, default_value_t = 0.5, allow_negative_numbers = true)]
    pub watch_interval: f64,
    #[arg(long, allow_negative_numbers = true)]
    pub watch_timeout: Option<f64>,
}

#[derive(Args, Clone)]
pub struct Town {
    pub workflow: Option<PathBuf>,
    #[arg(long)]
    pub state: Option<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[command(flatten)]
    pub definition: Definition,
    #[arg(long)]
    pub title: Option<String>,
}

#[derive(Args)]
pub struct Layout {
    #[arg(long, value_parser=["grid","iso","strip","compact","linear"], default_value="grid")]
    pub layout: String,
}

impl Layout {
    pub fn mode(&self) -> &str {
        match self.layout.as_str() {
            "compact" => "iso",
            "linear" => "strip",
            mode => mode,
        }
    }
}

#[derive(Args)]
pub struct Serve {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 8765)]
    pub port: u16,
    #[arg(long, default_value_t = 1200)]
    pub poll_ms: u64,
    #[arg(long, conflicts_with_all=["auth","no_auth"])]
    pub token: Option<String>,
    #[arg(long, conflicts_with = "no_auth")]
    pub auth: bool,
    #[arg(long)]
    pub no_auth: bool,
    #[arg(long)]
    pub open: bool,
    #[arg(long)]
    pub lan: bool,
    #[arg(long)]
    pub tunnel: bool,
}

#[derive(Subcommand)]
pub enum HubAction {
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Default, Clone)]
pub struct Publish {
    #[arg(long, env = "KGRAPH_HUB_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    #[arg(long)]
    pub run_id: Option<String>,
    #[arg(long)]
    pub project_name: Option<String>,
    #[arg(long)]
    pub host_label: Option<String>,
}

#[derive(Args)]
pub struct Update {
    pub state: PathBuf,
    #[arg(long)]
    pub node: String,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub detail: Option<String>,
    #[arg(long)]
    pub dep: Vec<String>,
    #[arg(long)]
    pub option: Vec<String>,
    #[arg(long)]
    pub recommended: Option<String>,
    #[arg(long)]
    pub ticket: Option<String>,
    #[arg(long)]
    pub push: bool,
    #[arg(long)]
    pub workflow: Option<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long)]
    pub hub_url: Option<String>,
    #[command(flatten)]
    pub publish: Publish,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct Progress {
    #[command(flatten)]
    pub update: Update,
    #[arg(long)]
    pub item: Option<String>,
    #[arg(long, value_parser=PossibleValuesParser::new(PROGRESS_STATUSES.iter().copied()))]
    pub status: Option<String>,
    #[arg(long, value_parser=["code","worker","verifier","gate"])]
    pub kind: Option<String>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub progress: Option<f64>,
    #[arg(long)]
    pub output: Vec<String>,
    #[arg(long)]
    pub clear: bool,
    #[arg(long)]
    pub question: Option<String>,
}

#[derive(Args)]
pub struct Ask {
    #[command(flatten)]
    pub update: Update,
    #[arg(long)]
    pub item: String,
    #[arg(
        long,
        required_unless_present = "questions_json",
        conflicts_with = "questions_json"
    )]
    pub question: Option<String>,
    #[arg(long)]
    pub questions_json: Option<String>,
    #[arg(long)]
    pub multi_select: bool,
    #[arg(long)]
    pub no_note: bool,
    #[arg(long, default_value = "Reviewer")]
    pub agent: String,
}

#[derive(Args)]
pub struct Poll {
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value_t = 500)]
    pub poll_ms: u64,
    #[arg(long, allow_negative_numbers = true)]
    pub timeout: Option<f64>,
    #[arg(long, env = "KGRAPH_HUB_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct History {
    #[arg(long)]
    pub history: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}
#[derive(Args)]
pub struct Checklist {
    pub checklist: PathBuf,
    #[arg(long)]
    pub record_history: bool,
    #[command(flatten)]
    pub history: History,
}
#[derive(Subcommand)]
pub enum Qa {
    Record {
        #[command(flatten)]
        checklist: Checklist,
        #[arg(long)]
        case: String,
        #[arg(long, value_parser=PossibleValuesParser::new(MANUAL_QA_VALUES.iter().copied()))]
        result: String,
        #[arg(long)]
        attempt: Option<u64>,
        #[arg(long, value_parser=PossibleValuesParser::new(GAP_TYPES.iter().copied()))]
        gap: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        tester: Option<String>,
        #[arg(long)]
        scorecard: bool,
    },
    Scorecard {
        #[command(flatten)]
        checklist: Checklist,
        #[arg(long)]
        json_out: Option<PathBuf>,
        #[arg(long)]
        md_out: Option<PathBuf>,
    },
    History(History),
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub enum Template {
    Ci,
    AgentLoop,
    SwarmSplitter,
    BugFix,
    BugBatch,
    Release,
}

#[derive(Args)]
pub struct Analyze {
    #[arg(default_value = ".kgraph/telemetry.jsonl")]
    pub telemetry_path: PathBuf,
    #[arg(long)]
    pub workflow: Option<String>,
    #[arg(long)]
    pub last: Option<usize>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub md_out: Option<PathBuf>,
    #[arg(long)]
    pub chrome_trace: Option<PathBuf>,
}
#[derive(Args)]
pub struct Board {
    pub state: PathBuf,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}
#[derive(Args)]
pub struct Claim {
    #[command(flatten)]
    pub board: Board,
    #[arg(long)]
    pub worker: String,
    #[arg(long)]
    pub node: Option<String>,
    #[arg(long, default_value_t = 300)]
    pub ttl: u64,
    #[arg(long)]
    pub worktree: Option<PathBuf>,
}
#[derive(Args)]
pub struct Lease {
    pub state: PathBuf,
    #[arg(long)]
    pub node: String,
    #[arg(long)]
    pub worker: String,
    #[arg(long)]
    pub json: bool,
}
