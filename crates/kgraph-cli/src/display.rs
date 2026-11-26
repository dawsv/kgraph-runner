use crate::{args::Qa, print_json, print_unicode_json, Result};
use kgraph_core::{files::read_object, qa_metrics::*};
use serde_json::{json, Value};
use std::path::Path;

pub fn table(rows: Vec<Vec<String>>) {
    let mut widths = vec![0; rows[0].len()];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in rows {
        let cells = row
            .iter()
            .zip(&widths)
            .map(|(s, w)| format!("{s:<w$}"))
            .collect::<Vec<_>>();
        println!("{}", cells.join("  ").trim_end());
    }
}
fn strings<const N: usize>(list: [&str; N]) -> Vec<String> {
    list.into_iter().map(str::to_owned).collect()
}
fn field(value: &Value, default: &str) -> String {
    match value {
        Value::Null => default.into(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
pub fn status(path: &Path, json: bool) -> Result<i32> {
    let data = read_object(path)?;
    if json {
        return print_json(&data);
    }
    let Some(nodes) = data.get("nodes") else {
        println!("No recorded nodes.");
        return Ok(0);
    };
    let nodes = nodes.as_object().ok_or("State nodes must be an object")?;
    if nodes.is_empty() {
        println!("No recorded nodes.");
        return Ok(0);
    }
    let mut rows = vec![strings([
        "Path",
        "Kind",
        "Status",
        "Attempts",
        "Duration (s)",
        "Detail / error signature",
    ])];
    let mut sorted: Vec<_> = nodes.iter().collect();
    sorted.sort_by_key(|(key, _)| *key);
    for (path, node) in sorted {
        if !node.is_object() {
            return Err(format!("State entry {path} must be an object"));
        }
        let mut row = vec![path.clone()];
        row.extend(
            ["kind", "status", "attempts", "duration_s"]
                .map(|k| node.get(k).map(|v| field(v, "None")).unwrap_or("-".into())),
        );
        let detail = ["detail", "error_signature"]
            .iter()
            .filter_map(|k| node[k].as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        row.push(detail.lines().collect::<Vec<_>>().join(" "));
        rows.push(row);
    }
    table(rows);
    Ok(0)
}

pub fn report(path: &Path, json: bool) -> Result<i32> {
    let data = Value::Object(read_object(path)?);
    if json {
        return print_json(&data);
    }
    println!("Status: {}", field(&data["status"], "unknown"));
    for (key, label) in [("error", "Error"), ("packet_path", "Packet")] {
        if let Some(s) = data[key].as_str().filter(|s| !s.is_empty()) {
            println!("{label}: {s}");
        }
    }
    for p in list(&data["packet_paths"]) {
        if p != &data["packet_path"] {
            println!("Packet: {}", field(p, "None"));
        }
    }
    let scope = &data["scope"];
    let dash = |v: &Value| {
        v.as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("-")
            .to_owned()
    };
    println!(
        "Scope: until={}, reached={}, last completed={}, nodes={}",
        dash(&scope["requested_until"]),
        dash(&data["reached_path"]),
        dash(&data["last_completed_stage"]),
        list(&scope["reached_nodes"]).len()
    );
    let summary = &data["summary"];
    let counts = summary
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, v)| v.is_i64() || v.is_u64())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("Summary: {}", if counts.is_empty() { "-" } else { &counts });
    let retries = list(&summary["retried_nodes"]);
    if !retries.is_empty() {
        println!(
            "Retried nodes: {}",
            retries
                .iter()
                .map(|v| field(v, "-"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for (key, label) in [
        ("failure_signatures", "Failure signatures"),
        ("slowest_nodes", "Slowest nodes"),
        ("evolution_hints", "Evolution hints"),
    ] {
        println!("{label}:");
        let entries = list(&data[key]);
        if entries.is_empty() {
            println!("  -");
        }
        for e in entries {
            match key {
                "failure_signatures" => println!(
                    "  {} [{}] {}",
                    field(&e["node"], "-"),
                    e["error_signature"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| e["status"].as_str().unwrap_or("-")),
                    field(&e["detail"], "")
                ),
                "slowest_nodes" => println!(
                    "  {}  {}s  ({} attempts)",
                    field(&e["node"], "-"),
                    field(&e["duration_s"], "-"),
                    field(&e["attempts"], "-")
                ),
                _ => println!("  {}", field(e, "")),
            }
        }
    }
    Ok(0)
}
fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_default()
}

pub fn qa(cmd: Qa) -> Result<i32> {
    match cmd {
        Qa::Record {
            checklist: c,
            case,
            result,
            attempt,
            gap,
            notes,
            tester,
            scorecard,
        } => {
            let data = record_qa_result(
                &c.checklist,
                &QaResultInput {
                    case_id: case.clone(),
                    manual_qa: result,
                    resolved_on_attempt: attempt,
                    gap_type: gap,
                    notes,
                },
                tester.as_deref(),
            )?;
            let card = if scorecard || c.record_history {
                Some(write_qa_scorecard(
                    &c.checklist,
                    None,
                    None,
                    c.record_history,
                    c.history.history.as_deref(),
                )?)
            } else {
                None
            };
            if c.history.json {
                let mut data = json!(data);
                if let Some(card) = card {
                    data["scorecard"] = json!(card);
                }
                return print_unicode_json(&data);
            }
            let row = data
                .cases
                .iter()
                .find(|x| x.id == case.trim())
                .ok_or("Updated case missing")?;
            println!(
                "Recorded {} for {}{}",
                row.manual_qa,
                row.id,
                row.resolved_on_attempt
                    .map(|n| format!(" (attempt {n})"))
                    .unwrap_or_default()
            );
        }
        Qa::Scorecard {
            checklist: c,
            json_out,
            md_out,
        } => {
            let card = write_qa_scorecard(
                &c.checklist,
                json_out.as_deref(),
                md_out.as_deref(),
                c.record_history,
                c.history.history.as_deref(),
            )?;
            if c.history.json {
                return print_unicode_json(&card);
            }
            print!("{}", render_qa_scorecard_markdown(&card));
        }
        Qa::History(h) => {
            let s = aggregate_qa_history(h.history.as_deref())?;
            if h.json {
                return print_unicode_json(&s);
            }
            println!(
                "QA history | {} batches | {}/{} tested",
                s.batches, s.tested, s.total_cases
            );
            for (label, rate) in [
                ("Manual pass", s.manual_pass_pct),
                ("One-shot pass", s.one_shot_pct),
                ("Within two attempts", s.within_two_attempts_pct),
            ] {
                println!(
                    "{label}  {}",
                    rate.map(|v| format!("{v:.1}%")).unwrap_or("N/A".into())
                );
            }
            println!(
                "Passed {} | Failed {} | Blocked {}",
                s.passed, s.failed, s.blocked
            );
            println!(
                "Automated pass, manual fail  {}",
                s.auto_pass_manual_fail_count
            );
        }
    }
    Ok(0)
}
