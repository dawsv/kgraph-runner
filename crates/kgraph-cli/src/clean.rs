use crate::{args::Clean, print_json, resolve, Result};
use kgraph_core::worktree::prune_orphaned_worktrees;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn collect_stale_files(kgraph_dir: &Path, include_all: bool) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    let Ok(entries) = fs::read_dir(kgraph_dir) else {
        return targets;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let remove = name.ends_with(".lock")
            || (include_all
                && (name.ends_with("_state.json")
                    || name.ends_with("_report.json")
                    || name == "state.json"
                    || name == "report.json"
                    || name.ends_with(".log")));
        if remove {
            targets.push(path);
        }
    }
    targets.sort();
    targets
}

pub fn run_data(args: &Clean) -> Result<Value> {
    let root = resolve(&args.dir)?;
    let kgraph_dir = root.join(".kgraph");
    let worktree_base = kgraph_dir.join("worktrees");

    let worktrees = if root.join(".git").exists() {
        prune_orphaned_worktrees(&root, &worktree_base, args.dry_run).unwrap_or_default()
    } else {
        Vec::new()
    };

    let files = collect_stale_files(&kgraph_dir, args.all);
    let mut removed_files = Vec::new();
    for path in files {
        let mut removed = false;
        if !args.dry_run && fs::remove_file(&path).is_ok() {
            removed = true;
        }
        removed_files.push(json!({"path": path, "removed": removed}));
    }

    Ok(json!({
        "dir": root,
        "dry_run": args.dry_run,
        "all": args.all,
        "worktrees": worktrees,
        "files": removed_files,
    }))
}

pub fn run(args: Clean) -> Result<i32> {
    let data = run_data(&args)?;
    if args.json {
        return print_json(&data);
    }
    let verb = if args.dry_run { "Would remove" } else { "Removed" };
    let wt_count = data["worktrees"].as_array().map_or(0, Vec::len);
    let file_count = data["files"].as_array().map_or(0, Vec::len);
    println!("{verb} {wt_count} orphaned worktree(s) and {file_count} stale file(s).");
    if let Some(wts) = data["worktrees"].as_array() {
        for w in wts {
            println!("  worktree: {}", w["path"].as_str().unwrap_or(""));
        }
    }
    if let Some(files) = data["files"].as_array() {
        for f in files {
            println!("  file:     {}", f["path"].as_str().unwrap_or(""));
        }
    }
    Ok(0)
}
