use crate::{
    args::Poll, print_json, project_dir, remaining, report_for_state, resolve, timeout, Result,
};
use kgraph_core::files::{read_optional_object, valid_id};
use kgraph_town::{interactions as live, network};
use serde_json::{json, Value};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub fn apply(
    response: &Value,
    state: &Path,
    report: Option<&Path>,
    skip_stale: bool,
) -> Result<Option<Value>> {
    let reject = |s: &str| {
        if skip_stale {
            Ok(None)
        } else {
            Err(s.to_owned())
        }
    };
    let node = response["node_path"].as_str().unwrap_or("");
    let hash = response["packet_hash"].as_str().unwrap_or("");
    let subtask = node
        .split_once("::")
        .is_some_and(|(p, id)| live::valid_node_path(p) && valid_id(id));
    if !(subtask || live::valid_node_path(node)) || hash.trim().is_empty() {
        return reject("Queued response must contain a valid node path and packet hash");
    }
    let state = resolve(state)?;
    let report = report
        .map(Path::to_owned)
        .unwrap_or_else(|| report_for_state(&state));
    let project = project_dir()?;
    let mut payload = response.clone();
    payload
        .as_object_mut()
        .ok_or("Expected a queued response object")?
        .remove("submitted_at");
    if !subtask {
        let telemetry = Value::Object(read_optional_object(&state)?);
        let overview = Value::Object(read_optional_object(&report)?);
        let Some((target, Some(raw))) = live::active_gate(&telemetry, &overview, Some(node)) else {
            return reject("The queued response's node is no longer awaiting input");
        };
        if target != node {
            return reject("The queued response's node is no longer awaiting input");
        }
        let path = live::resolve_packet(raw, &state, &report, Some(&project), &mut |_| {})?;
        let (packet, _, _) =
            live::json_file(&path, Some(live::PACKET_LIMIT)).map_err(|e| e.to_string())?;
        let gate = live::extract_input_gate(
            &packet,
            Some(&path),
            Some(node),
            Some(&state),
            Some(&project),
            true,
        );
        let Some(gate) = gate.filter(|g| g["packet_hash"] == hash) else {
            return reject("The queued response does not match the active gate's packet hash");
        };
        if let Some(local) = gate["submitted"]["answered_at"].as_str() {
            if response["answered_at"]
                .as_str()
                .is_none_or(|remote| remote <= local)
            {
                return reject("The queued response is older than the current local decision");
            }
        }
        payload["submitted_at"] = gate["submitted"]["answered_at"].clone();
    }
    let run = json!({"state_path":state,"report_path":report,"project_dir":project});
    let (code, body) = live::submit_gate_response(run.as_object().unwrap(), &payload);
    match code {
        200 => Ok(Some(body["decision"].clone())),
        409 => reject(body["error"].as_str().unwrap_or("Conflicting response")),
        _ => Err(body["error"]
            .as_str()
            .unwrap_or("Could not apply decision")
            .into()),
    }
}
fn poll_time(poll: &Poll) -> Result<(Duration, Option<Duration>)> {
    if poll.poll_ms == 0 {
        return Err("--poll-ms must be at least 1".into());
    }
    Ok((
        Duration::from_millis(poll.poll_ms),
        timeout(poll.timeout, "--timeout")?,
    ))
}
fn print_decision(decision: Option<Value>, json: bool, empty: &str, default: &str) -> Result<i32> {
    if json {
        print_json(&decision)?;
    } else {
        println!(
            "{}",
            decision
                .as_ref()
                .map(|d| d["summary"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(default))
                .unwrap_or(empty)
        );
    }
    Ok(if decision.is_some() { 0 } else { 4 })
}
pub fn answer(
    state: &Path,
    node: &str,
    item: &str,
    run_id: Option<&str>,
    url: Option<&str>,
    poll: Poll,
) -> Result<i32> {
    let (interval, limit) = poll_time(&poll)?;
    let start = Instant::now();
    let target = format!("{node}::{item}");
    loop {
        let mut d = live::read_subtask_answer(state, node, item)?;
        if d.is_none() {
            if let (Some(url), Some(id)) = (
                url.filter(|s| !s.is_empty()),
                run_id.filter(|s| !s.is_empty()),
            ) {
                if let Some(r) = network::fetch_remote_response(url, poll.token.as_deref(), id)? {
                    if r.get("node_path").and_then(Value::as_str) == Some(&target) {
                        d = apply(&Value::Object(r), state, None, true)?;
                    }
                }
            }
        }
        let left = remaining(start, limit);
        if d.is_some() || !poll.wait || left.is_zero() {
            return print_decision(d, poll.json, "No answer yet.", "");
        }
        thread::sleep(interval.min(left));
    }
}
pub fn pull(
    state: &Path,
    id: &str,
    url: &str,
    report: Option<&Path>,
    do_apply: bool,
    poll: Poll,
) -> Result<i32> {
    let (interval, limit) = poll_time(&poll)?;
    let start = Instant::now();
    let decision = loop {
        let r = network::fetch_remote_response(url, poll.token.as_deref(), id)?.map(Value::Object);
        let left = remaining(start, limit);
        if r.is_some() || !poll.wait || left.is_zero() {
            break r;
        }
        thread::sleep(interval.min(left));
        if remaining(start, limit).is_zero() {
            break None;
        }
    };
    if let Some(r) = decision.as_ref().filter(|_| do_apply) {
        apply(r, state, report, false)?;
    }
    print_decision(
        decision,
        poll.json,
        "No response queued.",
        &format!("Response for {id}"),
    )
}
