//! Command-line entry points shared by the native kgraph and kgraph-town binaries.
mod analyze;
mod args;
mod display;
mod execution;
mod init;
pub mod mcp;
mod publisher;
mod response;
mod swarm;
mod town;
mod watch;

use args::{Cli, Command};
use clap::Parser;
use kgraph_subgraphs::declarative::{load_declarative_workflow, LoadedWorkflow};
use serde::Serialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, String>;

pub fn run_cli() -> i32 {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();
    }
    run_cli_from(std::env::args_os())
}

/// Parse an argv sequence including argv[0] and return the process exit code.
pub fn run_cli_from<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = i32::from(error.use_stderr());
            let _ = error.print();
            return code;
        }
    };
    dispatch(cli.command).unwrap_or_else(|error| {
        eprintln!("kgraph-town: {error}");
        1
    })
}

fn dispatch(cmd: Command) -> Result<i32> {
    match cmd {
        Command::Run(args) => execution::run(args),
        Command::Plan { execution, json } => execution::plan(execution, json),
        Command::Analyze(args) => analyze::display(args),
        Command::Workboard(args) => swarm::display_board(args),
        Command::Claim(args) => swarm::display_claim(args),
        Command::Heartbeat { lease, ttl } => {
            swarm::display_lease(&lease, swarm::heartbeat(&lease, ttl)?)
        }
        Command::Release { lease, reason } => {
            swarm::display_lease(&lease, swarm::release(&lease, reason.as_deref())?)
        }
        Command::Complete { lease, summary } => {
            swarm::display_lease(&lease, swarm::complete(&lease, summary.as_deref())?)
        }
        Command::Mcp { once } => mcp::serve(once),
        Command::Init {
            template,
            output,
            dir,
            force,
        } => init::scaffold(template, &output, &dir, force),
        Command::Status { state, json } => display::status(&state, json),
        Command::Report { report, json } => display::report(&report, json),
        Command::Qa { action } => display::qa(action),
        Command::Ui {
            town,
            serve,
            layout,
            read_only,
        } => town::ui(town, serve, layout.mode(), read_only),
        Command::Hub {
            serve,
            watch_dir,
            action,
        } => town::hub(serve, watch_dir, action),
        Command::Render {
            town,
            layout,
            output,
            embed,
        } => town::render(town, layout.mode(), output, embed),
        Command::Push {
            town,
            publish,
            hub_url,
        } => town::push(town, default_url(hub_url.as_deref()), &publish),
        Command::Progress(args) => town::progress(args),
        Command::Ask(args) => town::ask(args),
        Command::Answer {
            state,
            node,
            item,
            run_id,
            hub_url,
            poll,
        } => response::answer(
            &state,
            &node,
            &item,
            run_id.as_deref(),
            hub_url.as_deref(),
            poll,
        ),
        Command::PullResponse {
            state,
            run_id,
            hub_url,
            report,
            no_apply,
            poll,
            ..
        } => response::pull(
            &state,
            &run_id,
            default_url(hub_url.as_deref()),
            report.as_deref(),
            !no_apply,
            poll,
        ),
    }
}
fn default_url(url: Option<&str>) -> &str {
    url.filter(|u| !u.is_empty())
        .unwrap_or("http://127.0.0.1:8765")
}
fn load(path: &Path, args: &args::Definition) -> Result<LoadedWorkflow> {
    if is_python(path) {
        return Err("Native Rust kgraph executes declarative .toml/.json workflows. Use the Python runner for .py workflows. ui, render, status, report, progress, ask, answer, qa, hub and push work with existing .kgraph files from either runner.".into());
    }
    load_declarative_workflow(path, args.ctx.as_deref(), args.ctx_file.as_deref())
}
fn is_python(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("py"))
}
fn default_file(workflow: &Path, suffix: &str) -> PathBuf {
    let mut name = workflow.file_stem().unwrap_or_default().to_owned();
    name.push(suffix);
    Path::new(".kgraph").join(name)
}
fn report_for_state(state: &Path) -> PathBuf {
    let stem = state.file_stem().unwrap_or_default().to_string_lossy();
    state.with_file_name(if stem == "state" {
        "report.json".into()
    } else {
        format!(
            "{}_report.json",
            stem.strip_suffix("_state").unwrap_or(&stem)
        )
    })
}
fn resolve(path: &Path) -> Result<PathBuf> {
    kgraph_town::interactions::resolve(path).map_err(|e| format!("{}: {e}", path.display()))
}
fn project_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|e| e.to_string())
}
fn print_json(value: &impl Serialize) -> Result<i32> {
    use std::fmt::Write;
    let raw = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let Some(first_escape) = raw.bytes().position(|b| b >= 127) else {
        println!("{raw}");
        return Ok(0);
    };
    let mut escaped = String::with_capacity(raw.len());
    escaped.push_str(&raw[..first_escape]);
    for c in raw[first_escape..].chars() {
        if c < '\x7f' {
            escaped.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                write!(escaped, "\\u{unit:04x}").expect("writing to a String cannot fail");
            }
        }
    }
    println!("{escaped}");
    Ok(0)
}
fn print_unicode_json(value: &impl Serialize) -> Result<i32> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(0)
}
fn timeout(seconds: Option<f64>, name: &str) -> Result<Option<Duration>> {
    seconds
        .map(|s| {
            if !s.is_finite() || s < 0.0 {
                return Err(format!("{name} must be a finite, nonnegative number"));
            }
            Ok(Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX))
        })
        .transpose()
}
fn remaining(start: Instant, limit: Option<Duration>) -> Duration {
    limit.map_or(Duration::MAX, |limit| limit.saturating_sub(start.elapsed()))
}
