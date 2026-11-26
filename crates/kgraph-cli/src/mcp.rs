//! Line-delimited JSON-RPC transport for the native workflow and workboard operations.
use crate::{
    analyze,
    args::{Cli, Command},
    clean, default_file, doctor, execution, swarm, town, Result,
};
use clap::{Arg, ArgAction, CommandFactory, Parser};
use kgraph_town::interactions::{json_file, read_subtask_answer};
use serde_json::{json, Map, Value};
use std::{
    any::TypeId,
    io::{self, BufRead, Write},
    path::Path,
};

const MAX_MCP_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_MCP_FILE_BYTES: usize = 16 * 1024 * 1024;

const TOOLS: [&str; 15] = [
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
    "doctor",
    "clean",
];

// The transport owns formatting. MCP calls execute once and return local results.
fn exposed(command: &str, arg: &Arg) -> bool {
    let key = arg.get_id().as_str();
    if matches!(
        key,
        "json" | "help" | "quiet" | "watch" | "watch_interval" | "watch_timeout"
    ) {
        return false;
    }
    match command {
        "run" => !matches!(
            key,
            "hub_url" | "hub_token" | "notify_webhook" | "notify_secret" | "notify_desktop"
        ),
        "progress" | "ask" => !matches!(
            key,
            "push"
                | "hub_url"
                | "report"
                | "workflow"
                | "token"
                | "run_id"
                | "project_name"
                | "host_label"
        ),
        "answer" => matches!(key, "state" | "node" | "item"),
        _ => true,
    }
}
fn scalar_type(arg: &Arg) -> &'static str {
    let id = arg.get_value_parser().type_id();
    if matches!(arg.get_action(), ArgAction::SetTrue) {
        "boolean"
    } else if [
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<isize>(),
    ]
    .iter()
    .any(|t| id == *t)
    {
        "integer"
    } else if id == TypeId::of::<f64>() {
        "number"
    } else {
        "string"
    }
}
fn definition(command: &clap::Command) -> Value {
    let name = command.get_name();
    let mut properties = Map::new();
    let mut required = vec![];
    for arg in command.get_arguments().filter(|a| exposed(name, a)) {
        let key = arg.get_id().as_str();
        let mut field = json!({"type":scalar_type(arg)});
        let possible = arg.get_possible_values();
        if field["type"] == "string" && !possible.is_empty() {
            field["enum"] = json!(possible.iter().map(|p| p.get_name()).collect::<Vec<_>>());
        }
        if matches!(arg.get_action(), ArgAction::Append) {
            field = json!({"type":"array","items":field});
        }
        if let Some(help) = arg.get_help() {
            field["description"] = json!(help.to_string());
        }
        if arg.is_required_set() {
            required.push(key);
        }
        properties.insert(key.into(), field);
    }
    let mut schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    if name == "ask" {
        schema["oneOf"] = json!([{"required":["question"]},{"required":["questions_json"]}]);
    }
    json!({"name":format!("kgraph_{name}"),"description":command.get_about().map(ToString::to_string),"inputSchema":schema})
}

fn scalar(arg: &Arg, value: &Value) -> Result<String> {
    let kind = scalar_type(arg);
    match kind {
        "string" if value.is_string() => Ok(value.as_str().unwrap().into()),
        "integer" if value.is_i64() || value.is_u64() => Ok(value.to_string()),
        "number" if value.is_number() => Ok(value.to_string()),
        _ => Err(format!("{} expects a {kind}", arg.get_id())),
    }
}
fn parse_tool(name: &str, arguments: &Map<String, Value>) -> Result<Command> {
    let name = name
        .strip_prefix("kgraph_")
        .filter(|n| TOOLS.contains(n))
        .ok_or_else(|| format!("Unknown tool {name}"))?;
    let root = Cli::command();
    let cmd = root
        .find_subcommand(name)
        .expect("tool maps to CLI command");
    let mut argv = vec!["kgraph".into(), name.into()];
    let mut positional = None;
    for (key, value) in arguments {
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_id().as_str() == key && exposed(name, a))
            .ok_or_else(|| format!("Unknown {name} argument {key}"))?;
        let Some(flag) = arg.get_long() else {
            positional = Some(scalar(arg, value)?);
            continue;
        };
        match arg.get_action() {
            ArgAction::SetTrue => {
                if value
                    .as_bool()
                    .ok_or_else(|| format!("{key} expects a boolean"))?
                {
                    argv.push(format!("--{flag}"));
                }
            }
            ArgAction::Append => {
                for v in value
                    .as_array()
                    .ok_or_else(|| format!("{key} expects an array"))?
                {
                    argv.push(format!("--{flag}={}", scalar(arg, v)?));
                }
            }
            _ => argv.push(format!("--{flag}={}", scalar(arg, value)?)),
        }
    }
    if let Some(value) = positional {
        argv.extend(["--".into(), value]);
    }
    Cli::try_parse_from(argv)
        .map(|c| c.command)
        .map_err(|e| e.to_string())
}

fn read_json_object(path: &Path) -> Result<Value> {
    json_file(path, Some(MAX_MCP_FILE_BYTES))
        .map(|(value, _, _)| value)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn call_tool(params: &Value) -> Result<(Value, bool)> {
    let name = params["name"].as_str().ok_or("Expected tool name")?;
    let empty = Map::new();
    let args = match params.get("arguments") {
        Some(v) => v.as_object().ok_or("Expected tool arguments object")?,
        None => &empty,
    };
    let value = match parse_tool(name, args)? {
        Command::Plan {
            execution: args, ..
        } => json!({"nodes":execution::plan_entries(args)?}),
        Command::Run(mut args) => {
            args.hub_url = None;
            args.hub_token = None;
            args.notify_webhook = None;
            args.notify_secret = None;
            args.notify_desktop = false;
            args.quiet = true;
            let report = args
                .report
                .clone()
                .unwrap_or_else(|| default_file(&args.execution.workflow, "_report.json"));
            let code = execution::run(args)?;
            let mut result = read_json_object(&report)?;
            result["exit_code"] = json!(code);
            return Ok((result, !matches!(code, 0 | 4)));
        }
        Command::Status { state, .. } => read_json_object(&state)?,
        Command::Report { report, .. } => read_json_object(&report)?,
        Command::Analyze(args) => {
            analyze::as_json(&analyze::analyze(&args, Some(MAX_MCP_FILE_BYTES))?)
        }
        Command::Workboard(args) => json!({"items":swarm::board(&args)?}),
        Command::Claim(args) => {
            swarm::claim(&args)?.map_or_else(|| json!({"claimed":false}), |c| json!(c))
        }
        Command::Heartbeat { lease, ttl } => swarm::heartbeat(&lease, ttl)?,
        Command::Release { lease, reason } => swarm::release(&lease, reason.as_deref())?,
        Command::Complete { lease, summary } => swarm::complete(&lease, summary.as_deref())?,
        Command::Progress(args) => town::progress_data(&args)?,
        Command::Ask(args) => town::ask_data(&args)?,
        Command::Answer {
            state, node, item, ..
        } => json!({"answer":read_subtask_answer(&state, &node, &item)?}),
        Command::Doctor { .. } => doctor::report(),
        Command::Clean(args) => clean::run_data(&args)?,
        _ => unreachable!("only exposed commands are parsed"),
    };
    Ok((value, false))
}

fn list_resources() -> Value {
    json!({
        "resources": [
            {
                "uri": "kgraph://schema",
                "name": "KGraph Workflow JSON Schema",
                "description": "Draft 2020-12 JSON Schema for declarative TOML/JSON KGraph workflows.",
                "mimeType": "application/json"
            },
            {
                "uri": "kgraph://subgraphs",
                "name": "Shared Library Subgraphs (@lib/...)",
                "description": "Catalog of reusable subgraphs installed under $KGRAPH_HOME/subgraphs/.",
                "mimeType": "application/json"
            },
            {
                "uri": "kgraph://templates",
                "name": "Starter Workflow Templates",
                "description": "Templates supported by `kgraph init <template>`.",
                "mimeType": "application/json"
            }
        ]
    })
}

fn read_resource(params: &Value) -> Result<Value> {
    let uri = params["uri"].as_str().ok_or("Expected resource uri")?;
    let payload = match uri {
        "kgraph://schema" => kgraph_subgraphs::workflow_json_schema(),
        "kgraph://subgraphs" => json!({"subgraphs": doctor::list_library_subgraphs()}),
        "kgraph://templates" => json!({
            "templates": [
                {"name": "ci", "description": "Lint, test, and build verification pipeline"},
                {"name": "agent-loop", "description": "Agent worker + deterministic verifier + evaluator loop"},
                {"name": "swarm-splitter", "description": "Parallel Git worktree splitter with ordered reduction"},
                {"name": "bug-fix", "description": "Single-bug bounded fix and verification loop"},
                {"name": "bug-batch", "description": "Multi-bug batch workflow with consolidated QA report"},
                {"name": "release", "description": "Release packaging with human approval gate"}
            ]
        }),
        _ => return Err(format!("Unknown resource URI: {uri}")),
    };
    let text = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": text
        }]
    }))
}
fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
// A workflow command may read stdin. Give it a dedicated process whose input
// reaches EOF after this request, so it cannot consume subsequent MCP frames.
fn isolated_run(params: &Value) -> Result<Value> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .args(["mcp", "--once"])
        .env_remove("KGRAPH_HUB_URL")
        .env_remove("KGRAPH_HUB_TOKEN")
        .env_remove("KGRAPH_NOTIFY_WEBHOOK")
        .env_remove("KGRAPH_NOTIFY_SECRET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let frame = json!({"jsonrpc":"2.0","id":0,"method":"tools/call","params":params});
    let sent = writeln!(child.stdin.take().expect("piped stdin"), "{frame}");
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    sent.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("Workflow process exited with {}", output.status));
    }
    let response: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "Missing workflow result".into())
}
// Pretty-printing a deeply nested result can expand it far beyond the input size.
#[derive(Default)]
struct ToolText(Vec<u8>);

impl Write for ToolText {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MCP_FILE_BYTES - self.0.len() {
            return Err(io::Error::other("Tool result exceeds maximum size"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn tool_result(outcome: Result<(Value, bool)>) -> Value {
    let (value, failed) = outcome.unwrap_or_else(|e| (json!({"error":e}), true));
    let mut text = ToolText::default();
    if let Err(e) = serde_json::to_writer_pretty(&mut text, &value) {
        let error = json!({"error":e.to_string()});
        return json!({"content":[{"type":"text","text":serde_json::to_string_pretty(&error).unwrap()}],"structuredContent":error,"isError":true});
    }
    let text = String::from_utf8(text.0).expect("serialized JSON is UTF-8");
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":failed})
}

pub fn handle_jsonrpc_line(line: &str) -> Option<Value> {
    handle(line.as_bytes(), false)
}
fn handle(line: &[u8], isolate_runs: bool) -> Option<Value> {
    let Ok(request) = serde_json::from_slice::<Value>(line) else {
        return Some(error(&Value::Null, -32700, "Parse error"));
    };
    let id = &request["id"];
    let method = request["method"].as_str();
    if request["jsonrpc"] != "2.0"
        || method.is_none()
        || !(id.is_null() || id.is_string() || id.is_number())
    {
        return Some(error(&Value::Null, -32600, "Invalid Request"));
    }
    request.get("id")?;
    let result = match method? {
        "notifications/initialized" => return None,
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"kgraph","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => {
            let root = Cli::command();
            json!({"tools":TOOLS.iter().map(|name| definition(root.find_subcommand(name).unwrap())).collect::<Vec<_>>()})
        }
        "resources/list" => list_resources(),
        "resources/read" => match read_resource(&request["params"]) {
            Ok(res) => res,
            Err(msg) => return Some(error(id, -32602, &msg)),
        },
        "tools/call" if isolate_runs && request["params"]["name"] == "kgraph_run" => {
            isolated_run(&request["params"]).unwrap_or_else(|error| tool_result(Err(error)))
        }
        "tools/call" => tool_result(call_tool(&request["params"])),
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
pub(crate) fn serve(once: bool) -> Result<i32> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut frame = Vec::new();
    loop {
        let response = match read_frame(&mut input, &mut frame).map_err(|e| e.to_string())? {
            Frame::End => break,
            Frame::Oversized => Some(error(&Value::Null, -32600, "Frame exceeds maximum size")),
            Frame::Ready => handle(&frame, !once),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")
                .and_then(|_| output.flush())
                .map_err(|e| e.to_string())?;
        }
        if once {
            break;
        }
    }
    Ok(0)
}

enum Frame {
    End,
    Ready,
    Oversized,
}

fn read_frame(input: &mut impl BufRead, frame: &mut Vec<u8>) -> io::Result<Frame> {
    frame.clear();
    let mut oversized = false;
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            if !oversized && frame.is_empty() {
                return Ok(Frame::End);
            }
            break;
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        if !oversized {
            oversized = length > MAX_MCP_FRAME_BYTES - frame.len();
            if !oversized {
                frame.extend_from_slice(&chunk[..length]);
            }
        }
        input.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    Ok(if oversized {
        Frame::Oversized
    } else {
        Frame::Ready
    })
}
