use crate::{
    args::{Execution, Run},
    default_file, display, load, print_json, project_dir,
    publisher::Publisher,
    resolve, timeout,
    watch::{self, Outputs, Wait, Wakeups, Watch},
    Result,
};
use kgraph_core::{
    engine::{GraphRunner, RunnerConfig},
    files,
    types::EventHook,
};
use kgraph_subgraphs::declarative::LoadedWorkflow;
use kgraph_town::{registry, snapshot};
use serde_json::{json, Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn config(args: &Execution) -> RunnerConfig {
    RunnerConfig {
        state_path: args
            .state
            .clone()
            .unwrap_or_else(|| default_file(&args.workflow, "_state.json")),
        retry_nodes: args.retry.iter().cloned().collect(),
        until: args.until.clone(),
        force: args.force,
        ..Default::default()
    }
}
pub fn plan_entries(args: Execution) -> Result<Vec<kgraph_core::types::PlanEntry>> {
    let flow = load(&resolve(&args.workflow)?, &args.definition)?;
    let runner = GraphRunner::new(config(&args))?;
    runner
        .plan(&flow.nodes, &flow.context, "")
        .map_err(|e| e.to_string())
}
pub fn plan(args: Execution, json: bool) -> Result<i32> {
    let entries = plan_entries(args)?;
    if json {
        return print_json(&entries);
    }
    let mut rows = vec![["Path", "Kind", "Status", "Reason", "Resource"]
        .map(str::to_owned)
        .to_vec()];
    for e in entries {
        rows.push(vec![
            e.path,
            e.kind,
            e.status,
            e.reason.unwrap_or("-".into()),
            e.resource.unwrap_or("-".into()),
        ]);
    }
    display::table(rows);
    Ok(0)
}

#[derive(Clone)]
struct RunFiles {
    workflow: PathBuf,
    state: PathBuf,
    report: PathBuf,
    project: PathBuf,
    factory: String,
    telemetry: Option<PathBuf>,
}
impl RunFiles {
    fn register(&self, status: &str, expected: Option<&str>) -> Option<Value> {
        registry::register_run(
            &self.workflow,
            &self.state,
            &self.report,
            Some(&self.project),
            &self.factory,
            status,
            None,
            None,
            expected,
        )
        .ok()
        .flatten()
    }
    fn snapshot(
        &self,
        flow: Option<&LoadedWorkflow>,
        status: &str,
        event: Option<&Value>,
    ) -> Value {
        let mut snap = snapshot::build_town_snapshot_from_graph(
            Some(&self.workflow),
            Some(&self.state),
            Some(&self.report),
            flow.map(|f| (f.nodes.as_slice(), &f.context)),
            flow.and_then(|f| f.title.as_deref()),
            Some(status),
            Some(&self.project),
        );
        snap["project_dir"] = json!(self.project);
        snap["factory"] = json!(self.factory);
        if let Some(event) = event {
            let fields = [
                "event",
                "timestamp",
                "node",
                "kind",
                "attempt",
                "attempts",
                "duration_s",
            ];
            snap["event"] = Value::Object(
                fields
                    .into_iter()
                    .filter_map(|k| Some((k.into(), event.get(k)?.clone())))
                    .collect(),
            );
        }
        snap
    }
    fn hook(
        &self,
        flow: Option<Arc<LoadedWorkflow>>,
        hub: Option<Arc<Publisher>>,
        watch: bool,
        outputs: Arc<Mutex<Outputs>>,
        wakeups: Wakeups,
    ) -> EventHook {
        let paths = self.clone();
        let last = Mutex::new(None::<Instant>);
        Arc::new(move |event| {
            let status = match event["event"].as_str() {
                Some("node_start" | "node_passed" | "node_retry") => "running",
                Some("node_failed") => "failed",
                Some("node_circuit_broken") => "circuit_broken",
                Some("node_awaiting_subagents") => "awaiting_subagents",
                _ => return,
            };
            let mut last = last.lock().unwrap();
            if watch && status == "awaiting_subagents" {
                let packets = event["packet_paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain([&event["packet_path"]])
                    .filter_map(Value::as_str);
                watch::collect(
                    &mut outputs.lock().unwrap(),
                    packets,
                    &paths.state,
                    &paths.report,
                    &paths.project,
                    &wakeups,
                );
            }
            if let Some(hub) = &hub {
                if status != "running"
                    || last.is_none_or(|time| time.elapsed() >= Duration::from_millis(400))
                {
                    hub.submit(paths.snapshot(flow.as_deref(), status, Some(event)));
                    *last = Some(Instant::now());
                }
            }
        })
    }
}

#[derive(Default)]
struct Outcome {
    error: String,
    code: i32,
    packet: Option<String>,
    packets: Vec<String>,
}
impl Outcome {
    fn status(&self, runner: &GraphRunner) -> &'static str {
        match self.code {
            0 if runner.reached_path().is_some() => "stopped",
            0 => "passed",
            2 => "circuit_broken",
            3 => "budget_exceeded",
            4 => "awaiting_subagents",
            _ => "failed",
        }
    }
}
fn write_report(
    paths: &RunFiles,
    runner: &GraphRunner,
    result: &Outcome,
    flow: Option<&LoadedWorkflow>,
    previous: usize,
    max: Option<usize>,
) -> Result<()> {
    let mut extra = Map::new();
    extra.insert("workflow_ref".into(), json!(paths.workflow));
    extra.insert("state_file".into(), json!(paths.state));
    if let Some(p) = flow.and_then(|f| f.qa_checklist_path.as_ref()) {
        extra.insert("qa_checklist_path".into(), json!(resolve(p)?));
    }
    let write = |p: &Path| {
        runner.write_run_report(
            Some(p),
            result.status(runner),
            &result.error,
            result.packet.as_deref(),
            &result.packets,
            Some(&extra),
        )
    };
    let dest = resolve(&paths.report)?;
    if previous == 0 {
        write(&dest)?;
        return Ok(());
    }
    // Publish the already-accumulated worker accounting atomically to readers.
    let draft = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut doc = Value::Object(files::read_object(&write(
        &draft.path().join("report.json"),
    )?)?);
    doc["summary"]["worker_calls_used"] = json!(previous + runner.worker_calls_used());
    doc["summary"]["max_worker_calls"] = json!(max);
    files::write_json(&dest, &doc, true)
}
struct Iteration {
    code: i32,
    outputs: Outputs,
    id: Option<String>,
    calls: usize,
}
fn once(
    args: &Run,
    paths: &RunFiles,
    hub: Option<&Arc<Publisher>>,
    wakeups: &Wakeups,
    previous: usize,
) -> Result<Iteration> {
    let began = chrono::Utc::now();
    let started = paths.register("running", None);
    let expected = started.as_ref().and_then(|v| v["updated_at"].as_str());
    if let (Some(hub), Some(id)) = (hub, started.as_ref().and_then(|v| v["id"].as_str())) {
        hub.initialize(id);
    }
    let loaded = if args.max_parallel < 1 {
        Err("--max-parallel must be at least 1".into())
    } else if args.max_workers.is_some_and(|n| n < 0) {
        Err("--max-workers must be nonnegative".into())
    } else {
        load(&paths.workflow, &args.execution.definition).map(Arc::new)
    };
    let max_workers = args.max_workers.and_then(|n| usize::try_from(n).ok());
    let outputs = Arc::new(Mutex::new(Outputs::new()));
    let mut config = config(&args.execution);
    config.max_parallel_nodes = usize::try_from(args.max_parallel).unwrap_or(1);
    config.max_worker_calls = max_workers.map(|n| n.saturating_sub(previous));
    if args.quiet {
        config.logger = Some(Arc::new(|_| {}));
    }
    if args.watch || hub.is_some() {
        config.on_event = Some(paths.hook(
            loaded.as_ref().ok().cloned(),
            hub.cloned(),
            args.watch,
            outputs.clone(),
            wakeups.clone(),
        ));
    }
    let mut result = Outcome::default();
    let runner = match GraphRunner::new(config.clone()) {
        Ok(r) => r,
        Err(error) => {
            result.error = error;
            result.code = 1;
            // The report writer needs a runner even when the intended state is corrupt.
            let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
            config.state_path = tmp.path().join("state.json");
            GraphRunner::new(config)?
        }
    };
    if result.code == 0 {
        match &loaded {
            Err(e) => {
                result.code = 1;
                result.error = e.clone();
            }
            Ok(flow) => {
                if let Err(e) = runner.run(&flow.nodes, &flow.context, "") {
                    result = Outcome {
                        code: e.exit_code(),
                        error: e.to_string(),
                        packet: e.packet_path,
                        packets: e.packet_paths,
                    };
                }
            }
        }
    }
    let flow = loaded.as_ref().ok().map(Arc::as_ref);
    let status = result.status(&runner);
    // Append here so the workflow reference never overrides a user's workflow context.
    // This also records load failures which do not reach GraphRunner::run.
    if let Some(path) = &paths.telemetry {
        let reference = paths.workflow.to_string_lossy();
        let mut run = runner.build_run_telemetry(status, result.code, Some(&reference));
        if run.run_id.is_empty() {
            let end = chrono::Utc::now();
            run.started_at = began.to_rfc3339();
            run.finished_at = end.to_rfc3339();
            run.run_id = format!("{}-{}", run.started_at, std::process::id());
            run.wall_time_ms = (end - began).num_microseconds().unwrap_or(0) as f64 / 1000.0;
            run.state_path = paths.state.display().to_string();
        }
        if let Err(error) = kgraph_core::telemetry::append_run_telemetry(path, &run) {
            eprintln!("kgraph-town: could not record telemetry: {error}");
        }
    }
    if !result.error.is_empty() {
        eprintln!("kgraph-town: {status}: {}", result.error);
    }
    if args.watch && result.code == 4 {
        watch::collect(
            &mut outputs.lock().unwrap(),
            &result.packets,
            &paths.state,
            &paths.report,
            &paths.project,
            wakeups,
        );
    }
    if let Err(e) = write_report(paths, &runner, &result, flow, previous, max_workers) {
        paths.register("failed", expected);
        if let Some(h) = hub {
            h.finish(paths.snapshot(flow, "failed", None));
        }
        return Err(e);
    }
    paths.register(status, expected);
    let id = hub.and_then(|h| h.finish(paths.snapshot(flow, status, None)));
    if !args.quiet {
        println!("{status} | report: {}", paths.report.display());
        if result.code == 4 && result.packets.len() > 1 {
            println!("Waiting packets:");
            for path in &result.packets {
                println!("  {path}");
            }
        }
    }
    let outputs = std::mem::take(&mut *outputs.lock().unwrap());
    Ok(Iteration {
        code: result.code,
        outputs,
        id,
        calls: previous + runner.worker_calls_used(),
    })
}

// Callers resolve both paths first so aliases also compare equal before creation.
fn same_file_identity(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(left), Ok(right)) = (a.metadata(), b.metadata()) {
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
    }
    false
}

pub fn run(args: Run) -> Result<i32> {
    let workflow = resolve(&args.execution.workflow)?;
    let state = config(&args.execution).state_path;
    let report = args
        .report
        .clone()
        .unwrap_or_else(|| default_file(&args.execution.workflow, "_report.json"));
    let dest = resolve(&report)?;
    let state_dest = resolve(&state)?;
    if same_file_identity(&state_dest, &workflow) {
        return Err("--state must be separate from the workflow file".into());
    }
    if same_file_identity(&dest, &workflow) || same_file_identity(&dest, &state_dest) {
        return Err("--report must be separate from the state and workflow files".into());
    }
    let telemetry = if args.no_telemetry {
        None
    } else {
        let ledger = args
            .telemetry
            .clone()
            .unwrap_or_else(|| state.with_file_name("telemetry.jsonl"));
        let target = resolve(&ledger)?;
        if [&workflow, &state_dest, &dest]
            .into_iter()
            .any(|path| same_file_identity(&target, path))
        {
            return Err("--telemetry must be separate from the workflow, state, and report".into());
        }
        Some(ledger)
    };
    let paths = RunFiles {
        telemetry,
        workflow,
        state,
        report,
        project: project_dir()?,
        factory: args.execution.definition.factory.clone(),
    };
    if !args.watch_interval.is_finite() || args.watch_interval <= 0.0 {
        return Err("--watch-interval must be a finite, positive number".into());
    }
    let interval = Duration::try_from_secs_f64(args.watch_interval)
        .unwrap_or(Duration::MAX)
        .max(Duration::from_nanos(1));
    let limit = timeout(args.watch_timeout, "--watch-timeout")?;
    let url = args.hub_url.as_deref().filter(|s| !s.is_empty());
    let hub = url.map(|url| Publisher::new(url.into(), args.hub_token.clone()));
    let mut calls = 0;
    let mut start = None;
    let mut wakeups = Wakeups::new();
    let mut applied = None;
    let result = (|| loop {
        let iteration = once(&args, &paths, hub.as_ref(), &wakeups, calls)?;
        calls = iteration.calls;
        if iteration.code != 4 || !args.watch {
            break Ok(iteration.code);
        }
        if iteration.outputs.is_empty() && iteration.id.is_none() {
            eprintln!("kgraph-town: no packet outputs or Hero Town Hub run to watch");
            break Ok(4);
        }
        let watch = Watch {
            state: &paths.state,
            report: &paths.report,
            interval,
            limit,
            url,
            token: args.hub_token.as_deref(),
            run_id: iteration.id.as_deref(),
        };
        match watch.wait(
            &iteration.outputs,
            &mut wakeups,
            &mut applied,
            *start.get_or_insert_with(Instant::now),
        )? {
            Wait::Ready => continue,
            Wait::TimedOut => {
                eprintln!("kgraph-town: watch timed out awaiting subagents");
                break Ok(4);
            }
            Wait::Interrupted => break Ok(130),
        }
    })();
    if let Some(hub) = hub {
        hub.close();
    }
    result
}
