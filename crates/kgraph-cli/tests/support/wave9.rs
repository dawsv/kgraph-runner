use super::support::*;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

#[test]
fn automatic_telemetry_and_filtered_exports() {
    let p = Project::new();
    p.workflow(json!([shell("first")]));
    p.cli(&["run", "flow.json"], 0);
    p.cli(&["run", "missing.toml"], 1);
    p.cli(&["run", "flow.json"], 0);
    let lines = fs::read_to_string(p.path(".kgraph/telemetry.jsonl")).unwrap();
    let runs: Vec<Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["workflow_ref"], json!(p.path("flow.json")));
    assert_eq!(
        runs[2]["run_id"],
        p.read(".kgraph/flow_report.json")["telemetry_summary"]["run_id"]
    );
    assert_eq!(runs[1]["exit_code"], 1);
    assert_eq!(runs[1]["workflow_ref"], json!(p.path("missing.toml")));
    let a = p.json(
        &[
            "analyze",
            "--json",
            "--workflow",
            "flow.",
            "--last",
            "1",
            "--md-out",
            "exports/out.md",
            "--chrome-trace",
            "exports/trace.json",
        ],
        0,
    );
    assert_eq!(a["runs_analyzed"], 1);
    assert_eq!(a["runs_passed"], 1);
    assert_eq!(a["runs_total"], 1);
    assert!(a["nodes"]["first"].is_object());
    assert!(fs::read_to_string(p.path("exports/out.md"))
        .unwrap()
        .contains("## Execution telemetry"));
    assert!(p.read("exports/trace.json")["traceEvents"]
        .as_array()
        .is_some_and(|a| !a.is_empty()));
    assert!(stdout(&p.cli(&["telemetry"], 0)).contains("Runs: 3"));
    let none = p.json(&["analyze", "--json", "--last", "0"], 0);
    assert_eq!(none["runs_analyzed"], 0);
    p.cli(&["analyze", "--chrome-trace", ".kgraph/telemetry.jsonl"], 1);
    assert_eq!(
        fs::read_to_string(p.path(".kgraph/telemetry.jsonl")).unwrap(),
        lines
    );
}
#[test]
fn telemetry_overrides_and_run_failures() {
    let p = Project::new();
    p.workflow(json!([{"type":"shell","id":"fails","command":[env!("CARGO_BIN_EXE_kgraph"), "invalid-command"]}]));
    p.cli(
        &[
            "run",
            "flow.json",
            "--state",
            ".kgraph/alt/s.json",
            "--quiet",
        ],
        1,
    );
    let run = p.read(".kgraph/alt/telemetry.jsonl");
    assert_eq!(run["nodes_failed"], 1);
    assert_eq!(run["workflow_ref"], json!(p.path("flow.json")));
    p.cli(
        &[
            "run",
            "flow.json",
            "--telemetry",
            "history/log.jsonl",
            "--force",
        ],
        1,
    );
    p.cli(&["run", "flow.json", "--no-telemetry", "--force"], 1);
    assert!(!p.path(".kgraph/telemetry.jsonl").exists());
    assert_eq!(
        p.json(&["analyze", "history/log.jsonl", "--json"], 0)["runs_analyzed"],
        1
    );
    assert!(stdout(&p.cli(&["analyze"], 0)).contains("No matching telemetry"));
    p.cli(&["run", "flow.json", "--telemetry", "flow.json"], 1);
    assert!(p.read("flow.json")["nodes"].is_array());
}

#[test]
fn concurrent_cli_claims_and_lease_lifecycle() {
    let p = Project::new();
    p.workflow(json!([gate("left"), gate("right")]));
    p.cli(&["run", "flow.json", "--max-parallel", "2"], 4);
    let state = ".kgraph/flow_state.json";
    let board = || p.json(&["workboard", state, "--json"], 0);
    assert_eq!(board()["items"].as_array().unwrap().len(), 2);
    let results = std::thread::scope(|s| {
        let claims: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|w| {
                s.spawn({
                    let p = &p;
                    move || p.json(&["claim", state, "--worker", w, "--json"], 0)
                })
            })
            .collect();
        claims
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    let node = results[0]["lease"]["node_path"].as_str().unwrap();
    assert_ne!(results[1]["lease"]["node_path"], node);
    assert!(board()["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["lease"].is_object()));
    assert_eq!(
        p.json(&["claim", state, "--worker", "c", "--json"], 4),
        Value::Null
    );
    p.cli(&["release", state, "--worker", "b", "--node", node], 1);
    assert_eq!(
        p.json(
            &[
                "heartbeat",
                state,
                "--worker",
                "a",
                "--node",
                node,
                "--ttl",
                "600",
                "--json"
            ],
            0
        )["heartbeat_count"],
        1
    );
    assert_eq!(
        p.json(
            &["release", state, "--worker", "a", "--node", node, "--reason", "handoff", "--json"],
            0
        )["released"],
        true
    );
    let claimed = p.json(
        &[
            "claim",
            state,
            "--worker",
            "c",
            "--node",
            node,
            "--worktree",
            "workspace",
            "--json",
        ],
        0,
    );
    assert_eq!(claimed["lease"]["worktree_path"], "workspace");
    assert_eq!(
        p.json(
            &[
                "complete",
                state,
                "--worker",
                "c",
                "--node",
                node,
                "--summary",
                "reviewed",
                "--json"
            ],
            0
        )["completed"],
        true
    );
    let progress = p.read(".kgraph/flow_state_progress.json");
    assert_eq!(progress[node]["items"]["lease"]["status"], "passed");
    assert_eq!(progress[node]["items"]["lease"]["detail"], "reviewed");
}

struct Rpc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
impl Rpc {
    fn new(p: &Project) -> Self {
        Self::spawn(p.cmd(&["mcp"]))
    }
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }
    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }
    fn receive(&mut self) -> Value {
        let mut line = String::new();
        assert!(self.stdout.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    }
    fn request(&mut self, id: i32, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string());
        let response = self.receive();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        response
    }
    fn tool(&mut self, id: i32, name: &str, args: Value, failed: bool) -> Value {
        let response = self.request(id, "tools/call", json!({"name":name, "arguments":args}));
        let result = &response["result"];
        let data = &result["structuredContent"];
        assert_eq!(
            result,
            &json!({"content":[{"type":"text", "text":serde_json::to_string_pretty(data).unwrap()}],"structuredContent":data,"isError":failed}),
            "{name}: {result}"
        );
        data.clone()
    }
}

#[test]
fn mcp_run_rejects_hub_settings_and_keeps_host_credentials_out_of_workflows() {
    let p = Project::new();
    p.workflow(json!([{"id":"local","type":"shell","command":["python3","-c",
        "import os; assert 'KGRAPH_HUB_TOKEN' not in os.environ and 'KGRAPH_HUB_URL' not in os.environ"]}]));
    let mut command = p.cmd(&["mcp"]);
    command
        .env("KGRAPH_HUB_URL", "http://127.0.0.1:1")
        .env("KGRAPH_HUB_TOKEN", "test-only");
    let mut rpc = Rpc::spawn(command);
    let list = rpc.request(1, "tools/list", json!({}));
    let run = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "kgraph_run")
        .unwrap();
    for (key, value) in [
        ("hub_url", "http://127.0.0.1:2"),
        ("hub_token", "caller-token"),
    ] {
        assert!(run["inputSchema"]["properties"].get(key).is_none());
        let error = rpc.tool(
            2,
            "kgraph_run",
            json!({"workflow":"flow.json", (key):value}),
            true,
        );
        assert_eq!(error["error"], format!("Unknown run argument {key}"));
        assert!(!p.path(".kgraph/flow_report.json").exists());
    }
    assert_eq!(
        rpc.tool(3, "kgraph_run", json!({"workflow":"flow.json"}), false)["exit_code"],
        0
    );
    drop(rpc.stdin);
    assert!(rpc.child.wait().unwrap().success());
}

#[test]
fn mcp_drains_oversized_frames_and_accepts_the_exact_limit() {
    let p = Project::new();
    let mut rpc = Rpc::new(&p);
    let limit = 4 * 1024 * 1024;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let mut frame = " ".repeat(limit - ping.len());
    frame.push_str(ping);
    rpc.send(&frame);
    assert_eq!(rpc.receive()["result"], json!({}));
    frame.push(' ');
    rpc.send(&frame);
    let oversized = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Frame exceeds maximum size"}});
    assert_eq!(rpc.receive(), oversized);
    assert_eq!(rpc.request(2, "ping", json!({}))["result"], json!({}));
    rpc.stdin.write_all(frame.as_bytes()).unwrap();
    drop(rpc.stdin);
    let mut line = String::new();
    rpc.stdout.read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), oversized);
    assert!(rpc.child.wait().unwrap().success());
}

#[test]
fn mcp_file_readers_reject_oversized_and_non_regular_files() {
    let p = Project::new();
    let mut rpc = Rpc::new(&p);
    fs::File::create(p.path("large"))
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    #[cfg(unix)]
    assert!(Command::new("mkfifo")
        .arg(p.path("pipe"))
        .status()
        .unwrap()
        .success());
    for (name, key) in [
        ("kgraph_status", "state"),
        ("kgraph_report", "report"),
        ("kgraph_analyze", "telemetry_path"),
    ] {
        for (path, reason) in [
            ("large", "size limit"),
            (".", "regular file"),
            #[cfg(unix)]
            ("pipe", "regular file"),
        ] {
            let error = rpc.tool(1, name, json!({(key):path}), true);
            assert!(error["error"].as_str().unwrap().contains(reason), "{error}");
        }
    }
    // Small on disk, but indentation alone would exceed the result text limit.
    let nested = format!(
        "{}{}0{}",
        "[".repeat(64),
        "0,".repeat(150_000),
        "]".repeat(64)
    );
    fs::write(p.path("wide.json"), format!(r#"{{"nested":{nested}}}"#)).unwrap();
    let error = rpc.tool(2, "kgraph_report", json!({"report":"wide.json"}), true);
    assert_eq!(error["error"], "Tool result exceeds maximum size");
    assert_eq!(rpc.request(3, "ping", json!({}))["result"], json!({}));
    drop(rpc.stdin);
    assert!(rpc.child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn run_rejects_hard_links_between_every_pair_of_input_and_output_files() {
    let p = Project::new();
    p.workflow(json!([shell("build")]));
    let original = fs::read(p.path("flow.json")).unwrap();
    fs::hard_link(p.path("flow.json"), p.path("alias")).unwrap();
    for flag in ["--telemetry", "--report", "--state"] {
        let output = p.cli(&["run", "flow.json", flag, "alias"], 1);
        assert!(stderr(&output).contains("must be separate"));
        assert_eq!(fs::read(p.path("flow.json")).unwrap(), original);
        assert_eq!(fs::read(p.path("alias")).unwrap(), original);
    }
    fs::write(p.path("out1"), b"original").unwrap();
    fs::hard_link(p.path("out1"), p.path("out2")).unwrap();
    for (first, second) in [
        ("--state", "--report"),
        ("--state", "--telemetry"),
        ("--report", "--telemetry"),
    ] {
        let output = p.cli(&["run", "flow.json", first, "out1", second, "out2"], 1);
        assert!(stderr(&output).contains("must be separate"));
        assert_eq!(fs::read(p.path("out1")).unwrap(), b"original");
        assert_eq!(fs::read(p.path("out2")).unwrap(), b"original");
    }
}
#[test]
fn stdio_mcp_serves_thirteen_tools_and_recovers_after_errors() {
    let p = Project::new();
    p.workflow(json!([shell("build"), gate("review")]));
    let mut rpc = Rpc::new(&p);
    let init = rpc.request(1, "initialize", json!({}));
    assert_eq!(
        init["result"],
        json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"kgraph","version":env!("CARGO_PKG_VERSION")}})
    );
    rpc.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    rpc.send(r#"{"jsonrpc":"2.0","method":"ping"}"#);
    let list = rpc.request(2, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 13);
    for (tool, name) in tools.iter().zip([
        "plan",
        "run",
        "status",
        "report",
        "analyze",
        "workboard",
        "claim",
        "heartbeat",
        "release",
        "complete",
        "progress",
        "ask",
        "answer",
    ]) {
        assert_eq!(tool["name"], format!("kgraph_{name}"));
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(tool["inputSchema"]["properties"].is_object());
        assert!(tool["description"].is_string());
    }
    assert_eq!(
        tools[1]["inputSchema"]["properties"]["force"],
        json!({"type":"boolean"})
    );
    let plan = rpc.tool(3, "kgraph_plan", json!({"workflow":"flow.json"}), false);
    assert_eq!(plan["nodes"].as_array().unwrap().len(), 2);
    let run = rpc.tool(
        4,
        "kgraph_run",
        json!({"workflow":"flow.json","max_parallel":2}),
        false,
    );
    assert_eq!(run["status"], "awaiting_subagents");
    assert_eq!(run["exit_code"], 4);
    let state = ".kgraph/flow_state.json";
    assert_eq!(
        rpc.tool(
            5,
            "kgraph_analyze",
            json!({"workflow":"flow","last":1}),
            false
        )["runs_analyzed"],
        1
    );
    assert_eq!(
        rpc.tool(
            6,
            "kgraph_claim",
            json!({"state":state,"worker":"worker"}),
            false
        )["lease"]["node_path"],
        "review"
    );
    assert_eq!(
        rpc.tool(7, "kgraph_workboard", json!({"state":state}), false)["items"][0]["lease"]
            ["worker_id"],
        "worker"
    );
    assert_eq!(
        rpc.tool(
            8,
            "kgraph_heartbeat",
            json!({"state":state,"worker":"worker","node":"review"}),
            false
        )["heartbeat_count"],
        1
    );
    assert!(rpc.tool(
        9,
        "kgraph_release",
        json!({"state":state,"worker":"other","node":"review"}),
        true
    )["error"]
        .is_string());
    assert_eq!(
        rpc.tool(
            10,
            "kgraph_release",
            json!({"state":state,"worker":"worker","node":"review"}),
            false
        )["released"],
        true
    );
    rpc.tool(
        11,
        "kgraph_claim",
        json!({"state":state,"worker":"next"}),
        false,
    );
    assert_eq!(
        rpc.tool(
            12,
            "kgraph_complete",
            json!({"state":state,"worker":"next","node":"review"}),
            false
        )["completed"],
        true
    );
    assert_eq!(
        rpc.tool(13, "kgraph_status", json!({"state":state}), false)["nodes"]["review"]["status"],
        "awaiting_subagents"
    );
    assert_eq!(
        rpc.tool(
            14,
            "kgraph_report",
            json!({"report":".kgraph/flow_report.json"}),
            false
        )["status"],
        "awaiting_subagents"
    );
    rpc.tool(
        15,
        "kgraph_progress",
        json!({"state":state,"node":"review","item":"qa","progress":75}),
        false,
    );
    rpc.tool(16,"kgraph_ask",json!({"state":state,"node":"review","item":"qa","question":"Ready?","option":["yes","no"]}),false);
    assert!(rpc.tool(
        17,
        "kgraph_answer",
        json!({"state":state,"node":"review","item":"qa"}),
        false
    )["answer"]
        .is_null());
    rpc.tool(18, "kgraph_plan", json!({"workflow":"absent.toml"}), true);
    rpc.tool(
        19,
        "kgraph_claim",
        json!({"state":state,"worker":"x","ttl":"5"}),
        true,
    );
    assert_eq!(
        rpc.request(20, "unknown", json!({}))["error"]["code"],
        -32601
    );
    rpc.send("invalid JSON");
    assert_eq!(
        rpc.receive(),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
    );
    assert_eq!(rpc.request(21, "ping", json!({}))["result"], json!({}));
    p.workflow(json!([{"id":"read","type":"shell","timeout_s":2,"command":["python3","-c","import sys; assert sys.stdin.read() == ''"]}]));
    assert_eq!(
        rpc.tool(
            22,
            "kgraph_run",
            json!({"workflow":"flow.json","force":true}),
            false
        )["exit_code"],
        0
    );
    rpc.stdin.write_all(&[0xff, b'\n']).unwrap();
    rpc.stdin.flush().unwrap();
    assert_eq!(rpc.receive()["error"]["code"], -32700);
    assert_eq!(rpc.request(23, "ping", json!({}))["result"], json!({}));
    drop(rpc.stdin);
    assert!(rpc.child.wait().unwrap().success());
}

#[test]
fn scaffold_templates_are_plannable_and_preserve_existing_files() {
    let p = Project::new();
    for template in [
        "ci",
        "agent-loop",
        "swarm-splitter",
        "bug-fix",
        "bug-batch",
        "release",
    ] {
        let path = format!(".kgraph/{template}.toml");
        p.cli(&["init", template, "--output", &path], 0);
        let entries = p.json(&["plan", &path, "--json"], 0);
        assert!(!entries.as_array().unwrap().is_empty(), "{template}");
    }
    p.cli(&["init"], 0);
    let path = p.path(".kgraph/workflow.toml");
    fs::write(&path, "local edits\n").unwrap();
    p.cli(&["init", "release"], 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), "local edits\n");
    p.cli(&["init", "release", "--force"], 0);
    assert!(fs::read_to_string(path)
        .unwrap()
        .contains("version_history"));
}

#[test]
fn scaffold_detects_workspace_even_when_output_is_nested() {
    let p = Project::new();
    for (dir, manifest, expected) in [
        ("rust", "Cargo.toml", json!(["cargo", "test"])),
        ("js", "package.json", json!(["npm", "test"])),
        ("py", "pyproject.toml", json!(["python3", "-m", "unittest"])),
    ] {
        fs::create_dir(p.path(dir)).unwrap();
        fs::write(p.path(dir).join(manifest), "").unwrap();
        p.cli(&["init", "--dir", dir], 0);
        let workflow = format!("{dir}/.kgraph/workflow.toml");
        let generated = fs::read_to_string(p.path(&workflow)).unwrap();
        assert!(generated.contains(&expected.to_string()));
        assert!(generated.contains("cwd = \"..\""));
        if dir == "rust" {
            assert!(generated.contains(&json!(["cargo", "clippy", "--all-targets"]).to_string()));
        }
        assert_eq!(
            p.json(&["plan", &workflow, "--json"], 0)
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
}
