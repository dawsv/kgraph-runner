use crate::{print_json, Result};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn kgraph_home() -> Option<PathBuf> {
    if let Some(custom) = env::var_os("KGRAPH_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(custom));
    }
    home_dir().map(|h| h.join(".kgraph"))
}

fn probe_bin(name: &str, args: &[&str]) -> Value {
    match Command::new(name)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("ok")
                .trim();
            json!({"available": true, "version": line})
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stderr
                .lines()
                .chain(stdout.lines())
                .find(|l| !l.trim().is_empty())
                .unwrap_or("non-zero exit")
                .trim();
            json!({"available": true, "version": line})
        }
        Err(_) => json!({"available": false}),
    }
}

pub fn list_library_subgraphs() -> Vec<Value> {
    let Some(dir) = kgraph_home().map(|h| h.join("subgraphs")) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_subgraphs(&dir, &dir, &mut found);
    found.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    found
}

fn collect_subgraphs(root: &Path, dir: &Path, out: &mut Vec<Value>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_subgraphs(root, &path, out);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("toml" | "json")) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let stem_name = rel_str
            .strip_suffix(".toml")
            .or_else(|| rel_str.strip_suffix(".json"))
            .unwrap_or(&rel_str);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let parsed: Option<Value> = match ext.as_deref() {
            Some("toml") => toml::from_str(&raw).ok(),
            Some("json") => serde_json::from_str(&raw).ok(),
            _ => None,
        };
        let title = parsed
            .as_ref()
            .and_then(|v| v["title"].as_str())
            .map(str::to_owned);
        let default_context = parsed
            .as_ref()
            .and_then(|v| v["default_context"].as_object())
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        out.push(json!({
            "name": format!("@lib/{stem_name}"),
            "path": path,
            "title": title,
            "parameters": default_context,
        }));
    }
}

fn check_mcp_file(path: PathBuf) -> Value {
    let configured = fs::read_to_string(&path)
        .map(|raw| raw.contains("kgraph"))
        .unwrap_or(false);
    json!({"path": path, "exists": path.exists(), "kgraph_configured": configured})
}

pub fn report() -> Value {
    let exe = env::current_exe().ok();
    let subgraphs = list_library_subgraphs();
    let home = home_dir();
    let mcp_clients = if let Some(ref h) = home {
        json!({
            "codex": check_mcp_file(h.join(".codex/config.toml")),
            "antigravity": check_mcp_file(h.join(".gemini/config/mcp_config.json")),
            "claude": check_mcp_file(h.join(".claude.json")),
        })
    } else {
        json!({})
    };
    let tools = json!({
        "git": probe_bin("git", &["--version"]),
        "cargo": probe_bin("cargo", &["--version"]),
        "rustc": probe_bin("rustc", &["--version"]),
        "python3": probe_bin("python3", &["--version"]),
    });
    let agents = json!({
        "claude": probe_bin("claude", &["--version"]),
        "codex": probe_bin("codex", &["--version"]),
        "agy": probe_bin("agy", &["--version"]),
    });
    let ok = tools["git"]["available"].as_bool().unwrap_or(false);
    json!({
        "ok": ok,
        "version": env!("CARGO_PKG_VERSION"),
        "executable": exe,
        "kgraph_home": kgraph_home(),
        "tools": tools,
        "agents": agents,
        "library_subgraphs": subgraphs,
        "mcp_clients": mcp_clients,
    })
}

pub fn run(as_json: bool) -> Result<i32> {
    let data = report();
    if as_json {
        return print_json(&data);
    }
    println!("KGraph Runner v{}", env!("CARGO_PKG_VERSION"));
    if let Some(exe) = data["executable"].as_str() {
        println!("  binary:       {exe}");
    }
    if let Some(kh) = data["kgraph_home"].as_str() {
        println!("  kgraph_home:  {kh}");
    }
    println!("\nToolchain:");
    for name in ["git", "cargo", "rustc", "python3"] {
        let info = &data["tools"][name];
        if info["available"].as_bool().unwrap_or(false) {
            println!("  [ok]      {name:<8} {}", info["version"].as_str().unwrap_or(""));
        } else {
            println!("  [missing] {name}");
        }
    }
    println!("\nAgent CLIs:");
    for name in ["claude", "codex", "agy"] {
        let info = &data["agents"][name];
        if info["available"].as_bool().unwrap_or(false) {
            println!("  [ok]      {name:<8} {}", info["version"].as_str().unwrap_or(""));
        } else {
            println!("  [absent]  {name}");
        }
    }
    let subs = data["library_subgraphs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    println!("\nShared library subgraphs ({}):", subs.len());
    for s in subs {
        println!(
            "  - {} ({})",
            s["name"].as_str().unwrap_or(""),
            s["title"].as_str().unwrap_or("untitled")
        );
    }
    println!("\nMCP Client Configs:");
    for name in ["codex", "antigravity", "claude"] {
        let info = &data["mcp_clients"][name];
        let mark = if info["kgraph_configured"].as_bool().unwrap_or(false) {
            "configured"
        } else if info["exists"].as_bool().unwrap_or(false) {
            "not configured"
        } else {
            "absent"
        };
        println!("  [{mark:<14}] {name}");
    }
    Ok(0)
}
