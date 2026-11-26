use crate::{
    args::{Board, Claim, Lease},
    display::table,
    print_json, project_dir, report_for_state, Result,
};
use kgraph_town::workboard::{self as wb, ClaimedWork, WorkItem};
use serde_json::{json, Value};

pub fn board(args: &Board) -> Result<Vec<WorkItem>> {
    let report = args
        .report
        .clone()
        .unwrap_or_else(|| report_for_state(&args.state));
    wb::list_workboard(&args.state, Some(&report), Some(&project_dir()?))
}
pub fn claim(args: &Claim) -> Result<Option<ClaimedWork>> {
    let report = args
        .board
        .report
        .clone()
        .unwrap_or_else(|| report_for_state(&args.board.state));
    wb::claim_work(
        &args.board.state,
        Some(&report),
        Some(&project_dir()?),
        &args.worker,
        args.ttl,
        args.node.as_deref(),
        args.worktree.clone(),
    )
}
pub fn heartbeat(lease: &Lease, ttl: u64) -> Result<Value> {
    Ok(json!(wb::heartbeat_lease(
        &lease.state,
        &lease.node,
        &lease.worker,
        ttl
    )?))
}
pub fn release(lease: &Lease, reason: Option<&str>) -> Result<Value> {
    Ok(json!({"released":wb::release_lease(&lease.state, &lease.node, &lease.worker, reason)?}))
}
pub fn complete(lease: &Lease, summary: Option<&str>) -> Result<Value> {
    Ok(json!({"completed":wb::complete_lease(&lease.state, &lease.node, &lease.worker, summary)?}))
}
pub fn display_board(args: Board) -> Result<i32> {
    let items = board(&args)?;
    if args.json {
        return print_json(&json!({"items":items}));
    }
    let mut rows = vec![["Node", "Packet", "Worker", "Expires"]
        .map(str::to_owned)
        .to_vec()];
    for item in items {
        let (worker, expires) = item
            .lease
            .map(|l| (l.worker_id, l.expires_at))
            .unwrap_or_else(|| ("-".into(), "-".into()));
        rows.push(vec![item.node_path, item.packet_path, worker, expires]);
    }
    table(rows);
    Ok(0)
}
pub fn display_claim(args: Claim) -> Result<i32> {
    let item = claim(&args)?;
    if args.board.json {
        print_json(&item)?;
    } else if let Some(c) = &item {
        println!(
            "{} claimed {} ({})",
            c.lease.worker_id, c.lease.node_path, c.lease.packet_path
        );
    } else {
        println!("No unclaimed work available.");
    }
    Ok(if item.is_some() { 0 } else { 4 })
}
pub fn display_lease(lease: &Lease, result: Value) -> Result<i32> {
    if lease.json {
        print_json(&result)
    } else {
        println!("{} | {} | {}", lease.node, lease.worker, result);
        Ok(0)
    }
}
