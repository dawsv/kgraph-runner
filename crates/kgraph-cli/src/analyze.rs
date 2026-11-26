use crate::{args::Analyze, print_json, resolve, Result};
use kgraph_core::{
    files::{file_lock, write_json, write_text},
    telemetry::*,
};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{ErrorKind, Read},
    path::Path,
};

// Telemetry is JSON Lines, so the town reader for single JSON objects cannot parse it.
fn bounded_runs(path: &Path, limit: usize) -> Result<Vec<RunTelemetry>> {
    let context = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        result => result.map_err(context)?,
    };
    let metadata = file.metadata().map_err(context)?;
    if !metadata.is_file() {
        return Err(format!("{}: Expected a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    {
        // Coordinate the snapshot with append_run_telemetry's ledger lock.
        let mut lock = path.as_os_str().to_owned();
        lock.push(".lock");
        let _guard = file_lock(Path::new(&lock))?;
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(context)?;
    }
    if bytes.len() > limit {
        return Err(format!(
            "{}: Telemetry file exceeds the size limit",
            path.display()
        ));
    }
    let text = std::str::from_utf8(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line)
                .map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))
        })
        .collect()
}

pub fn analyze(args: &Analyze, file_limit: Option<usize>) -> Result<TelemetryAnalysis> {
    let ledger = resolve(&args.telemetry_path)?;
    let mut destinations = vec![ledger];
    for path in [&args.md_out, &args.chrome_trace].into_iter().flatten() {
        let target = resolve(path)?;
        if destinations.contains(&target) {
            return Err("Telemetry input and export destinations must be separate files".into());
        }
        destinations.push(target);
    }
    let mut runs = match file_limit {
        Some(limit) => bounded_runs(&destinations[0], limit)?,
        None => read_telemetry_runs(
            &args.telemetry_path,
            None,
            args.last.filter(|_| args.workflow.is_none()),
        )?,
    };
    if let Some(filter) = &args.workflow {
        runs.retain(|r| {
            r.workflow_ref
                .as_ref()
                .is_some_and(|name| name.contains(filter))
        });
    }
    if let Some(last) = args.last {
        runs.drain(..runs.len().saturating_sub(last));
    }
    if let Some(path) = &args.chrome_trace {
        let latest = runs
            .last()
            .ok_or("No matching telemetry run to export as a Chrome trace")?;
        write_json(path, &export_chrome_trace(latest), true)?;
    }
    let analysis = analyze_telemetry(&runs);
    if let Some(path) = &args.md_out {
        write_text(path, &render_analysis_markdown(&analysis))?;
    }
    Ok(analysis)
}
pub fn as_json(analysis: &TelemetryAnalysis) -> Value {
    let mut value = json!(analysis);
    value["runs_analyzed"] = json!(analysis.runs_total);
    value
}
pub fn display(args: Analyze) -> Result<i32> {
    let analysis = analyze(&args, None)?;
    if args.json {
        print_json(&as_json(&analysis))
    } else {
        if analysis.runs_total == 0 {
            println!("No matching telemetry runs.");
        } else {
            print!("{}", render_analysis_text(&analysis));
        }
        Ok(0)
    }
}
