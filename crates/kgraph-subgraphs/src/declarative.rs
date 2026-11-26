//! Load native workflow graphs from a TOML or JSON document.

use crate::bug_fix::{
    create_bug_batch_graph, create_desktop_bug_fix_subgraph, outputs, resolve_path, BugBatchConfig,
    DesktopBugFixConfig,
};
use crate::common::{fingerprint, verification, with_retry_route};
use crate::histoury_bug_fix::{create_histoury_bug_fix_graph, parse_command, HistouryBugFixConfig};
use crate::regression::{create_game_regression_graph, GameRegressionConfig, VisualCheckConfig};
use crate::version_history::{create_version_history_subgraph, VersionHistoryConfig};
use kgraph_core::agent::{agent_task, AgentAdapter, AgentTaskOptions, EvaluatorConfig};
use kgraph_core::contracts::{apply_reducer, verify_json_file_schema, ReducerKind};
use kgraph_core::files::{read_object, require_id};
use kgraph_core::gate::{subagent_gate, SubagentGateOptions};
use kgraph_core::hash::{hash_globs, hash_paths};
use kgraph_core::repair::{attach_repair_context, build_repair_context};
use kgraph_core::shell::{run_command_streaming, shell_task, CommandSpec, ShellTaskOptions};
use kgraph_core::types::*;
use kgraph_core::verifiers::verify_png_file;
use kgraph_core::worktree::{IsolatedWorktree, MergePolicy, WorktreeSpec};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkflowBudgets {
    #[serde(default)]
    pub max_worker_calls: Option<usize>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_wall_time_s: Option<f64>,
}

pub struct LoadedWorkflow {
    pub title: Option<String>,
    pub nodes: Vec<Node>,
    pub finally_nodes: Vec<Node>,
    pub budgets: WorkflowBudgets,
    pub context: Context,
    pub qa_checklist_path: Option<PathBuf>,
}

static TEMPLATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{\{\s*(ctx\.[\w.-]+|item(?:\.[\w.-]+)?)\s*\}\}").unwrap());

fn validate_shell_template(key: &str, command: &Value) -> Result<(), String> {
    if command
        .as_str()
        .and_then(|s| s.split_once("{{"))
        .is_some_and(|(_, tail)| tail.contains("}}"))
    {
        return Err(format!(
            "{key}: template placeholders in shell command strings are unsafe. Use an argument array or pass values through env variables to a fixed shell string."
        ));
    }
    Ok(())
}

fn validate_subgraph_source(node: &Value) -> Result<(), String> {
    let sources = ["nodes", "from", "preset"];
    if sources
        .iter()
        .filter(|key| node.get(**key).is_some())
        .count()
        != 1
    {
        return Err(format!(
            "{}: subgraph requires exactly one of nodes, from, or preset",
            node["id"]
        ));
    }
    if node.get("preset").is_some() {
        if let Some(config) = node.get("config") {
            validate_preset_commands(config.as_object().ok_or("config must be an object")?)?;
        }
    }
    if node.get("from").is_some() && node.get("with").is_some_and(|v| !v.is_object()) {
        return Err("with must be an object".into());
    }
    Ok(())
}

fn validate_node_commands(node: &Value) -> Result<(), String> {
    // Splitter templates may never expand. Check the declared targets during loading too.
    if !node["output_schema"].is_null() {
        (Spec {
            value: node,
            base: Path::new("."),
        })
        .schema()?;
    }
    for key in ["command", "verify_cmd"] {
        validate_shell_template(key, &node[key])?;
    }
    if node["type"] == "subgraph" {
        validate_subgraph_source(node)?;
    }
    if let Some(children) = node["nodes"].as_array() {
        for child in children {
            validate_node_commands(child)?;
        }
    }
    if let Some(template) = node.get("template") {
        validate_node_commands(template)?;
    }
    Ok(())
}

fn validate_preset_commands(config: &Map<String, Value>) -> Result<(), String> {
    for key in ["build_cmd", "test_cmd", "repro_cmd"] {
        if let Some(command) = config.get(key) {
            validate_shell_template(key, command)?;
        }
        if let Some(bugs) = config.get("bugs").and_then(Value::as_array) {
            for bug in bugs {
                validate_shell_template(key, &bug[key])?;
            }
        }
    }
    for key in ["build_cmds", "test_cmds"] {
        if let Some(commands) = config.get(key).and_then(Value::as_object) {
            for command in commands.values() {
                validate_shell_template(key, command)?;
            }
        }
    }
    Ok(())
}

fn interpolate(s: &str, ctx: &Context, item: Option<&Value>) -> Result<String, String> {
    if !s.contains("{{") {
        return Ok(s.to_owned());
    }
    let mut result = String::with_capacity(s.len());
    let mut last = 0;
    for m in TEMPLATE.captures_iter(s) {
        let full = m.get(0).unwrap();
        result.push_str(&s[last..full.start()]);
        let mut parts = m[1].split('.');
        let mut value = match parts.next() {
            Some("ctx") => ctx.get(parts.next().unwrap()),
            _ => item.cloned(),
        };
        for key in parts {
            value = value.and_then(|v| v.get(key).cloned())
        }
        let value = value.ok_or_else(|| format!("No value for template {}", full.as_str()))?;
        match value {
            Value::String(v) => result.push_str(&v),
            v => result.push_str(&v.to_string()),
        }
        last = full.end();
    }
    result.push_str(&s[last..]);
    Ok(result)
}
fn render(v: &Value, ctx: &Context, item: Option<&Value>) -> Result<Value, String> {
    match v {
        Value::String(s) => interpolate(s, ctx, item).map(Value::String),
        Value::Array(a) => a
            .iter()
            .map(|v| render(v, ctx, item))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| Ok((k.clone(), render(v, ctx, item)?)))
            .collect::<Result<_, _>>()
            .map(Value::Object),
        v => Ok(v.clone()),
    }
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

struct OutputSchema {
    schema: Value,
    paths: Vec<PathBuf>,
}

impl OutputSchema {
    fn check(&self) -> Option<VerifyOutcome> {
        self.paths.iter().find_map(|p| {
            let outcome = verify_json_file_schema(p, &self.schema);
            if outcome.error_signature == "invalid_json" {
                // A document that cannot be parsed violates the contract at the root JSON pointer.
                Some(VerifyOutcome::fail("schema_violation:", outcome.detail))
            } else {
                (!outcome.ok).then_some(outcome)
            }
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct JsonChecks {
    require_resolved: bool,
    require_approved: bool,
    require_fields: Vec<String>,
}

impl JsonChecks {
    fn verify(&self, paths: &[PathBuf]) -> VerifyOutcome {
        for path in paths {
            let doc = match read_object(path) {
                Ok(doc) => doc,
                Err(e) => return VerifyOutcome::fail("invalid_json", e),
            };
            for (key, required) in [
                ("resolved", self.require_resolved),
                ("approved", self.require_approved),
            ] {
                if required && doc.get(key) != Some(&Value::Bool(true)) {
                    return VerifyOutcome::fail(
                        "invalid_json",
                        format!("{} requires {key} = true", path.display()),
                    );
                }
            }
            for key in &self.require_fields {
                if doc.get(key).is_none_or(Value::is_null) {
                    return VerifyOutcome::fail(
                        "invalid_json",
                        format!("{} requires {key}", path.display()),
                    );
                }
            }
        }
        VerifyOutcome::pass("JSON artifacts verified")
    }
}

fn verify_command(
    cmd: &CommandSpec,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Option<f64>,
) -> VerifyOutcome {
    match run_command_streaming(cmd, Some(cwd), env, timeout, None) {
        Ok(out) if out.returncode != 0 => VerifyOutcome::fail(
            format!("exit_{}", out.returncode),
            format!("{}{}", out.stdout, out.stderr),
        ),
        Err(e) => VerifyOutcome::fail("verify_cmd", e.to_string()),
        Ok(_) => VerifyOutcome::pass("Verification command passed"),
    }
}

fn repair_callback(id: String, retries: usize, log: Option<PathBuf>, cwd: PathBuf) -> RepairFn {
    Arc::new(move |ctx, attempt, sig, detail| {
        let bundle = build_repair_context(
            &id,
            attempt,
            retries,
            sig,
            detail,
            log.as_deref(),
            Some(&cwd),
        );
        attach_repair_context(ctx, &bundle);
        Ok(())
    })
}

// Decode supplied fields strictly while retaining native defaults for absent fields.
struct Spec<'a> {
    value: &'a Value,
    base: &'a Path,
}
impl Spec<'_> {
    fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T, String> {
        let v = self
            .value
            .get(key)
            .ok_or_else(|| format!("Missing {key}"))?;
        serde_json::from_value(v.clone()).map_err(|e| format!("{key}: {e}"))
    }
    fn or<T: DeserializeOwned>(&self, key: &str, default: T) -> Result<T, String> {
        if self.value.get(key).is_none() {
            Ok(default)
        } else {
            self.get(key)
        }
    }
    fn strings(&self, key: &str) -> Result<Vec<String>, String> {
        self.or(key, Vec::new())
    }
    fn path(&self, key: &str) -> Result<PathBuf, String> {
        Ok(self.base.join(self.get::<String>(key)?))
    }
    fn optional_path(&self, key: &str) -> Result<Option<PathBuf>, String> {
        Ok(self
            .or::<Option<String>>(key, None)?
            .map(|p| self.base.join(p)))
    }
    fn paths(&self, key: &str) -> Result<Vec<PathBuf>, String> {
        Ok(self
            .strings(key)?
            .into_iter()
            .map(|p| self.base.join(p))
            .collect())
    }
    fn command(&self, key: &str) -> Result<Option<CommandSpec>, String> {
        parse_command(&self.value[key], key)
    }
    fn required_command(&self, key: &str) -> Result<CommandSpec, String> {
        self.command(key)?.ok_or_else(|| format!("Missing {key}"))
    }
    fn commands(&self, key: &str) -> Result<BTreeMap<String, CommandSpec>, String> {
        self.or::<BTreeMap<String, Value>>(key, BTreeMap::new())?
            .into_iter()
            .map(|(id, v)| {
                let command = parse_command(&v, key)?
                    .ok_or_else(|| format!("Expected a command in {key}.{id}"))?;
                Ok((id, command))
            })
            .collect()
    }

    fn hash(&self, exclude: Vec<PathBuf>) -> Result<InputHashFn, String> {
        let spec_hash = kgraph_core::hash::sha256_json(self.value);
        let globs = self.strings("watch_globs")?;
        let paths = self.paths("watch_paths")?;
        if globs.is_empty() && paths.is_empty() {
            const EMPTY_SHA256: &str =
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
            let static_hash = fingerprint(json!([spec_hash, EMPTY_SHA256, EMPTY_SHA256]));
            return Ok(Arc::new(move |_| Ok(static_hash.clone())));
        }
        let base = self.base.to_owned();
        Ok(Arc::new(move |_| {
            let globs_hash = if globs.is_empty() {
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned()
            } else {
                hash_globs(&globs, Some(&base), None, &exclude, true)?
            };
            let paths_hash = if paths.is_empty() {
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned()
            } else {
                hash_paths(&paths, Some(&base), None, &exclude, true)?
            };
            Ok(fingerprint(json!([spec_hash, globs_hash, paths_hash])))
        }))
    }

    fn pngs(&self) -> Result<Vec<(PathBuf, u32, u32)>, String> {
        self.or::<Vec<Value>>("verify_pngs", vec![])?
            .into_iter()
            .map(|v| {
                if let Some(s) = v.as_str() {
                    return Ok((self.base.join(s), 1, 1));
                }
                let png = Spec {
                    base: self.base,
                    value: &v,
                };
                Ok((
                    png.path("path")?,
                    png.or("min_width", 1)?,
                    png.or("min_height", 1)?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()
    }

    fn schema(&self) -> Result<Option<OutputSchema>, String> {
        let Some(schema) = self.or::<Option<Map<String, Value>>>("output_schema", None)? else {
            return Ok(None);
        };
        let paths = if let Some(path) = self.optional_path("output_schema_path")? {
            vec![path]
        } else {
            let response = self.optional_path("response_output_path")?;
            let mut paths = self.paths("outputs")?;
            paths.extend(self.paths("artifact_paths")?);
            paths.extend(response.clone());
            if paths.iter().any(|p| is_json(p)) {
                paths.retain(|p| is_json(p));
            } else if let Some(path) = response {
                paths = vec![path];
            }
            paths
        };
        if paths.is_empty() || paths.iter().any(|p| p == self.base) {
            return Err("output_schema requires a target output path".into());
        }
        Ok(Some(OutputSchema {
            schema: Value::Object(schema),
            paths,
        }))
    }

    fn shell(&self, id: &str) -> Result<TaskNode, String> {
        let paths = self.paths("outputs")?;
        let log = self.optional_path("log_path")?;
        let mut excluded = paths.clone();
        excluded.extend(log.clone());
        let mut opts =
            ShellTaskOptions::new(id, self.required_command("command")?, self.hash(excluded)?);
        opts.cwd = Some(
            self.optional_path("cwd")?
                .unwrap_or_else(|| self.base.into()),
        );
        opts.env = self.or("env", BTreeMap::new())?;
        opts.log_path = log;
        opts.timeout_s = self.or("timeout_s", None)?;
        opts.max_retries = self.or("max_retries", 0)?;
        opts.kind = TaskKind::parse(&self.or("kind", "code".to_owned())?)
            .ok_or("Invalid shell task kind")?;
        opts.deps = self.strings("deps")?;
        opts.cacheable = self.or("cacheable", true)?;
        opts.resource = self.or("resource", None)?;
        let regex = self
            .or::<Option<String>>("expect_stdout_regex", None)?
            .map(|s| Regex::new(&s))
            .transpose()
            .map_err(|e| format!("expect_stdout_regex: {e}"))?;
        let pngs = self.pngs()?;
        let schema = self.schema()?;
        // Verified PNGs also participate in output freshness on subsequent runs.
        let mut tracked = paths.clone();
        tracked.extend(
            schema
                .iter()
                .flat_map(|s| s.paths.iter().cloned())
                .filter(|p| !paths.contains(p)),
        );
        for (p, _, _) in &pngs {
            if !tracked.contains(p) {
                tracked.push(p.clone())
            }
        }
        opts.outputs_fn = outputs(tracked);
        opts.verify_fn = Some(Arc::new(move |_, v| {
            if regex
                .as_ref()
                .is_some_and(|r| !r.is_match(v["stdout"].as_str().unwrap_or_default()))
            {
                return VerifyOutcome::fail(
                    "stdout_mismatch",
                    "Command stdout did not match expect_stdout_regex",
                );
            }
            if let Some(missing) = paths.iter().find(|p| !p.exists()) {
                return VerifyOutcome::fail(
                    "missing_output",
                    format!("Missing output {}", missing.display()),
                );
            }
            for (path, w, h) in &pngs {
                let check = verify_png_file(path, *w, *h);
                if !check.ok {
                    return check;
                }
            }
            schema
                .as_ref()
                .and_then(OutputSchema::check)
                .unwrap_or_else(|| VerifyOutcome::pass("Command passed"))
        }));
        Ok(shell_task(opts))
    }

    fn gate(&self, id: &str) -> Result<TaskNode, String> {
        let paths = self.paths("artifact_paths")?;
        if paths.is_empty() {
            return Err("A gate requires artifact_paths".into());
        }
        let packet = self.get::<Map<String, Value>>("packet")?;
        let packet_path = self.path("packet_path")?;
        let schema = self.schema()?;
        let mut paths = paths;
        for path in schema.iter().flat_map(|s| &s.paths) {
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        let mut tracked = paths.clone();
        tracked.push(packet_path.clone());
        let mut opts = SubagentGateOptions::new(
            id,
            packet_path,
            outputs(paths.clone()),
            Arc::new(move |_| Ok(Value::Object(packet.clone()))),
            self.hash(tracked)?,
        );
        opts.deps = self.strings("deps")?;
        opts.resource = self.or("resource", None)?;
        opts.cacheable = self.or("cacheable", true)?;
        let checks = self.or::<Option<JsonChecks>>("verify_json", None)?;
        if let Some(checks) = checks {
            opts.verify_fn = Some(Arc::new(move |_, artifacts| checks.verify(artifacts)))
        }
        let mut node = subagent_gate(opts);
        if let Some(schema) = schema {
            let artifacts = paths;
            let run = node.run_fn.clone();
            node.run_fn = Arc::new(move |ctx, index| {
                // Leave missing artifacts to the native gate's packet/pause path.
                if artifacts.iter().all(|path| path.exists()) {
                    if let Some(failure) = schema.check() {
                        return Err(TaskRunError::with_sig(
                            failure.error_signature,
                            failure.detail,
                        ));
                    }
                }
                run(ctx, index)
            });
        }
        Ok(node)
    }

    fn adapter(&self) -> Result<AgentAdapter, String> {
        let mut config = match &self.value["adapter"] {
            Value::Object(object) => object.clone(),
            Value::String(name) => {
                let mut config = Map::from_iter([("kind".into(), json!(name))]);
                if name == "custom" {
                    config.insert("argv".into(), self.get("argv")?);
                }
                config
            }
            _ => return Err("adapter must be a name or an object with kind".into()),
        };
        let kind = config.remove("kind").ok_or("adapter requires kind")?;
        config.insert("type".into(), kind);
        let adapter =
            serde_json::from_value(Value::Object(config)).map_err(|e| format!("adapter: {e}"))?;
        if matches!(&adapter, AgentAdapter::Custom { argv } if argv.first().is_none_or(String::is_empty))
        {
            return Err("custom adapter requires a nonempty argv".into());
        }
        Ok(adapter)
    }

    fn agent(&self, id: &str) -> Result<TaskNode, String> {
        let prompt = self.or::<Option<String>>("prompt", None)?;
        let prompt_file = self.optional_path("prompt_file")?;
        if prompt.is_some() == prompt_file.is_some() {
            return Err(format!(
                "{id}: specify exactly one of prompt and prompt_file"
            ));
        }
        let paths = self.paths("outputs")?;
        let response = self.optional_path("response_output_path")?;
        let log = self.optional_path("log_path")?;
        let prompt_artifact = self.optional_path("prompt_artifact_path")?;
        let evaluator = self
            .value
            .get("evaluator")
            .map(|v| {
                let e = Spec {
                    value: v,
                    base: self.base,
                };
                Ok::<_, String>(EvaluatorConfig {
                    adapter: e.adapter()?,
                    prompt: e.get("prompt")?,
                    timeout_s: e.or("timeout_s", None)?,
                    env: e.or("env", BTreeMap::new())?,
                    log_path: e.optional_path("log_path")?,
                })
            })
            .transpose()?;
        let pngs = self.pngs()?;
        let png_paths: Vec<_> = pngs.iter().map(|(p, _, _)| p.clone()).collect();
        let mut declared = paths.clone();
        declared.extend(response.clone());
        let mut excluded = declared.clone();
        excluded.extend(log.clone());
        excluded.extend(prompt_artifact.clone());
        excluded.extend(evaluator.as_ref().and_then(|e| e.log_path.clone()));
        excluded.extend(png_paths.iter().cloned());
        let mut opts = AgentTaskOptions::new(
            id,
            self.adapter()?,
            prompt.unwrap_or_default(),
            self.hash(excluded)?,
        );
        opts.prompt_file = prompt_file;
        opts.evaluator = evaluator;
        opts.outputs = paths;
        opts.response_output_path = response;
        opts.prompt_artifact_path = prompt_artifact;
        opts.log_path = log.clone();
        let cwd = self
            .optional_path("cwd")?
            .unwrap_or_else(|| self.base.into());
        opts.cwd = Some(cwd.clone());
        opts.env = self.or("env", BTreeMap::new())?;
        opts.timeout_s = self.or("timeout_s", None)?;
        opts.max_retries = self.or("max_retries", 0)?;
        opts.max_feedback_bytes = self.or("max_feedback_bytes", None)?;
        opts.deps = self.strings("deps")?;
        opts.resource = self.or("resource", None)?;
        opts.cacheable = self.or("cacheable", true)?;
        let schema = self.schema()?;
        for path in schema.iter().flat_map(|s| &s.paths) {
            if !declared.contains(path) {
                opts.outputs.push(path.clone());
            }
        }
        let checks = self.or::<Option<JsonChecks>>("verify_json", None)?;
        let cmd = self.command("verify_cmd")?;
        let verify_cwd = cwd.clone();
        let env = opts.env.clone();
        let timeout = opts.timeout_s;
        if schema.is_some() || checks.is_some() || cmd.is_some() || !pngs.is_empty() {
            opts.deterministic_verify_fn = Some(Arc::new(move |_, _| {
                if let Some(outcome) = schema.as_ref().and_then(OutputSchema::check) {
                    return outcome;
                }
                if let Some(checks) = &checks {
                    let files: Vec<_> = declared.iter().filter(|p| is_json(p)).cloned().collect();
                    if files.is_empty() {
                        return VerifyOutcome::fail(
                            "invalid_json",
                            "verify_json needs a declared JSON output",
                        );
                    }
                    let outcome = checks.verify(&files);
                    if !outcome.ok {
                        return outcome;
                    }
                }
                for (p, w, h) in &pngs {
                    let outcome = verify_png_file(p, *w, *h);
                    if !outcome.ok {
                        return outcome;
                    }
                }
                if let Some(cmd) = &cmd {
                    return verify_command(cmd, &verify_cwd, &env, timeout);
                }
                VerifyOutcome::pass("Agent outputs verified")
            }));
        }
        let mut node = agent_task(opts);
        let outputs = node.outputs_fn.clone();
        if !png_paths.is_empty() {
            node.outputs_fn = Arc::new(move |ctx| {
                let mut paths = outputs(ctx)?;
                for path in &png_paths {
                    if !paths.contains(path) {
                        paths.push(path.clone());
                    }
                }
                Ok(paths)
            });
        }
        node.repair_fn = Some(repair_callback(id.into(), node.max_retries, log, cwd));
        Ok(node)
    }

    #[allow(clippy::result_large_err)] // LoopRepairFn returns GraphFailure by value.
    fn review_loop(&self, id: &str, children: Vec<Node>) -> Result<Node, String> {
        let cmd = self.command("verify_cmd")?;
        let path = self.optional_path("verify_json_path")?;
        if cmd.is_none() && path.is_none() {
            return Err(format!("{id}: provide verify_cmd or verify_json_path"));
        }
        let cwd = self
            .optional_path("cwd")?
            .unwrap_or_else(|| self.base.into());
        let timeout = self.or("timeout_s", None)?;
        let repair_cwd = cwd.clone();
        let mut n = LoopSubgraphNode::new(
            id,
            children,
            Arc::new(move |_| {
                if let Some(cmd) = &cmd {
                    let checked = verify_command(cmd, &cwd, &BTreeMap::new(), timeout);
                    if !checked.ok {
                        return Ok(checked);
                    }
                }
                if let Some(path) = &path {
                    return Ok(verification(path)
                        .unwrap_or_else(|e| VerifyOutcome::fail("invalid_verification", e)));
                }
                Ok(VerifyOutcome::pass("Verification command passed"))
            }),
        );
        n.max_retries = self.or("max_retries", n.max_retries)?;
        n.retry_policy = self.retry_policy()?;
        n.protected_paths = self.protected_paths()?;
        n.deps = self.strings("deps")?;
        n.resource = self.or("resource", None)?;
        let dirty = self.or(
            "dirty_nodes",
            n.nodes
                .iter()
                .map(|n| n.id().to_owned())
                .collect::<Vec<_>>(),
        )?;
        let loop_id = id.to_owned();
        let retries = n.max_retries;
        let log = self.optional_path("log_path")?;
        n.repair_fn = Some(Arc::new(move |ctx, attempt, sig, detail| {
            let mut repair = build_repair_context(
                &loop_id,
                attempt,
                retries,
                sig,
                detail,
                log.as_deref(),
                Some(&repair_cwd),
            );
            attach_repair_context(ctx, &repair);
            for child in &dirty {
                repair.node_path = child.clone();
                attach_repair_context(ctx, &repair);
                // Agent prompts look up the local ID even when dirty_nodes names a nested path.
                if let Some((_, local)) = child.rsplit_once('/') {
                    repair.node_path = local.into();
                    attach_repair_context(ctx, &repair);
                }
            }
            Ok(dirty.clone())
        }));
        match (
            self.or::<Option<String>>("retry_from", None)?,
            self.or::<Option<String>>("retry_to", None)?,
        ) {
            (Some(from), Some(to)) => {
                n = with_retry_route(n, from, to, &self.or("retry_label", "retry".to_owned())?)
            }
            (None, None) => (),
            _ => return Err("Specify both retry_from and retry_to".into()),
        }
        Ok(Node::LoopSubgraph(n))
    }
}

#[derive(Clone, Deserialize)]
struct ExportJson {
    key: String,
    path: PathBuf,
}

fn contained_path(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let joined = base.join(path);
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
        || !joined.starts_with(base)
    {
        return Err(format!(
            "{} must stay inside {}",
            path.display(),
            base.display()
        ));
    }
    let resolved = resolve_path(&joined)?;
    if !resolved.starts_with(base) {
        return Err(format!(
            "{} must stay inside {}",
            path.display(),
            base.display()
        ));
    }
    Ok(resolved)
}

// Open the resolved components without following a replacement symlink between
// containment validation and reading the file. Nonblocking open also rejects FIFOs promptly.
#[cfg(unix)]
fn open_resolved(path: &Path) -> std::io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) {
        return Err(std::io::Error::other("Expected an absolute path"));
    }
    let mut file = fs::File::open("/")?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::other("Expected a resolved path"));
        };
        let name = CString::new(name.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if components.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        // The directory fd remains live for the lookup. The new fd belongs to File.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        file = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok(file)
}

fn contained_file(base: &Path, path: &Path) -> Result<fs::File, String> {
    open_regular_file(&contained_path(base, path)?)
}

fn open_regular_file(path: &Path) -> Result<fs::File, String> {
    #[cfg(unix)]
    let file = open_resolved(path);
    #[cfg(not(unix))]
    let file = {
        if !fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .is_file()
        {
            return Err(format!("Expected a regular file at {}", path.display()));
        }
        fs::File::open(path)
    };
    let file = file.map_err(|e| format!("{}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("Expected a regular file at {}", path.display()));
    }
    Ok(file)
}

fn read_value(base: &Path, path: &Path) -> Result<Value, String> {
    let file = contained_file(base, path)?;
    serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Isolation {
    base_ref: Option<String>,
    branch_prefix: Option<String>,
    keep_on_failure: bool,
    merge_policy: Option<MergePolicy>,
    patch_dir: Option<PathBuf>,
}

impl Isolation {
    fn parse(spec: &Spec) -> Result<Option<Self>, String> {
        match spec.or::<String>("isolation", "none".into())?.as_str() {
            "none" => Ok(None),
            "worktree" => Ok(Some(spec.or("worktree", Self::default())?)),
            other => Err(format!("Unknown isolation {other}")),
        }
    }

    fn create(&self, spec: &Spec, id: &str) -> Result<IsolatedWorktree, String> {
        let cwd = spec
            .optional_path("cwd")?
            .unwrap_or_else(|| spec.base.into());
        let output = std::process::Command::new("git")
            .current_dir(&cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().into());
        }
        let root = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        IsolatedWorktree::create(WorktreeSpec {
            repo_root: PathBuf::from(root.trim_end_matches(['\r', '\n'])),
            worktree_base: contained_path(spec.base, Path::new(".kgraph/worktrees"))?,
            node_path: id.into(),
            base_ref: self.base_ref.clone(),
            branch_prefix: self.branch_prefix.clone(),
            keep_on_failure: self.keep_on_failure,
            merge_policy: self.merge_policy.unwrap_or(MergePolicy::None),
        })
    }
}

const MAX_IMPORT_DEPTH: usize = 8;
const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone)]
struct ImportLocation {
    root: PathBuf,
    directory: PathBuf,
    chain: Vec<PathBuf>,
}

fn library_root() -> Result<PathBuf, String> {
    let env_path = |key| {
        std::env::var_os(key)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    };
    let home = env_path("KGRAPH_HOME")
        .or_else(|| {
            env_path("HOME")
                .or_else(|| env_path("USERPROFILE"))
                .map(|p| p.join(".kgraph"))
        })
        .ok_or("Set KGRAPH_HOME or HOME to resolve @lib imports")?;
    Ok(resolve_path(&home)?.join("subgraphs"))
}

impl ImportLocation {
    fn load(&self, from: &str) -> Result<(Self, Value), String> {
        use std::io::Read;
        use std::path::Component;

        let (root, requested) = if let Some(name) = from.strip_prefix("@lib/") {
            let name_path = Path::new(name);
            if name.is_empty()
                || name.contains("..")
                || name.contains(['\\', '\0'])
                || name_path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
                || name.split('/').any(|part| part.is_empty() || part == ".")
            {
                return Err(format!("Invalid @lib subgraph name: {name:?}"));
            }
            let root = library_root()?;
            let mut path = root.join(name);
            if path.extension().is_none() {
                path.set_extension("toml");
                if let Err(error) = fs::symlink_metadata(&path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        return Err(format!("{}: {error}", path.display()));
                    }
                    path.set_extension("json");
                }
            }
            (root, path)
        } else {
            let path = Path::new(from);
            if from.is_empty()
                || path.has_root()
                || matches!(path.components().next(), Some(Component::Prefix(_)))
            {
                return Err(format!("Expected a relative subgraph import: {from:?}"));
            }
            (self.root.clone(), self.directory.join(path))
        };
        let path = resolve_path(&requested)?;
        if !path.starts_with(&root) {
            return Err(format!(
                "Subgraph import {from:?} must stay inside {}",
                root.display()
            ));
        }
        let mut chain = self.chain.clone();
        if chain.contains(&path) {
            chain.push(path);
            return Err(format!(
                "Circular subgraph import: {}",
                chain
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ));
        }
        // The entry workflow occupies the first slot and is not itself an import.
        if chain.len() > MAX_IMPORT_DEPTH {
            return Err(format!(
                "Subgraph import nesting exceeds limit {MAX_IMPORT_DEPTH}: {}",
                path.display()
            ));
        }
        let file = open_regular_file(&path)?;
        let too_large = || format!("Subgraph import exceeds 2 MiB: {}", path.display());
        if file.metadata().map_err(|e| e.to_string())?.len() > MAX_IMPORT_BYTES {
            return Err(too_large());
        }
        let mut raw = String::new();
        file.take(MAX_IMPORT_BYTES + 1)
            .read_to_string(&mut raw)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if raw.len() as u64 > MAX_IMPORT_BYTES {
            return Err(too_large());
        }
        let doc = parse_document(&requested, &raw)?;
        let directory = path
            .parent()
            .ok_or("Missing subgraph directory")?
            .to_owned();
        chain.push(path);
        Ok((
            Self {
                root,
                directory,
                chain,
            },
            doc,
        ))
    }
}

// A reducing branch keeps its own view of published keys so nested children can reuse
// the same export names as siblings. Other workflow context remains shared.
#[derive(Clone)]
struct BuildScope {
    imports: ImportLocation,
    // Imports get private bindings. Explicit exports also reach an enclosing reducing branch.
    enclosing_locals: Vec<Context>,
    path: String,
    local: Option<Context>,
    published_keys: Arc<BTreeSet<String>>,
    captures: Vec<FileCapture>,
}

#[derive(Clone)]
struct FileCapture {
    path: PathBuf,
    op: ReducerKind,
    value: Arc<Mutex<Option<Value>>>,
}

impl BuildScope {
    fn published_keys(nodes: &Value) -> BTreeSet<String> {
        fn visit(node: &Value, keys: &mut BTreeSet<String>) {
            for pointer in ["/export_json/key", "/reduce/to_ctx"] {
                if let Some(key) = node.pointer(pointer).and_then(Value::as_str) {
                    keys.insert(key.to_owned());
                }
            }
            if let Some(template) = node.get("template") {
                visit(template, keys);
            }
            if let Some(nodes) = node["nodes"].as_array() {
                for child in nodes {
                    visit(child, keys);
                }
            }
        }
        let mut keys = BTreeSet::new();
        for node in nodes.as_array().into_iter().flatten() {
            visit(node, &mut keys);
        }
        keys
    }

    fn workflow(nodes: &Value, base: &Path, path: &Path) -> Result<Self, String> {
        Ok(Self {
            imports: ImportLocation {
                root: base.into(),
                directory: base.into(),
                chain: vec![resolve_path(path)?],
            },
            enclosing_locals: vec![],
            path: String::new(),
            local: None,
            published_keys: Arc::new(Self::published_keys(nodes)),
            captures: vec![],
        })
    }

    fn import(
        &self,
        spec: &Spec,
        ctx: &Context,
        item: Option<&Value>,
    ) -> Result<Vec<Node>, String> {
        let (imports, document) = self.imports.load(&spec.get::<String>("from")?)?;
        let mut fields = document
            .as_object()
            .ok_or("Imported workflow must be an object")?
            .clone();
        let defaults = fields
            .remove("default_context")
            .unwrap_or_else(|| json!({}));
        let mut bindings = self
            .local
            .as_ref()
            .map(Context::snapshot_map)
            .unwrap_or_default();
        for (key, value) in defaults
            .as_object()
            .ok_or("default_context must be an object")?
        {
            if ctx.get(key).is_none() {
                bindings.insert(key.clone(), value.clone());
            }
        }
        bindings.extend(spec.or::<Map<String, Value>>("with", Map::new())?);
        let mut child = Self {
            imports,
            local: Some(Context::from_map(bindings)),
            ..self.clone()
        };
        child.enclosing_locals.extend(self.local.clone());
        match (fields.remove("nodes"), fields.remove("preset")) {
            (Some(mut nodes), None) => {
                for node in nodes.as_array_mut().ok_or("nodes must be an array")? {
                    validate_node_commands(node)?;
                    if spec.value["isolation"] == "worktree" {
                        inherit_worktree(node, spec.value)?;
                    }
                }
                Arc::make_mut(&mut child.published_keys).extend(Self::published_keys(&nodes));
                build_nodes(&nodes, spec.base, ctx, item, &child)
            }
            (None, Some(Value::String(name))) => {
                if spec.value["isolation"] == "worktree" {
                    return Err(
                        "Built-in presets do not support inherited worktree isolation".into(),
                    );
                }
                validate_preset_document(&fields)?;
                let rendered = render(&Value::Object(fields), &child.context(ctx), item)?;
                preset_document(&name, spec.base, &rendered)
            }
            _ => Err("Imported workflow requires either a nodes array or a named preset".into()),
        }
    }

    fn runtime_key(&self, name: &str) -> bool {
        matches!(name, "__kgraph_run_status" | "__kgraph_failure")
            || self.published_keys.contains(name)
            || self.published_keys.iter().any(|key| TEMPLATE.is_match(key))
    }

    fn child(&self, id: &str) -> Self {
        let path = if self.path.is_empty() {
            id.into()
        } else {
            format!("{}/{id}", self.path)
        };
        Self {
            path,
            ..self.clone()
        }
    }

    fn context(&self, ctx: &Context) -> Context {
        if let Some(local) = &self.local {
            let mut values = ctx.snapshot_map();
            values.extend(local.snapshot_map());
            Context::from_map(values)
        } else {
            ctx.clone()
        }
    }

    fn publish(&self, ctx: &Context, key: &str, value: Value) {
        if let Some(local) = &self.local {
            local.insert(key, value.clone());
        }
        for local in &self.enclosing_locals {
            local.insert(key, value.clone());
        }
        ctx.insert(key, value);
    }
}

#[derive(Clone, Deserialize)]
struct Reduction {
    from_output: Option<PathBuf>,
    from_ctx_key: Option<String>,
    to_ctx: String,
    op: ReducerKind,
}

#[derive(Clone)]
struct ReductionSlot {
    parent: BuildScope,
    spec: Reduction,
    index: usize,
    slots: Arc<Mutex<Vec<Option<Value>>>>,
}

impl ReductionSlot {
    fn value(
        &self,
        base: &Path,
        ctx: &Context,
        export: Option<(&ExportJson, &Value)>,
    ) -> Result<Value, String> {
        if let Some(path) = &self.spec.from_output {
            return read_value(base, path);
        }
        let key = self
            .spec
            .from_ctx_key
            .as_ref()
            .ok_or("Missing reducer source")?;
        if let Some((_, val)) = export.filter(|(e, _)| &e.key == key) {
            // A sibling may publish the same key. Use this child's verified value directly.
            return Ok(val.clone());
        }
        ctx.get(key)
            .ok_or_else(|| format!("Missing reducer context key {key}"))
    }

    fn prepare(&self, value: Value) -> Result<PreparedReduction<'_>, String> {
        // Hold this slot set until integration succeeds. Siblings cannot validate against
        // an uncommitted contribution or publish a result containing a failed integration.
        let slots = self.slots.lock().unwrap();
        let complete = slots
            .iter()
            .enumerate()
            .all(|(i, v)| i == self.index || v.is_some());
        let aggregate = if complete {
            let ordered: Vec<_> = slots
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    if i == self.index {
                        value.clone()
                    } else {
                        v.as_ref().unwrap().clone()
                    }
                })
                .collect();
            Some(apply_reducer(self.spec.op, &ordered)?)
        } else {
            // Validate the individual contribution. A partial numeric sum can overflow
            // even when the final, ordered sum is representable.
            apply_reducer(self.spec.op, std::slice::from_ref(&value))?;
            if matches!(self.spec.op, ReducerKind::Concat)
                && slots
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != self.index)
                    .filter_map(|(_, v)| v.as_ref())
                    .next()
                    .is_some_and(|previous| previous.is_array() != value.is_array())
            {
                return Err("concat requires homogeneous strings or arrays".into());
            }
            None
        };
        Ok(PreparedReduction {
            slot: self,
            slots,
            value,
            aggregate,
        })
    }
}

struct PreparedReduction<'a> {
    slot: &'a ReductionSlot,
    slots: std::sync::MutexGuard<'a, Vec<Option<Value>>>,
    value: Value,
    aggregate: Option<Value>,
}

impl PreparedReduction<'_> {
    fn publish(mut self, ctx: &Context) {
        self.slots[self.slot.index] = Some(self.value);
        if let Some(result) = self.aggregate {
            self.slot
                .parent
                .publish(ctx, &self.slot.spec.to_ctx, result);
        }
    }
}

impl Spec<'_> {
    fn retry_policy(&self) -> Result<RetryPolicy, String> {
        let backoff_s: Option<f64> = self.or("retry_backoff_s", None)?;
        if let Some(b) = backoff_s {
            if !b.is_finite() || b < 0.0 {
                return Err("retry_backoff_s must be a non-negative finite number".into());
            }
        }
        let max_backoff_s: Option<f64> = self.or("retry_max_backoff_s", None)?;
        if let Some(m) = max_backoff_s {
            if !m.is_finite() || m <= 0.0 {
                return Err("retry_max_backoff_s must be a positive finite number".into());
            }
        }
        let transient_error_regex: Option<String> = self.or("transient_error_regex", None)?;
        if let Some(ref re) = transient_error_regex {
            Regex::new(re).map_err(|e| format!("Invalid transient_error_regex: {e}"))?;
        }
        Ok(RetryPolicy {
            backoff_s,
            max_backoff_s,
            jitter: self.or("retry_jitter", true)?,
            transient_retries: self.or("transient_retries", 0)?,
            transient_error_regex,
            oscillation_breaker: self.or("oscillation_breaker", true)?,
        })
    }

    fn protected_paths(&self) -> Result<Option<ProtectedPaths>, String> {
        let mut paths: Vec<PathBuf> = self
            .strings("immutable_paths")?
            .into_iter()
            .map(PathBuf::from)
            .collect();
        paths.extend(self.strings("protect_paths")?.into_iter().map(PathBuf::from));
        let mut globs = self.strings("protect_globs")?;
        globs.extend(self.strings("immutable_globs")?);
        if paths.is_empty() && globs.is_empty() {
            return Ok(None);
        }
        let base_dir = self
            .optional_path("cwd")?
            .unwrap_or_else(|| self.base.into());
        Ok(Some(ProtectedPaths {
            base_dir,
            paths,
            globs,
        }))
    }

    fn task(&self, id: &str) -> Result<TaskNode, String> {
        let mut node = match self.value["type"].as_str() {
            Some("agent") => self.agent(id)?,
            Some("gate") => self.gate(id)?,
            Some("shell") => self.shell(id)?,
            _ => return Err("Expected task node".into()),
        };
        node.retry_policy = self.retry_policy()?;
        node.protected_paths = self.protected_paths()?;
        Ok(node)
    }
}

// Keep execution-time fields in their original form to render once against the current context.
// In particular, placeholders in an interpolated value must never be interpreted as code.
#[derive(Clone)]
struct TaskInvocation {
    scope: BuildScope,
    pending_repair: Arc<Mutex<Option<kgraph_core::repair::RepairContext>>>,
    fields: Value,
    late_fields: Map<String, Value>,
    item: Option<Value>,
    base: PathBuf,
    id: String,
    isolation: Option<Isolation>,
    export: Option<ExportJson>,
    reduction: Option<ReductionSlot>,
}

impl TaskInvocation {
    fn fields(&self, ctx: &Context) -> Result<Value, String> {
        let ctx = &self.scope.context(ctx);
        let mut fields = self.fields.clone();
        for (key, raw) in &self.late_fields {
            fields[key] = render(raw, ctx, self.item.as_ref())?;
        }
        if let Some(path) = fields["prompt_file"]
            .as_str()
            .filter(|_| fields["type"] == "agent")
        {
            use std::io::Read;
            let mut prompt = String::new();
            contained_file(&self.base, Path::new(path))?
                .read_to_string(&mut prompt)
                .map_err(|e| format!("{path}: {e}"))?;
            fields.as_object_mut().unwrap().remove("prompt_file");
            fields["prompt"] = json!(interpolate(&prompt, ctx, self.item.as_ref())?);
        }
        Ok(fields)
    }

    fn run(&self, ctx: &Context, index: usize) -> TaskResult {
        let mut fields = self.fields(ctx).map_err(TaskRunError::other)?;
        let wt = self
            .isolation
            .as_ref()
            .map(|iso| {
                iso.create(
                    &Spec {
                        value: &fields,
                        base: &self.base,
                    },
                    &self.scope.path,
                )
            })
            .transpose()
            .map_err(TaskRunError::other)?;
        let base = wt
            .as_ref()
            .map_or(self.base.as_path(), IsolatedWorktree::path)
            .to_owned();
        if wt.is_some() {
            fields["cwd"] = json!(base);
            fields["env"]["KGRAPH_WORKTREE"] = json!(base);
            if fields["evaluator"].is_object() {
                fields["evaluator"]["env"]["KGRAPH_WORKTREE"] = json!(base);
            }
        }
        let spec = Spec {
            value: &fields,
            base: &base,
        };
        let execute = || -> Result<_, TaskRunError> {
            let task = spec.task(&self.id).map_err(TaskRunError::other)?;
            let result = (task.run_fn)(ctx, index)?;
            let mut outcome = task.verify_fn.as_ref().unwrap()(ctx, &result);
            let mut exported = None;
            let mut reduced = None;
            if outcome.ok {
                let collect = || -> Result<_, String> {
                    let export = self
                        .export
                        .as_ref()
                        .map(|e| read_value(&base, &e.path))
                        .transpose()?;
                    let reduced = self
                        .reduction
                        .as_ref()
                        .map(|r| {
                            r.value(
                                &base,
                                self.scope.local.as_ref().unwrap_or(ctx),
                                self.export.as_ref().zip(export.as_ref()),
                            )
                        })
                        .transpose()?;
                    Ok((export, reduced))
                };
                match collect() {
                    Ok(values) => (exported, reduced) = values,
                    Err(e) => outcome = VerifyOutcome::fail("json_publication", e),
                }
            }
            Ok((result, outcome, exported, reduced))
        };
        let mut result = execute();
        let mut prepared = None;
        if let Ok((_, outcome, _, reduced)) = &mut result {
            if let Some((slot, value)) = self.reduction.as_ref().zip(reduced.take()) {
                match slot.prepare(value) {
                    Ok(proposal) => prepared = Some(proposal),
                    Err(e) => *outcome = VerifyOutcome::fail("reduce", e),
                }
            }
        }
        if let Some(wt) = wt {
            let patch = self
                .isolation
                .as_ref()
                .and_then(|i| i.patch_dir.as_ref())
                .map(|dir| {
                    contained_path(&self.base, &dir.join(format!("{}.patch", self.scope.path)))
                })
                .transpose();
            let patch = match patch {
                Ok(path) => path,
                Err(e) => {
                    wt.finish(false, None).map_err(TaskRunError::other)?;
                    return Err(TaskRunError::other(e));
                }
            };
            let mut captured = Vec::new();
            if let Ok((_, outcome, _, _)) = &mut result {
                if outcome.ok {
                    for capture in &self.scope.captures {
                        let read = || -> Result<Option<Value>, String> {
                            let path = contained_path(&base, &capture.path)?;
                            if !path.try_exists().map_err(|e| e.to_string())? {
                                return Ok(None);
                            }
                            let value = read_value(&base, &path)?;
                            apply_reducer(capture.op, std::slice::from_ref(&value))?;
                            Ok(Some(value))
                        };
                        match read() {
                            Ok(Some(value)) => captured.push((capture, value)),
                            Ok(None) => {}
                            Err(e) => {
                                *outcome = VerifyOutcome::fail("reduce", e);
                                break;
                            }
                        }
                    }
                }
            }
            let passed = result.as_ref().is_ok_and(|(_, check, _, _)| check.ok);
            if !passed
                && fields["type"] == "agent"
                && index < spec.or("max_retries", 0).map_err(TaskRunError::other)?
            {
                let (sig, detail) = match &result {
                    Ok((_, outcome, _, _)) => {
                        (outcome.error_signature.clone(), outcome.detail.clone())
                    }
                    Err(e) => ("agent_error".into(), e.to_string()),
                };
                let repair = build_repair_context(
                    &self.id,
                    index + 1,
                    spec.or("max_retries", 0).map_err(TaskRunError::other)?,
                    &sig,
                    &detail,
                    spec.optional_path("log_path")
                        .map_err(TaskRunError::other)?
                        .as_deref(),
                    Some(wt.path()),
                );
                *self.pending_repair.lock().unwrap() = Some(repair);
            }
            let finished = wt
                .finish(passed, patch.as_deref())
                .map_err(TaskRunError::other)?;
            for (capture, value) in captured.into_iter().filter(|_| passed) {
                let mut stored = capture.value.lock().unwrap();
                // A later isolated task may have checked out the original version of this file.
                if stored.is_none()
                    || finished
                        .changed_files
                        .iter()
                        .any(|p| Path::new(p) == capture.path)
                {
                    *stored = Some(value);
                }
            }
        }
        let (result, outcome, exported, _) = result?;
        if outcome.ok {
            if let Some((export, value)) = self.export.as_ref().zip(exported) {
                self.scope.publish(ctx, &export.key, value);
            }
            if let Some(reduction) = prepared {
                reduction.publish(ctx);
            }
        }
        // Keep native usage/efficiency fields available to the runner.
        let mut result = if result.is_object() {
            result
        } else {
            json!({"result":result})
        };
        result["verification"] = json!({"ok":outcome.ok, "error_signature":outcome.error_signature, "detail":outcome.detail});
        Ok(result)
    }

    fn wrap(self, mut task: TaskNode) -> TaskNode {
        let invocation = Arc::new(self);
        let run = invocation.clone();
        task.run_fn = Arc::new(move |ctx, index| run.run(ctx, index));
        task.verify_fn = Some(Arc::new(|_, result| {
            let check = &result["verification"];
            let detail = check["detail"]
                .as_str()
                .unwrap_or("Missing verification outcome");
            if check["ok"] == true {
                VerifyOutcome::pass(detail)
            } else {
                VerifyOutcome::fail(
                    check["error_signature"]
                        .as_str()
                        .unwrap_or("invalid_result"),
                    detail,
                )
            }
        }));
        let input = task.input_hash_fn.clone();
        if let Some(export) = &invocation.export {
            let base = invocation.base.clone();
            let export_path = export.path.clone();
            let outputs = task.outputs_fn.clone();
            task.outputs_fn = Arc::new(move |ctx| {
                let path = contained_path(&base, &export_path)?;
                let mut paths = outputs(ctx)?;
                if !paths.contains(&path) {
                    paths.push(path.clone());
                }
                Ok(paths)
            });
        }
        if invocation.isolation.is_some() {
            // Disposed worktree outputs cannot prove freshness in the source checkout.
            task.cacheable = false;
            task.outputs_fn = outputs(Vec::new());
            if let Some(previous) = task.repair_fn.take() {
                let pending = invocation.pending_repair.clone();
                task.repair_fn = Some(Arc::new(move |ctx, attempt, sig, detail| {
                    let captured = pending.lock().unwrap().take();
                    if let Some(repair) = captured.filter(|r| r.attempt == attempt) {
                        attach_repair_context(ctx, &repair);
                        Ok(())
                    } else {
                        previous(ctx, attempt, sig, detail)
                    }
                }));
            }
        }
        task.input_hash_fn = Arc::new(move |ctx| {
            let export = invocation
                .export
                .as_ref()
                .and_then(|e| invocation.scope.local.as_ref().unwrap_or(ctx).get(&e.key));
            let filled = invocation
                .reduction
                .as_ref()
                .map(|r| r.slots.lock().unwrap()[r.index].is_some());
            Ok(fingerprint(json!([
                input(ctx)?,
                invocation.fields(ctx)?,
                export.is_some(),
                export,
                filled
            ])))
        });
        task
    }
}

fn build_node(
    spec: &Value,
    base: &Path,
    ctx: &Context,
    item: Option<&Value>,
    scope: &BuildScope,
    reduction: Option<ReductionSlot>,
) -> Result<Node, String> {
    let ctx = &scope.context(ctx);
    let mut fields = spec
        .as_object()
        .ok_or("Each node must be an object")?
        .clone();
    // A nested splitter binds its own item when it expands. Keep its child template intact.
    let template = fields.remove("template");
    let children = fields.remove("nodes");
    let reduce = fields.remove("reduce");
    let mut late_fields = Map::new();
    if matches!(
        fields.get("type").and_then(Value::as_str),
        Some("shell" | "gate" | "agent")
    ) {
        for key in [
            "prompt",
            "env",
            "command",
            "verify_cmd",
            "packet",
            "evaluator",
            "adapter",
            "argv",
        ] {
            if let Some(raw) = fields.get(key).filter(|v| {
                TEMPLATE.captures_iter(&v.to_string()).any(|m| {
                    m[1].strip_prefix("ctx.").is_some_and(|name| {
                        fields["type"] == "agent"
                            || scope.runtime_key(name.split('.').next().unwrap())
                    })
                })
            }) {
                late_fields.insert(key.into(), raw.clone());
            }
        }
        for key in late_fields.keys() {
            fields.remove(key);
        }
    }
    let mut rendered = render(&Value::Object(fields), ctx, item)?;
    for (key, value) in &late_fields {
        rendered[key] = match render(value, ctx, item) {
            Ok(rendered) => rendered,
            Err(_)
                if TEMPLATE.captures_iter(&value.to_string()).any(|m| {
                    m[1].strip_prefix("ctx.")
                        .is_some_and(|name| scope.runtime_key(name.split('.').next().unwrap()))
                }) =>
            {
                value.clone()
            }
            Err(e) => return Err(e),
        };
    }
    let s = Spec {
        value: &rendered,
        base,
    };
    let id = require_id(&s.get::<String>("id")?)?;
    let scope = scope.child(&id);
    let build = || {
        build_nodes(
            children.as_ref().ok_or("Missing child nodes")?,
            base,
            ctx,
            item,
            &scope,
        )
    };
    let when_expr: Option<String> = s.or("when", None)?;
    if let Some(ref expr) = when_expr {
        parse_when_expr(expr)?;
    }
    let when_scope = scope.clone();
    let when_item = item.cloned();
    let when_fn: Option<WhenFn> = when_expr.clone().map(|expr| {
        Arc::new(move |ctx: &Context| {
            eval_when_expr(&expr, &when_scope.context(ctx), when_item.as_ref(), &when_scope)
        }) as WhenFn
    });
    let mut node = match s.get::<String>("type")?.as_str() {
        "shell" | "agent" | "gate" => {
            let task = s.task(&id)?;
            let isolation = Isolation::parse(&s)?;
            if isolation.is_some() && rendered["type"] == "gate" {
                return Err("isolation requires a shell or agent".into());
            }
            let export: Option<ExportJson> = s.or("export_json", None)?;
            if isolation.is_none()
                && export.is_none()
                && reduction.is_none()
                && late_fields.is_empty()
                && !(rendered["type"] == "agent" && rendered.get("prompt_file").is_some())
            {
                Node::Task(task)
            } else {
                Node::Task(
                    TaskInvocation {
                        scope,
                        pending_repair: Default::default(),
                        fields: rendered,
                        base: base.into(),
                        item: item.cloned(),
                        id,
                        late_fields,
                        isolation,
                        export,
                        reduction,
                    }
                    .wrap(task),
                )
            }
        }
        "subgraph" => {
            if spec["type"] != "subgraph" {
                // A templated type was unknown during document validation.
                validate_subgraph_source(spec)?;
            }
            let nodes = if rendered.get("from").is_some() {
                scope.import(&s, ctx, item)?
            } else if rendered.get("preset").is_some() {
                let config = s.or("config", Map::<String, Value>::new())?;
                Spec {
                    base,
                    value: &Value::Object(config),
                }
                .preset(&s.get::<String>("preset")?)?
            } else {
                build()?
            };
            let mut n = SubgraphNode::new(id, nodes);
            n.qa_checklist_path = graph_qa_checklist(&n.nodes);
            n.deps = s.strings("deps")?;
            n.resource = s.or("resource", None)?;
            Node::Subgraph(n)
        }
        "loop_subgraph" => s.review_loop(&id, build()?)?,
        "splitter" => {
            let items = s.or::<Option<Vec<Value>>>("items", None)?;
            let key = s.or::<Option<String>>("items_from_ctx", None)?;
            let item_scope = scope.clone();
            let items_fn: SplitterItemsFn = match (items, key) {
                (Some(items), None) => Arc::new(move |_| Ok(items.clone())),
                (None, Some(key)) => Arc::new(move |ctx| {
                    item_scope
                        .context(ctx)
                        .get(&key)
                        .and_then(|v| v.as_array().cloned())
                        .ok_or_else(|| format!("items_from_ctx {key} must identify an array"))
                }),
                _ => return Err("Specify exactly one of items and items_from_ctx".into()),
            };
            let mut template = template
                .filter(Value::is_object)
                .ok_or("Missing splitter template object")?;
            if Isolation::parse(&s)?.is_some() {
                inherit_worktree(&mut template, &rendered)?;
            }
            let dir = base.to_owned();
            let mut n = if let Some(reduce) = reduce {
                let mut spec: Reduction =
                    serde_json::from_value(reduce).map_err(|e| format!("reduce: {e}"))?;
                if spec.from_ctx_key.is_some() == spec.from_output.is_some() {
                    return Err(
                        "reduce requires exactly one of from_output and from_ctx_key".into(),
                    );
                }
                spec.to_ctx = interpolate(&spec.to_ctx, ctx, item)?;
                reducing_splitter(id, items_fn, template, dir, spec, scope)
            } else {
                SplitterNode::new(
                    id,
                    items_fn,
                    Arc::new(move |item, ctx| {
                        build_node(&template, &dir, ctx, Some(item), &scope, None)
                    }),
                )
            };
            n.max_workers = s.or("max_workers", n.max_workers)?;
            if n.max_workers == 0 {
                return Err("max_workers must be positive".into());
            }
            n.deps = s.strings("deps")?;
            n.resource = s.or("resource", None)?;
            Node::Splitter(n)
        }
        kind => return Err(format!("{id}: unknown node type {kind:?}")),
    };
    if let Some(when_fn) = when_fn {
        match &mut node {
            Node::Task(n) => {
                n.when_fn = Some(when_fn);
                n.when_expr = when_expr;
            }
            Node::Subgraph(n) => {
                n.when_fn = Some(when_fn);
                n.when_expr = when_expr;
            }
            Node::LoopSubgraph(n) => {
                n.when_fn = Some(when_fn);
                n.when_expr = when_expr;
            }
            Node::Splitter(n) => {
                n.when_fn = Some(when_fn);
                n.when_expr = when_expr;
            }
        }
    }
    Ok(node)
}
fn inherit_worktree(child: &mut Value, parent: &Value) -> Result<(), String> {
    match child["type"].as_str() {
        Some("subgraph") if child.get("preset").is_some() => {
            return Err("Built-in presets do not support inherited worktree isolation".into());
        }
        Some("subgraph") if child.get("from").is_some() => (),
        Some("subgraph" | "loop_subgraph") => {
            for node in child["nodes"].as_array_mut().ok_or("Missing child nodes")? {
                inherit_worktree(node, parent)?;
            }
            return Ok(());
        }
        Some("gate") => return Ok(()),
        Some("shell" | "agent" | "splitter") => (),
        _ => return Err("Unknown isolated splitter child type".into()),
    }
    child["isolation"] = json!("worktree");
    if child.get("worktree").is_none() {
        if let Some(options) = parent.get("worktree") {
            child["worktree"] = options.clone();
        }
    }
    Ok(())
}

fn reducing_splitter(
    id: String,
    items: SplitterItemsFn,
    template: Value,
    base: PathBuf,
    spec: Reduction,
    scope: BuildScope,
) -> SplitterNode {
    // Core enumerates items and builds their nodes synchronously on one thread, then schedules
    // execution. Give every expansion (including a concurrent plan) its own indexed buffer.
    let builders = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let pending = builders.clone();
    let config = spec.clone();
    let parent = scope.clone();
    let items_fn = Arc::new(move |ctx: &Context| {
        let items = items(ctx)?;
        if items.is_empty() {
            parent.publish(ctx, &config.to_ctx, apply_reducer(config.op, &[])?);
        } else {
            let slots = Arc::new(Mutex::new(vec![None; items.len()]));
            pending
                .lock()
                .unwrap()
                .insert(std::thread::current().id(), (0usize, items.len(), slots));
        }
        Ok(items)
    });
    let builder = Arc::new(move |item: &Value, ctx: &Context| {
        let thread = std::thread::current().id();
        let (index, slots) = {
            let mut pending = builders.lock().unwrap();
            let (index, count, slots) = pending
                .get_mut(&thread)
                .ok_or("Missing reducer expansion")?;
            let current = (*index, slots.clone());
            *index += 1;
            if index == count {
                pending.remove(&thread);
            }
            current
        };
        let build = || {
            let effective = scope.context(ctx);
            let mut spec = spec.clone();
            spec.from_output = spec
                .from_output
                .map(|p| {
                    interpolate(&p.to_string_lossy(), &effective, Some(item)).map(PathBuf::from)
                })
                .transpose()?;
            spec.from_ctx_key = spec
                .from_ctx_key
                .map(|s| interpolate(&s, &effective, Some(item)))
                .transpose()?;
            let local = scope
                .local
                .as_ref()
                .map_or_else(Context::new, Context::shallow_clone);
            // Snapshot an initial context source before any sibling starts publishing to it.
            if let Some(key) = &spec.from_ctx_key {
                if let Some(val) = scope.context(ctx).get(key) {
                    local.insert(key, val);
                }
            }
            let mut branch_scope = BuildScope {
                local: Some(local),
                ..scope.clone()
            };
            let slot = ReductionSlot {
                parent: scope.clone(),
                spec,
                index,
                slots,
            };
            if matches!(template["type"].as_str(), Some("shell" | "agent" | "gate")) {
                return build_node(&template, &base, ctx, Some(item), &branch_scope, Some(slot));
            }
            // A container has no verification hook. Capture ephemeral source files before
            // worktree cleanup, then schedule collection after the entire child DAG.
            fn all_isolated(template: &Value) -> bool {
                match template["type"].as_str() {
                    Some("subgraph") if template.get("from").is_some() => {
                        template["isolation"] == "worktree"
                    }
                    Some("subgraph" | "loop_subgraph") => template["nodes"]
                        .as_array()
                        .is_some_and(|nodes| nodes.iter().all(all_isolated)),
                    Some("splitter") => {
                        template["isolation"] == "worktree" || all_isolated(&template["template"])
                    }
                    _ => template["isolation"] == "worktree",
                }
            }
            let isolated = all_isolated(&template);
            let capture = slot.spec.from_output.as_ref().map(|path| FileCapture {
                path: path.clone(),
                op: slot.spec.op,
                value: Default::default(),
            });
            branch_scope.captures.extend(capture.clone());
            let mut child = build_node(&template, &base, ctx, Some(item), &branch_scope, None)?;
            let id = child.id().to_owned();
            let dir = base.clone();
            let collect_id = format!("{id}-reduce");
            let mut collect = TaskNode::new(
                collect_id,
                TaskKind::Code,
                Arc::new(move |ctx, _| {
                    let captured = capture
                        .as_ref()
                        .and_then(|c| c.value.lock().unwrap().clone());
                    let value = match captured {
                        Some(value) => Ok(value),
                        None if isolated && capture.is_some() => {
                            Err("Missing reducer output in isolated worktree".into())
                        }
                        None => slot.value(&dir, branch_scope.local.as_ref().unwrap_or(ctx), None),
                    }
                    .map_err(TaskRunError::other)?;
                    slot.prepare(value)
                        .map_err(TaskRunError::other)?
                        .publish(ctx);
                    Ok(Value::Null)
                }),
                Arc::new(|_| Ok(String::new())),
            );
            collect.deps.push(id.clone());
            collect.cacheable = false;
            let deps = match &mut child {
                Node::Subgraph(n) => std::mem::take(&mut n.deps),
                Node::LoopSubgraph(n) => std::mem::take(&mut n.deps),
                Node::Splitter(n) => std::mem::take(&mut n.deps),
                Node::Task(_) => unreachable!(),
            };
            let mut branch = SubgraphNode::new(id, vec![child, Node::Task(collect)]);
            branch.deps = deps;
            Ok(Node::Subgraph(branch))
        };
        let result = build();
        if result.is_err() {
            builders.lock().unwrap().remove(&thread);
        }
        result
    });
    SplitterNode::new(id, items_fn, builder)
}

fn build_nodes(
    raw: &Value,
    base: &Path,
    ctx: &Context,
    item: Option<&Value>,
    scope: &BuildScope,
) -> Result<Vec<Node>, String> {
    raw.as_array()
        .ok_or("nodes must be an array")?
        .iter()
        .map(|v| build_node(v, base, ctx, item, scope, None))
        .collect()
}

impl Spec<'_> {
    fn fix_config(&self) -> Result<DesktopBugFixConfig, String> {
        let default = DesktopBugFixConfig::default();
        Ok(DesktopBugFixConfig {
            work_dir: self.path("work_dir")?,
            repo_dir: self
                .optional_path("repo_dir")?
                .unwrap_or_else(|| self.base.into()),
            repro_cmd: self.command("repro_cmd")?,
            build_cmd: self.command("build_cmd")?,
            test_cmd: self.command("test_cmd")?,
            watch_globs: self.strings("watch_globs")?,
            resource: self.or("resource", default.resource)?,
            timeout_s: self.or("timeout_s", default.timeout_s)?,
            max_retries: self.or("max_retries", default.max_retries)?,
            tester: self.or("tester", None)?,
            ..default
        })
    }
    fn preset(&self, kind: &str) -> Result<Vec<Node>, String> {
        match kind {
            "desktop_bug_fix" => {
                let cfg = DesktopBugFixConfig {
                    bug_spec: self.get("bug_spec")?,
                    deps: self.strings("deps")?,
                    node_id: self.or("node_id", None)?,
                    review_feedback: self.or("review_feedback", None)?,
                    ..self.fix_config()?
                };
                Ok(vec![Node::LoopSubgraph(create_desktop_bug_fix_subgraph(
                    cfg,
                )?)])
            }
            "bug_batch" => {
                let fix = self.fix_config()?;
                create_bug_batch_graph(BugBatchConfig {
                    bugs: self.get("bugs")?,
                    work_dir: fix.work_dir,
                    repo_dir: fix.repo_dir,
                    repro_cmd: fix.repro_cmd,
                    build_cmd: fix.build_cmd,
                    test_cmd: fix.test_cmd,
                    watch_globs: fix.watch_globs,
                    resource: fix.resource,
                    timeout_s: fix.timeout_s,
                    max_retries: fix.max_retries,
                    batch_id: self.or("batch_id", None)?,
                    tester: fix.tester,
                    verify_fn: None,
                })
            }
            "game_regression" => {
                let mut visuals = vec![];
                for v in self.or::<Vec<Value>>("visual_checks", vec![])? {
                    if let Value::String(id) = v {
                        visuals.push(VisualCheckConfig {
                            id,
                            ..Default::default()
                        });
                        continue;
                    }
                    let s = Spec {
                        base: self.base,
                        value: &v,
                    };
                    visuals.push(VisualCheckConfig {
                        id: s.get("id")?,
                        path: s.or("path", None)?,
                        min_width: s.or("min_width", None)?,
                        min_height: s.or("min_height", None)?,
                    });
                }
                let default = GameRegressionConfig::default();
                create_game_regression_graph(GameRegressionConfig {
                    suite_id: self.get("suite_id")?,
                    work_dir: self.path("work_dir")?,
                    repo_dir: self
                        .optional_path("repo_dir")?
                        .unwrap_or_else(|| self.base.into()),
                    build_cmd: self.command("build_cmd")?,
                    test_cmd: self.required_command("test_cmd")?,
                    watch_globs: self.strings("watch_globs")?,
                    visual_checks: visuals,
                    min_width: self.or("min_width", default.min_width)?,
                    min_height: self.or("min_height", default.min_height)?,
                    timeout_s: self.or("timeout_s", default.timeout_s)?,
                    deps: self.strings("deps")?,
                    tester: self.or("tester", None)?,
                })
            }
            "version_history" => {
                let default = VersionHistoryConfig::default();
                let cfg = VersionHistoryConfig {
                    work_dir: self.path("work_dir")?,
                    product: self.or("product", default.product)?,
                    release_id: self.or("release_id", None)?,
                    changes_manifest_path: self.optional_path("changes_manifest_path")?,
                    media_dir: self.optional_path("media_dir")?,
                    builder_script: self.optional_path("builder_script")?,
                    max_retries: self.or("max_retries", default.max_retries)?,
                    deps: self.strings("deps")?,
                    node_id: self.or("node_id", default.node_id)?,
                    source_paths: self.paths("source_paths")?,
                };
                Ok(vec![Node::LoopSubgraph(create_version_history_subgraph(
                    cfg,
                )?)])
            }
            "histoury_bug_fix" => {
                let default = HistouryBugFixConfig::default();
                let repos = self
                    .get::<BTreeMap<String, PathBuf>>("repos")?
                    .into_iter()
                    .map(|(name, p)| (name, self.base.join(p)))
                    .collect();
                create_histoury_bug_fix_graph(HistouryBugFixConfig {
                    batch_id: self.get("batch_id")?,
                    work_dir: self.path("work_dir")?,
                    repos,
                    product: self.or("product", default.product)?,
                    bugs: self.or("bugs", None)?,
                    build_cmds: self.commands("build_cmds")?,
                    test_cmds: self.commands("test_cmds")?,
                    watch_globs: self.strings("watch_globs")?,
                    builder_script: self.optional_path("builder_script")?,
                    bug_reports_url: self.or("bug_reports_url", default.bug_reports_url)?,
                    version_history_url: self
                        .or("version_history_url", default.version_history_url)?,
                    max_fix_retries: self.or("max_fix_retries", default.max_fix_retries)?,
                    max_package_retries: self
                        .or("max_package_retries", default.max_package_retries)?,
                    max_review_retries: self
                        .or("max_review_retries", default.max_review_retries)?,
                    timeout_s: self.or("timeout_s", default.timeout_s)?,
                    reviewer: self.or("reviewer", None)?,
                    tester: self.or("tester", None)?,
                    verify_fns: BTreeMap::new(),
                })
            }
            _ => Err(format!("Unknown workflow preset {kind:?}")),
        }
    }
}

fn validate_preset_document(fields: &Map<String, Value>) -> Result<(), String> {
    validate_preset_commands(fields)?;
    if let Some(config) = fields.get("config").and_then(Value::as_object) {
        validate_preset_commands(config)?;
    }
    Ok(())
}

fn preset_document(name: &str, base: &Path, rendered: &Value) -> Result<Vec<Node>, String> {
    let mut fields = rendered.as_object().unwrap().clone();
    if let Some(config) = fields.remove("config") {
        fields.extend(
            config
                .as_object()
                .ok_or("config must be an object")?
                .clone(),
        );
    }
    Spec {
        base,
        value: &Value::Object(fields),
    }
    .preset(name)
}

fn parse_document(path: &Path, raw: &str) -> Result<Value, String> {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => serde_json::from_str(raw).map_err(|e| format!("{}: {e}", path.display())),
        Some("toml") => toml::from_str(raw).map_err(|e| format!("{}: {e}", path.display())),
        _ => Err(format!(
            "Expected a TOML or JSON workflow: {}",
            path.display()
        )),
    }
}

pub fn load_declarative_workflow(
    path: &Path,
    ctx_inline: Option<&str>,
    ctx_file: Option<&Path>,
) -> Result<LoadedWorkflow, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc = parse_document(path, &raw)?;
    let base = resolve_path(path.parent().unwrap_or(Path::new(".")))?;
    let mut fields = doc.as_object().ok_or("Workflow must be an object")?.clone();
    let default = fields
        .remove("default_context")
        .unwrap_or_else(|| json!({}));
    let mut values = default
        .as_object()
        .ok_or("default_context must be an object")?
        .clone();
    if let Some(file) = ctx_file {
        values.extend(read_object(file)?)
    }
    if let Some(raw) = ctx_inline {
        values.extend(
            serde_json::from_str::<Map<String, Value>>(raw)
                .map_err(|e| format!("Inline context must be a JSON object: {e}"))?,
        )
    }
    let context = Context::from_map(values);
    let explicit = fields.remove("nodes");
    let finally_raw = fields.remove("finally");
    let preset = fields.remove("preset");
    // Inspect command sources before rendering erases the boundary between code and data.
    if let Some(nodes) = explicit.as_ref().and_then(Value::as_array) {
        for node in nodes {
            validate_node_commands(node)?;
        }
    }
    if let Some(raw_finally) = finally_raw.as_ref() {
        let arr = raw_finally
            .as_array()
            .ok_or("finally must be an array of nodes")?;
        for node in arr {
            validate_node_commands(node)?;
        }
    }
    if preset.is_some() {
        validate_preset_document(&fields)?;
    }
    let rendered = render(&Value::Object(fields), &context, None)?;
    let spec = Spec {
        base: &base,
        value: &rendered,
    };
    let budgets: WorkflowBudgets = spec.or("budgets", WorkflowBudgets::default())?;
    if let Some(c) = budgets.max_cost_usd {
        if !c.is_finite() || c <= 0.0 {
            return Err("budgets.max_cost_usd must be a positive finite number".into());
        }
    }
    if let Some(w) = budgets.max_wall_time_s {
        if !w.is_finite() || w <= 0.0 {
            return Err("budgets.max_wall_time_s must be a positive finite number".into());
        }
    }
    let nodes = match (explicit, preset) {
        (Some(raw), None) => build_nodes(
            &raw,
            &base,
            &context,
            None,
            &BuildScope::workflow(&raw, &base, path)?,
        )?,
        (None, Some(Value::String(name))) => preset_document(&name, &base, &rendered)?,
        _ => return Err("Specify either a nodes array or a named preset".into()),
    };
    let finally_nodes = match finally_raw {
        Some(raw) => build_nodes(
            &raw,
            &base,
            &context,
            None,
            &BuildScope::workflow(&raw, &base, path)?,
        )?,
        None => Vec::new(),
    };
    let qa_checklist_path = spec
        .optional_path("qa_checklist_path")?
        .or_else(|| graph_qa_checklist(&nodes));
    Ok(LoadedWorkflow {
        title: spec.or("title", None)?,
        nodes,
        finally_nodes,
        budgets,
        context,
        qa_checklist_path,
    })
}

// ---------------------------------------------------------------------------
// Safe deterministic `when` expression parser and evaluator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum WhenExpr {
    Or(Box<WhenExpr>, Box<WhenExpr>),
    And(Box<WhenExpr>, Box<WhenExpr>),
    Not(Box<WhenExpr>),
    Eq(WhenOperand, WhenOperand),
    Ne(WhenOperand, WhenOperand),
    In(WhenOperand, WhenOperand),
    Truthy(WhenOperand),
}

#[derive(Debug, Clone)]
enum WhenOperand {
    Path(String),
    Literal(Value),
    List(Vec<Value>),
}

fn split_top_level<'a>(input: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let bytes = input.as_bytes();
    let op_bytes = op.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i + op_bytes.len() <= bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => quote = Some(b),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            _ if depth == 0 && &bytes[i..i + op_bytes.len()] == op_bytes => {
                // Ensure word operators like ` in ` are bounded by whitespace.
                if op == " in " {
                    return Some((&input[..i], &input[i + op_bytes.len()..]));
                }
                return Some((&input[..i], &input[i + op_bytes.len()..]));
            }
            _ => (),
        }
        i += 1;
    }
    None
}

fn strip_outer_parens(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim();
        if !trimmed.starts_with('(') || !trimmed.ends_with(')') {
            return trimmed;
        }
        let bytes = trimmed.as_bytes();
        let mut depth = 0usize;
        let mut quote: Option<u8> = None;
        let mut encloses_all = true;
        for (idx, &b) in bytes.iter().enumerate() {
            if let Some(q) = quote {
                if b == q && bytes.get(idx.wrapping_sub(1)) != Some(&b'\\') {
                    quote = None;
                }
                continue;
            }
            match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && idx + 1 < bytes.len() {
                        encloses_all = false;
                        break;
                    }
                }
                _ => (),
            }
        }
        if encloses_all && depth == 0 {
            s = &trimmed[1..trimmed.len() - 1];
        } else {
            return trimmed;
        }
    }
}

fn parse_when_literal(raw: &str) -> Result<Value, String> {
    let t = raw.trim();
    if (t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"')) {
        if t.len() < 2 {
            return Err(format!("Unterminated string literal in when expression: {raw}"));
        }
        return Ok(Value::String(
            t[1..t.len() - 1]
                .replace("\\'", "'")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\"),
        ));
    }
    match t {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        _ => {
            if let Ok(i) = t.parse::<i64>() {
                Ok(json!(i))
            } else if let Ok(f) = t.parse::<f64>() {
                Ok(json!(f))
            } else {
                Err(format!("Invalid literal in when expression: {raw}"))
            }
        }
    }
}

fn parse_when_operand(raw: &str) -> Result<WhenOperand, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("Empty operand in when expression".into());
    }
    if t.starts_with('[') && t.ends_with(']') {
        let inner = t[1..t.len() - 1].trim();
        if inner.is_empty() {
            return Ok(WhenOperand::List(Vec::new()));
        }
        let mut items = Vec::new();
        for part in inner.split(',') {
            items.push(parse_when_literal(part)?);
        }
        return Ok(WhenOperand::List(items));
    }
    if t.starts_with("ctx.")
        || t == "item"
        || t.starts_with("item.")
        || t.starts_with("outputs.")
    {
        return Ok(WhenOperand::Path(t.to_owned()));
    }
    Ok(WhenOperand::Literal(parse_when_literal(t)?))
}

fn parse_when_expr(raw: &str) -> Result<WhenExpr, String> {
    let t = strip_outer_parens(raw);
    if t.is_empty() {
        return Err("when expression cannot be empty".into());
    }
    if let Some((l, r)) = split_top_level(t, "||") {
        return Ok(WhenExpr::Or(
            Box::new(parse_when_expr(l)?),
            Box::new(parse_when_expr(r)?),
        ));
    }
    if let Some((l, r)) = split_top_level(t, "&&") {
        return Ok(WhenExpr::And(
            Box::new(parse_when_expr(l)?),
            Box::new(parse_when_expr(r)?),
        ));
    }
    if let Some(rest) = t.strip_prefix('!') {
        return Ok(WhenExpr::Not(Box::new(parse_when_expr(rest)?)));
    }
    if let Some((l, r)) = split_top_level(t, "==") {
        return Ok(WhenExpr::Eq(parse_when_operand(l)?, parse_when_operand(r)?));
    }
    if let Some((l, r)) = split_top_level(t, "!=") {
        return Ok(WhenExpr::Ne(parse_when_operand(l)?, parse_when_operand(r)?));
    }
    if let Some((l, r)) = split_top_level(t, " in ") {
        return Ok(WhenExpr::In(parse_when_operand(l)?, parse_when_operand(r)?));
    }
    Ok(WhenExpr::Truthy(parse_when_operand(t)?))
}

fn lookup_dotted(mut cur: &Value, rest: &str) -> Value {
    for seg in rest.split('.') {
        match cur {
            Value::Object(map) => match map.get(seg) {
                Some(v) => cur = v,
                None => return Value::Null,
            },
            Value::Array(arr) => match seg.parse::<usize>().ok().and_then(|i| arr.get(i)) {
                Some(v) => cur = v,
                None => return Value::Null,
            },
            _ => return Value::Null,
        }
    }
    cur.clone()
}

fn resolve_when_operand(
    op: &WhenOperand,
    ctx: &Context,
    item: Option<&Value>,
    scope: &BuildScope,
) -> Result<Value, String> {
    match op {
        WhenOperand::Literal(v) => Ok(v.clone()),
        WhenOperand::List(list) => Ok(Value::Array(list.clone())),
        WhenOperand::Path(path) => {
            if let Some(rest) = path.strip_prefix("ctx.") {
                let mut parts = rest.splitn(2, '.');
                let head = parts.next().unwrap_or("");
                if let Some(val) = ctx.get(head) {
                    return Ok(match parts.next() {
                        Some(tail) => lookup_dotted(&val, tail),
                        None => val,
                    });
                }
                if scope.runtime_key(head) {
                    return Err(format!("upstream context key '{head}' not yet produced"));
                }
                Ok(Value::Null)
            } else if let Some(rest) = path.strip_prefix("outputs.") {
                // Support both ctx.outputs.<node> and top-level exported ctx.<node>.
                if let Some(outputs_val) = ctx.get("outputs") {
                    let found = lookup_dotted(&outputs_val, rest);
                    if !found.is_null() {
                        return Ok(found);
                    }
                }
                let mut parts = rest.splitn(2, '.');
                let head = parts.next().unwrap_or("");
                if let Some(val) = ctx.get(head) {
                    return Ok(match parts.next() {
                        Some(tail) => lookup_dotted(&val, tail),
                        None => val,
                    });
                }
                if scope.runtime_key(head) {
                    return Err(format!("upstream output '{head}' not yet produced"));
                }
                Ok(Value::Null)
            } else if path == "item" {
                Ok(item.cloned().unwrap_or(Value::Null))
            } else if let Some(rest) = path.strip_prefix("item.") {
                Ok(item.map_or(Value::Null, |v| lookup_dotted(v, rest)))
            } else {
                Ok(Value::Null)
            }
        }
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn eval_when_ast(
    expr: &WhenExpr,
    ctx: &Context,
    item: Option<&Value>,
    scope: &BuildScope,
) -> Result<bool, String> {
    match expr {
        WhenExpr::Or(l, r) => Ok(eval_when_ast(l, ctx, item, scope)? || eval_when_ast(r, ctx, item, scope)?),
        WhenExpr::And(l, r) => Ok(eval_when_ast(l, ctx, item, scope)? && eval_when_ast(r, ctx, item, scope)?),
        WhenExpr::Not(inner) => Ok(!eval_when_ast(inner, ctx, item, scope)?),
        WhenExpr::Truthy(op) => Ok(is_truthy(&resolve_when_operand(op, ctx, item, scope)?)),
        WhenExpr::Eq(l, r) => Ok(resolve_when_operand(l, ctx, item, scope)?
            == resolve_when_operand(r, ctx, item, scope)?),
        WhenExpr::Ne(l, r) => Ok(resolve_when_operand(l, ctx, item, scope)?
            != resolve_when_operand(r, ctx, item, scope)?),
        WhenExpr::In(needle, haystack) => {
            let n = resolve_when_operand(needle, ctx, item, scope)?;
            let h = resolve_when_operand(haystack, ctx, item, scope)?;
            Ok(match h {
                Value::Array(arr) => arr.contains(&n),
                Value::String(s) => n.as_str().is_some_and(|sub| s.contains(sub)),
                Value::Object(map) => n.as_str().is_some_and(|k| map.contains_key(k)),
                _ => false,
            })
        }
    }
}

fn eval_when_expr(
    raw: &str,
    ctx: &Context,
    item: Option<&Value>,
    scope: &BuildScope,
) -> Result<bool, String> {
    let ast = parse_when_expr(raw)?;
    eval_when_ast(&ast, ctx, item, scope)
}

/// Return a Draft 2020-12 JSON Schema describing declarative TOML/JSON KGraph workflows.
pub fn workflow_json_schema() -> Value {
    let mut node_props = Map::new();
    for (k, v) in [
        ("id", json!({ "type": "string", "pattern": "^[A-Za-z0-9_][A-Za-z0-9_-]*$" })),
        ("type", json!({ "type": "string", "enum": ["shell", "agent", "gate", "subgraph", "loop_subgraph", "splitter"] })),
        ("deps", json!({ "type": "array", "items": { "type": "string" } })),
        ("resource", json!({ "type": "string" })),
        ("when", json!({ "type": "string", "description": "Safe boolean expression over ctx.*, outputs.*, and item.* (supports !, ==, !=, in, &&, ||)." })),
        ("cacheable", json!({ "type": "boolean", "default": true })),
        ("cwd", json!({ "type": "string" })),
        ("env", json!({ "type": "object", "additionalProperties": { "type": "string" } })),
        ("command", json!({ "oneOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }] })),
        ("outputs", json!({ "type": "array", "items": { "type": "string" } })),
        ("watch_paths", json!({ "type": "array", "items": { "type": "string" } })),
        ("watch_globs", json!({ "type": "array", "items": { "type": "string" } })),
        ("log_path", json!({ "type": "string" })),
        ("expect_stdout_regex", json!({ "type": "string" })),
        ("timeout_s", json!({ "type": "number", "exclusiveMinimum": 0 })),
        ("max_retries", json!({ "type": "integer", "minimum": 0 })),
        ("retry_backoff_s", json!({ "type": "number", "minimum": 0, "description": "Initial exponential backoff delay in seconds before retrying." })),
        ("retry_max_backoff_s", json!({ "type": "number", "exclusiveMinimum": 0, "description": "Maximum backoff cap in seconds (default 60)." })),
        ("retry_jitter", json!({ "type": "boolean", "default": true })),
        ("transient_retries", json!({ "type": "integer", "minimum": 0, "default": 0, "description": "Extra retries for transient 429/529/timeout failures without consuming max_retries." })),
        ("transient_error_regex", json!({ "type": "string", "description": "Optional custom regex matching transient errors." })),
        ("oscillation_breaker", json!({ "type": "boolean", "default": true, "description": "Trip circuit breaker on ping-pong oscillation cycles (e.g. A -> B -> A)." })),
        ("immutable_paths", json!({ "type": "array", "items": { "type": "string" }, "description": "Files/directories that must remain unmodified during node/loop execution." })),
        ("protect_globs", json!({ "type": "array", "items": { "type": "string" }, "description": "Relative globs whose SHA-256 fingerprint must remain unchanged (anti-reward-hijacking)." })),
        ("max_feedback_bytes", json!({ "type": "integer", "minimum": 1, "description": "Byte budget for compacted retry feedback injected into agent prompts." })),
        ("adapter", json!({ "type": "object" })),
        ("prompt", json!({ "type": "string" })),
        ("prompt_file", json!({ "type": "string" })),
        ("evaluator", json!({ "type": "object" })),
        ("isolation", json!({ "type": "string", "enum": ["worktree"] })),
        ("worktree", json!({ "type": "object" })),
        ("export_json", json!({ "type": "object" })),
        ("output_schema", json!({ "type": "object" })),
        ("verify_cmd", json!({ "oneOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }] })),
        ("verify_json", json!({ "type": "object" })),
        ("verify_json_path", json!({ "type": "string" })),
        ("dirty_nodes", json!({ "type": "array", "items": { "type": "string" } })),
        ("packet_path", json!({ "type": "string" })),
        ("packet", json!({ "type": "object" })),
        ("artifact_paths", json!({ "type": "array", "items": { "type": "string" } })),
        ("from", json!({ "type": "string", "description": "Relative path or @lib/<name> reusable subgraph reference." })),
        ("with", json!({ "type": "object" })),
        ("preset", json!({ "type": "string" })),
        ("config", json!({ "type": "object" })),
        ("nodes", json!({ "type": "array", "items": { "$ref": "#/$defs/node" } })),
        ("items", json!({ "type": "array" })),
        ("items_from_ctx", json!({ "type": "string" })),
        ("max_workers", json!({ "type": "integer", "minimum": 1 })),
        ("template", json!({ "$ref": "#/$defs/node" })),
        ("reduce", json!({ "type": "object" })),
    ] {
        node_props.insert(k.into(), v);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://github.com/dawsv/kgraph-runner/schema/workflow.v0.4.json",
        "title": "KGraph Runner Declarative Workflow",
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Human-readable workflow title shown in Hero Town and reports." },
            "default_context": { "type": "object", "description": "Default key/value context available as {{ctx.key}} and in when expressions." },
            "qa_checklist_path": { "type": "string", "description": "Optional path to a manual QA checklist JSON file." },
            "budgets": {
                "type": "object",
                "description": "Hard run-level budget guardrails enforced before node dispatches and retries.",
                "properties": {
                    "max_worker_calls": { "type": "integer", "minimum": 1 },
                    "max_cost_usd": { "type": "number", "exclusiveMinimum": 0 },
                    "max_tokens": { "type": "integer", "minimum": 1 },
                    "max_wall_time_s": { "type": "number", "exclusiveMinimum": 0 }
                },
                "additionalProperties": false
            },
            "nodes": {
                "type": "array",
                "description": "Top-level DAG nodes executed in dependency order.",
                "items": { "$ref": "#/$defs/node" }
            },
            "finally": {
                "type": "array",
                "description": "Cleanup or escalation nodes executed unconditionally after the main graph completes (receives ctx.__kgraph_run_status and ctx.__kgraph_failure).",
                "items": { "$ref": "#/$defs/node" }
            },
            "preset": {
                "type": "string",
                "enum": ["desktop_bug_fix", "bug_batch", "game_regression", "version_history", "histoury_bug_fix"]
            },
            "config": { "type": "object" }
        },
        "$defs": {
            "node": {
                "type": "object",
                "required": ["id", "type"],
                "properties": Value::Object(node_props)
            }
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::engine::{GraphRunner, RunnerConfig};
    use kgraph_core::files::write_json;
    use tempfile::TempDir;

    fn runner(dir: &Path) -> GraphRunner {
        let mut cfg = RunnerConfig::new(dir.join("state.json"));
        cfg.logger = Some(Arc::new(|_| {}));
        GraphRunner::new(cfg).unwrap()
    }
    fn json_workflow(dir: &Path, doc: Value) -> Result<LoadedWorkflow, String> {
        let path = dir.join("flow.json");
        write_json(&path, &doc, true)?;
        load_declarative_workflow(&path, None, None)
    }

    fn import_node(from: &str) -> Value {
        json!({"type":"subgraph", "id":"stage", "from":from})
    }

    #[test]
    fn parallel_imports_keep_parameters_private_and_inherit_missing_values() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("check.toml"), r#"
            [default_context]
            target = "default"
            flavor = "vanilla"
            inherited = "wrong"
            [[nodes]]
            id = "check"
            type = "shell"
            env = { TARGET = "{{ctx.target}}", PEER = "{{ctx.peer}}", TEXT = "{{ctx.inherited}}/{{ctx.flavor}}/{{ctx.target}}" }
            outputs = ["{{ctx.target}}.txt"]
            command = ["python3", "-c", """
from pathlib import Path
from os import environ
from time import monotonic, sleep
Path(environ['TARGET'] + '.ready').touch()
deadline = monotonic() + 5
while not Path(environ['PEER'] + '.ready').exists():
    if monotonic() > deadline:
        raise RuntimeError('peer did not run concurrently')
    sleep(0.02)
Path(environ['TARGET'] + '.txt').write_text(environ['TEXT'])
"""]
        "#).unwrap();
        let flow = json_workflow(dir.path(), json!({
            "default_context":{"target":"parent","inherited":"workflow","second":"b"},
            "nodes":[
                {"type":"subgraph","id":"first","from":"check.toml","with":{"target":"a","peer":"b"}},
                {"type":"subgraph","id":"second","from":"check.toml","with":{"target":"{{ctx.second}}","peer":"a","flavor":"mint"}}
            ]
        })).unwrap();
        let initial = flow.context.snapshot_map();
        let mut cfg = RunnerConfig::new(dir.path().join("state.json"));
        cfg.max_parallel_nodes = 2;
        GraphRunner::new(cfg)
            .unwrap()
            .run(&flow.nodes, &flow.context, "")
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "workflow/vanilla/a"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "workflow/mint/b"
        );
        assert_eq!(flow.context.snapshot_map(), initial);
    }

    #[test]
    fn shared_library_imports_keep_nested_sources_and_execution_paths_separate() {
        // Run the fixture in its own process so concurrent tests keep their own KGRAPH_HOME.
        const TEST: &str = "declarative::tests::shared_library_imports_keep_nested_sources_and_execution_paths_separate";
        if std::env::var_os("KGRAPH_TEST_IMPORT_FIXTURE").is_none() {
            let home = TempDir::new().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("KGRAPH_TEST_IMPORT_FIXTURE", "1")
                .env("KGRAPH_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            return;
        }
        let dir = TempDir::new().unwrap();
        let library = library_root().unwrap();
        let ci = library.join("ci");
        fs::create_dir_all(&ci).unwrap();
        fs::write(
            ci.join("check.toml"),
            r#"[[nodes]]
id = "nested"
type = "subgraph"
from = "write.json"
"#,
        )
        .unwrap();
        write_json(&ci.join("write.json"), &json!({"nodes":[{
            "type":"shell","id":"write","command":["python3","-c","from pathlib import Path; Path('result.txt').write_text(Path('input.txt').read_text())"],
            "watch_paths":["input.txt"], "outputs":["result.txt"], "log_path":"check.log"
        }]}), true).unwrap();
        fs::write(ci.join("check.json"), "invalid").unwrap();
        fs::write(dir.path().join("input.txt"), "project").unwrap();
        let flow =
            json_workflow(dir.path(), json!({"nodes":[import_node("@lib/ci/check")]})).unwrap();
        runner(dir.path())
            .run(&flow.nodes, &flow.context, "")
            .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("result.txt")).unwrap(),
            "project"
        );
        assert!(dir.path().join("check.log").is_file());
        assert!(!ci.join("result.txt").exists());
        for name in ["@lib/ci/write", "@lib/ci/write.json"] {
            json_workflow(dir.path(), json!({"nodes":[import_node(name)]})).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), ci.join("external")).unwrap();
            let err = json_workflow(
                dir.path(),
                json!({"nodes":[import_node("@lib/ci/external/flow.json")]}),
            )
            .err()
            .unwrap();
            assert!(err.contains("must stay inside"), "{err}");
        }
    }

    #[test]
    fn nested_presets_plan_and_run_with_stage_dependencies() {
        let dir = TempDir::new().unwrap();
        let flow = json_workflow(dir.path(), json!({"default_context":{"bug":"login"},"nodes":[
            {"id":"smoke","type":"subgraph","preset":"game_regression","deps":["fix"],"resource":"qa",
             "config":{"work_dir":"suite","suite_id":"{{ctx.bug}}","test_cmd":["python3","--version"]}},
            {"id":"fix","type":"subgraph","preset":"desktop_bug_fix",
             "config":{"work_dir":"fixes","bug_spec":{"id":"{{ctx.bug}}","title":"Login"},"test_cmd":["python3","--version"]}}
        ]})).unwrap();
        let runner = runner(dir.path());
        let plan = runner.plan(&flow.nodes, &flow.context, "").unwrap();
        let stage = plan.iter().find(|entry| entry.path == "smoke").unwrap();
        assert_eq!(stage.deps, ["fix"]);
        assert_eq!(stage.resource.as_deref(), Some("qa"));
        assert!(plan.iter().any(|entry| entry.path == "fix/login/apply_fix"));
        assert!(plan
            .iter()
            .any(|entry| entry.path.starts_with("smoke/login/")));
        let err = runner.run(&flow.nodes, &flow.context, "").unwrap_err();
        assert!(
            err.awaiting_subagents
                && err.node_path == "apply_fix"
                && err
                    .packet_path
                    .as_deref()
                    .is_some_and(|p| Path::new(p).ends_with("fixes/bugs/login/fix_packet.json")),
            "{err:?}"
        );
        assert!(!dir.path().join("suite/regression_report.json").exists());
        write_json(&dir.path().join("fixes/bugs/login/fix_report.json"),
            &json!({"revision":1,"summary":"Restored login","root_cause":"Defaults","changed_files":["login"]}), true).unwrap();
        runner.run(&flow.nodes, &flow.context, "").unwrap();
        assert_eq!(
            read_object(&dir.path().join("suite/regression_report.json")).unwrap()["status"],
            "passed"
        );
    }

    #[test]
    fn splitter_imports_bind_item_parameters_in_nested_project_files() {
        let dir = TempDir::new().unwrap();
        let folder = dir.path().join("subgraphs");
        fs::create_dir(&folder).unwrap();
        write_json(&folder.join("check.json"), &json!({"nodes":[{
            "type":"splitter","id":"checks","items_from_ctx":"inputs",
            "template":{"type":"subgraph","id":"inner-{{item}}","from":"write.json","with":{"suffix":"{{item}}"}}
        }]}), true).unwrap();
        write_json(&folder.join("write.json"), &json!({"nodes":[{
            "type":"shell","id":"write",
            "command":["python3","-c","from pathlib import Path; import sys; Path(sys.argv[1]).write_text(sys.argv[2])","{{ctx.target}}.txt","{{ctx.suffix}}"],
            "outputs":["{{ctx.target}}.txt"]
        }]}), true).unwrap();
        let flow = json_workflow(
            dir.path(),
            json!({"nodes":[{
                "type":"splitter","id":"jobs","items":[{"name":"one"},{"name":"two"}],
                "template":{"type":"subgraph","id":"{{item.name}}","from":"subgraphs/check.json",
                    "with":{"target":"{{item.name}}","inputs":["{{item.name}}-nested"]}}
            }]}),
        )
        .unwrap();
        runner(dir.path())
            .run(&flow.nodes, &flow.context, "")
            .unwrap();
        for name in ["one", "two"] {
            assert_eq!(
                fs::read_to_string(dir.path().join(format!("{name}.txt"))).unwrap(),
                format!("{name}-nested")
            );
        }
        assert!(flow.context.get("target").is_none());
    }

    #[test]
    fn splitter_can_embed_presets_and_import_preset_documents() {
        let dir = TempDir::new().unwrap();
        write_json(&dir.path().join("suite.json"), &json!({"preset":"game_regression","default_context":{"id":"external"},
            "suite_id":"{{ctx.id}}","config":{"work_dir":"{{ctx.id}}","test_cmd":["python3","--version"]}}), true).unwrap();
        let flow = json_workflow(dir.path(), json!({"nodes":[import_node("suite.json"),{
            "type":"splitter","id":"batch","items":[{"name":"one"},{"name":"two"}],
            "template":{"type":"subgraph","id":"{{item.name}}","preset":"game_regression",
                "config":{"suite_id":"{{item.name}}","work_dir":"{{item.name}}","watch_globs":["flow.json"],"test_cmd":["python3","--version"]}}
        }]})).unwrap();
        runner(dir.path())
            .run(&flow.nodes, &flow.context, "")
            .unwrap();
        for stage in ["one", "two", "external"] {
            assert_eq!(
                read_object(&dir.path().join(stage).join("regression_report.json")).unwrap()
                    ["suite_id"],
                stage
            );
        }
    }

    #[test]
    fn import_errors_cover_sources_escape_bounds_and_command_validation() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        fs::write(
            base.join("large.toml"),
            vec![b' '; MAX_IMPORT_BYTES as usize + 1],
        )
        .unwrap();
        fs::create_dir(base.join("directory.toml")).unwrap();
        fs::write(
            base.join("unsafe.toml"),
            "preset='game_regression'\n[config]\ntest_cmd='echo {{ctx.x}}'",
        )
        .unwrap();
        let template = json!({"type":"subgraph","id":"bad","from":"missing.json","nodes":[]});
        for (node, expected) in [
            (json!({"type":"subgraph","id":"missing"}), "exactly one"),
            (template.clone(), "exactly one"),
            (
                json!({"type":"splitter","id":"s","items":[],"template":template}),
                "exactly one",
            ),
            (
                json!({"type":"subgraph","id":"bad","preset":"game_regression","from":"missing.json"}),
                "exactly one",
            ),
            (
                json!({"type":"subgraph","id":"bad","from":"missing.json","with":[]}),
                "with must be an object",
            ),
            (import_node("../../escape.toml"), "must stay inside"),
            (
                import_node(base.join("flow.json").to_str().unwrap()),
                "relative subgraph import",
            ),
            (import_node("@lib/../escape"), "Invalid @lib"),
            (import_node("@lib//abs"), "Invalid @lib"),
            (import_node("@lib/a\\b"), "Invalid @lib"),
            (import_node("@lib/a\0b"), "Invalid @lib"),
            (import_node("large.toml"), "2 MiB"),
            (import_node("directory.toml"), "regular file"),
            (
                import_node("unsafe.toml"),
                "shell command strings are unsafe",
            ),
            (
                json!({"type":"subgraph","id":"bad","preset":"game_regression","config":{"test_cmd":"echo {{ctx.x}}"}}),
                "shell command strings are unsafe",
            ),
        ] {
            let err = json_workflow(base, json!({"nodes":[node]})).err().unwrap();
            assert!(err.contains(expected), "{node}: {err}");
        }
        #[cfg(unix)]
        {
            let outside = TempDir::new().unwrap();
            fs::write(outside.path().join("other.json"), r#"{"nodes":[]}"#).unwrap();
            std::os::unix::fs::symlink(outside.path(), base.join("link")).unwrap();
            let err = json_workflow(base, json!({"nodes":[import_node("link/other.json")]}))
                .err()
                .unwrap();
            assert!(err.contains("must stay inside"), "{err}");
        }
    }

    #[test]
    fn import_chain_detects_cycles_and_limits_depth_across_splitter_expansion() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        for (source, dest) in [("a", "b"), ("b", "a")] {
            fs::write(
                base.join(format!("{source}.toml")),
                format!("[[nodes]]\nid='child'\ntype='subgraph'\nfrom='{dest}.toml'"),
            )
            .unwrap();
        }
        let doc = json!({"nodes":[import_node("a.toml")]});
        let err = json_workflow(base, doc.clone()).err().unwrap();
        assert!(
            err.contains("Circular subgraph import")
                && err.contains("b.toml")
                && err.matches("a.toml").count() == 2,
            "{err}"
        );
        fs::write(base.join("b.toml"), "[[nodes]]\ntype='splitter'\nid='fan'\nitems=['x']\n[nodes.template]\ntype='subgraph'\nid='x'\nfrom='a.toml'").unwrap();
        let flow = json_workflow(base, doc).unwrap();
        let err = runner(base)
            .run(&flow.nodes, &flow.context, "")
            .unwrap_err();
        assert!(err.reason.contains("Circular subgraph import"), "{err:?}");

        let root = json!({"nodes":[import_node("1.json")]});
        for i in 1..MAX_IMPORT_DEPTH {
            write_json(
                &base.join(format!("{i}.json")),
                &json!({"nodes":[import_node(&format!("{}.json", i+1))]}),
                true,
            )
            .unwrap();
        }
        let eighth = base.join(format!("{MAX_IMPORT_DEPTH}.json"));
        write_json(&eighth, &json!({"nodes":[]}), true).unwrap();
        json_workflow(base, root.clone()).unwrap();
        write_json(&eighth, &json!({"nodes":[import_node("extra.toml")]}), true).unwrap();
        let err = json_workflow(base, root).err().unwrap();
        assert!(
            err.contains("Subgraph import nesting exceeds limit"),
            "{err}"
        );
    }

    #[test]
    fn toml_context_precedence_paths_shell_metadata_and_watch_invalidation() {
        let root = TempDir::new().unwrap();
        let base = root.path().canonicalize().unwrap();
        fs::create_dir(base.join("job")).unwrap();
        fs::write(base.join("watch.txt"), "first").unwrap();
        let path = base.join("workflow.toml");
        fs::write(&path,r#"
title = "Run {{ctx.who}}"
qa_checklist_path = "results/manual_qa.json"
[default_context]
who = "default"
file_only = "overridden"
keep = "yes"
[[nodes]]
id = "build"
type = "shell"
command = ["python3","-c","import os; from pathlib import Path; print(os.environ['GREETING']); Path('delivery.txt').write_text(os.environ['GREETING'])"]
cwd = "job"
outputs = ["job/delivery.txt"]
env = { GREETING = "{{ctx.who}} {{ctx.file_only}} {{ctx.keep}}" }
expect_stdout_regex = "^inline file yes"
watch_globs = ["*.txt"]
watch_paths = ["input"]
log_path = "logs/build.log"
resource = "builder"
kind = "verifier"
timeout_s = 10
max_retries = 1
"#).unwrap();
        let file = base.join("context.json");
        write_json(&file, &json!({"who":"file","file_only":"file"}), true).unwrap();
        let flow =
            load_declarative_workflow(&path, Some(r#"{"who":"inline"}"#), Some(&file)).unwrap();
        assert_eq!(flow.title.as_deref(), Some("Run inline"));
        assert_eq!(
            flow.qa_checklist_path,
            Some(base.join("results/manual_qa.json"))
        );
        let Node::Task(task) = &flow.nodes[0] else {
            panic!()
        };
        assert_eq!(task.kind, TaskKind::Verifier);
        assert_eq!(task.resource.as_deref(), Some("builder"));
        let hash = || (task.input_hash_fn)(&flow.context).unwrap();
        runner(&base).run(&flow.nodes, &flow.context, "").unwrap();
        assert_eq!(
            fs::read_to_string(base.join("job/delivery.txt")).unwrap(),
            "inline file yes"
        );
        assert!(base.join("logs/build.log").is_file());
        let warm = runner(&base);
        warm.run(&flow.nodes, &flow.context, "").unwrap();
        assert_eq!(warm.trace()[0]["status"], "cached");
        let prior = hash();
        fs::write(base.join("watch.txt"), "different").unwrap();
        assert_ne!(hash(), prior);
        let prior = hash();
        fs::write(base.join("input"), "new input").unwrap();
        assert_ne!(hash(), prior);
        assert!(load_declarative_workflow(&path, Some("[]"), None).is_err());
    }

    #[test]
    fn json_splitters_build_nested_gates_and_validate_packets_and_delivery() {
        let root = TempDir::new().unwrap();
        let base = root.path().canonicalize().unwrap();
        let doc = json!({"default_context":{"team":"QA","jobs":[{"id":"east","name":"East job"},{"id":"west","name":"West job"}]},
        "nodes":[{"id":"group","type":"subgraph","resource":"review","nodes":[
            {"id":"requests","type":"splitter","items_from_ctx":"jobs","max_workers":1,"template":{"id":"{{item.id}}","type":"gate",
                "packet_path":"{{item.id}}.packet.json","artifact_paths":["{{item.id}}.json"],"packet":{"title":"{{ctx.team}} - {{item.name}}","nested":["{{item.id}}"]},
                "verify_json":{"require_resolved":true,"require_approved":true,"require_fields":["reviewer"]},"cacheable":false}},
            {"id":"write","type":"splitter","items":["one","two"],"deps":["requests"],"template":{"id":"write-{{item}}","type":"shell",
                "command":["python3","-c","import sys; from pathlib import Path; Path(sys.argv[1]+'.txt').write_text(sys.argv[1])","{{item}}"],"outputs":["{{item}}.txt"]}}
        ]}]});
        let flow = json_workflow(&base, doc).unwrap();
        let run = || {
            runner(&base)
                .run(&flow.nodes, &flow.context, "")
                .map_err(Box::new)
        };
        let missing = run().unwrap_err();
        assert!(missing.awaiting_subagents);
        let packet = Value::Object(read_object(&base.join("east.packet.json")).unwrap());
        assert_eq!(packet, json!({"title":"QA - East job","nested":["east"]}));
        for (doc, field) in [
            (json!({"resolved":false}), "resolved"),
            (json!({"resolved":true,"approved":false}), "approved"),
            (json!({"resolved":true,"approved":true}), "reviewer"),
        ] {
            write_json(&base.join("east.json"), &doc, true).unwrap();
            assert!(run().unwrap_err().reason.contains(field))
        }
        let delivery = json!({"approved":true,"resolved":true,"reviewer":"Alex"});
        write_json(&base.join("east.json"), &delivery, true).unwrap();
        assert!(run().unwrap_err().node_path.ends_with("west"));
        write_json(&base.join("west.json"), &delivery, true).unwrap();
        run().unwrap();
        for item in ["one", "two"] {
            assert_eq!(
                fs::read_to_string(base.join(format!("{item}.txt"))).unwrap(),
                item
            )
        }
    }

    #[test]
    fn splitter_shell_templates_require_literal_argv_or_environment_values() {
        let root = TempDir::new().unwrap();
        let name = "two words; echo pwned";
        let mut doc = json!({"nodes":[{
            "type":"splitter", "id":"jobs", "items":[{"name":name}],
            "template":{"type":"shell", "id":"report", "command":"echo {{item.name}}"}
        }]});
        let err = json_workflow(root.path(), doc.clone()).err().unwrap();
        assert!(
            err.contains("argument array") && err.contains("env"),
            "{err}"
        );
        let template = &mut doc["nodes"][0]["template"];
        template["command"] = json!(["python3", "-c",
            "import json, sys; from pathlib import Path; Path('args.json').write_text(json.dumps(sys.argv[1:]))",
            "{{item.name}}"]);
        template["outputs"] = json!(["args.json"]);
        let flow = json_workflow(root.path(), doc.clone()).unwrap();
        runner(root.path())
            .run(&flow.nodes, &flow.context, "")
            .unwrap();
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(root.path().join("args.json")).unwrap()).unwrap();
        assert_eq!(args, [name]);

        #[cfg(unix)]
        {
            let template = &mut doc["nodes"][0]["template"];
            template["env"] = json!({"TARGET":"{{item.name}}"});
            template["command"] = json!(r#"printf '%s' "$TARGET" > env.txt"#);
            template["outputs"] = json!(["env.txt"]);
            let flow = json_workflow(root.path(), doc).unwrap();
            runner(root.path())
                .run(&flow.nodes, &flow.context, "")
                .unwrap();
            assert_eq!(
                fs::read_to_string(root.path().join("env.txt")).unwrap(),
                name
            );
        }
    }

    #[test]
    fn regression_visual_packet_resolves_shorthand_and_relative_paths_under_work_dir() {
        let root = TempDir::new().unwrap();
        let base = root.path().canonicalize().unwrap();
        let work_dir = base.join(".kgraph/suite");
        let flow = json_workflow(&base, json!({
            "preset":"game_regression", "suite_id":"suite", "work_dir":".kgraph/suite",
            "test_cmd":["python3","-c","pass"],
            "visual_checks":["overview", {"id":"dashboard","path":"shots/dashboard.png","min_width":200}]
        })).unwrap();
        let failure = runner(&base)
            .run(&flow.nodes, &flow.context, "")
            .unwrap_err();
        assert!(failure.awaiting_subagents, "{failure}");
        let packet = read_object(&work_dir.join("visual_packet.json")).unwrap();
        assert_eq!(
            packet["outputs"],
            json!({
                "overview": work_dir.join("visual/overview.png"),
                "dashboard": work_dir.join("shots/dashboard.png")
            })
        );
        assert_eq!(packet["visual_checks"][1]["min_width"], 200);
    }

    #[test]
    fn loop_retries_dirty_shell_node_with_command_and_json_verification() {
        let dir = TempDir::new().unwrap();
        let flow=json_workflow(dir.path(),json!({"nodes":[{"type":"loop_subgraph","id":"repair","max_retries":1,
            "resource":"checkout","dirty_nodes":["try"],"retry_from":"try","retry_to":"try","retry_label":"run again",
            "verify_cmd":["python3","-c","from pathlib import Path; assert Path('check.json').exists()"],"verify_json_path":"check.json",
            "nodes":[{"id":"try","type":"shell","command":["python3","-c","from pathlib import Path; import json; p=Path('check.json'); p.write_text(json.dumps({'ok':p.exists(),'error_signature':'retry','detail':'second attempt passes'}))"],"outputs":["check.json"]}]}]})).unwrap();
        let Node::LoopSubgraph(n) = &flow.nodes[0] else {
            panic!()
        };
        assert_eq!(n.retry_route.as_ref().unwrap().label, "run again (max 1)");
        let runner = runner(dir.path());
        runner.run(&flow.nodes, &flow.context, "").unwrap();
        assert_eq!(runner.state().nodes["repair"]["attempts"], 2);
    }

    #[test]
    fn shell_requires_matching_stdout_and_valid_png() {
        let dir = TempDir::new().unwrap();
        let mut shell = json!({"type":"shell","id":"check","command":["python3","-c","print('test')"],"expect_stdout_regex":"other","verify_pngs":[{"path":"image.png","min_width":16,"min_height":16}]});
        for expected in ["stdout_mismatch", "missing_file"] {
            let flow = json_workflow(dir.path(), json!({"nodes":[shell]})).unwrap();
            let Node::Task(n) = &flow.nodes[0] else {
                panic!()
            };
            let v = (n.run_fn)(&flow.context, 0).unwrap();
            let verdict = (n.verify_fn.as_ref().unwrap())(&flow.context, &v);
            assert!(!verdict.ok);
            assert!(verdict.error_signature.contains(expected), "{verdict:?}");
            shell["expect_stdout_regex"] = json!("test");
        }
    }

    #[test]
    fn all_presets_produce_native_graphs_and_relative_artifact_paths() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let bug = json!({"id":"issue","title":"Settings"});
        let cases = [
            (
                json!({"preset":"desktop_bug_fix","config":{"work_dir":"work","bug_spec":bug,"repro_cmd":["true"]}}),
                vec!["issue"],
            ),
            (
                json!({"preset":"game_regression","suite_id":"suite","work_dir":"suite-work","repo_dir":".","test_cmd":"true","watch_globs":["*.rs"]}),
                vec!["suite"],
            ),
            (
                json!({"preset":"histoury_bug_fix","batch_id":"desktop-42","work_dir":"work","repos":{"client":"repo"},"test_cmds":{"client":["true"]}}),
                vec![
                    "select_bugs",
                    "capture_before",
                    "fix_and_review_loop",
                    "release",
                ],
            ),
            (
                json!({"preset":"version_history","work_dir":"pkg","release_id":"v1"}),
                vec!["version_history_package"],
            ),
        ];
        for (doc, ids) in cases {
            let flow = json_workflow(&base, doc).unwrap();
            assert_eq!(flow.nodes.iter().map(Node::id).collect::<Vec<_>>(), ids);
            if matches!(ids[0], "suite" | "select_bugs") {
                assert!(flow.qa_checklist_path.unwrap().starts_with(&base))
            }
        }
        let flow = json_workflow(
            &base,
            json!({"preset":"bug_batch","bugs":[bug],"work_dir":"work","test_cmd":"true"}),
        )
        .unwrap();
        assert_eq!(
            flow.nodes.iter().map(Node::id).collect::<Vec<_>>(),
            ["issue", "consolidated_report"]
        );
        let toml = base.join("preset.toml");
        fs::write(&toml,"preset='histoury_bug_fix'\nbatch_id='desktop-1'\nwork_dir='work'\n[repos]\nclient='repo'").unwrap();
        let flow = load_declarative_workflow(&toml, None, None).unwrap();
        let err = runner(&base)
            .run(&flow.nodes, &flow.context, "")
            .unwrap_err();
        assert_eq!(
            Path::new(err.packet_path.as_deref().unwrap()),
            base.join("work/bug_reports_packet.json")
        );
    }

    #[test]
    fn agent_evaluator_retries_with_repair_and_accounts_for_both_attempts() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("prompt.txt"), "Fix {{ctx.topic}}").unwrap();
        let flow = json_workflow(dir.path(), json!({"default_context":{"topic":"layout"}, "nodes":[{
            "type":"agent","id":"author","adapter":"custom","prompt_file":"prompt.txt",
            "argv":["python3","-c",
                "import json,sys,os; assert os.environ['TOPIC']=='layout'; print(json.dumps({'result':'done','usage':{'input_tokens':3,'output_tokens':2}})); print('API_KEY=secret-123',file=sys.stderr)",
                "{{attempt}}"],
            "env":{"TOPIC":"{{ctx.topic}}"},"prompt_artifact_path":"prompt.log","log_path":"agent.log",
            "response_output_path":"answer.txt","max_retries":1,
            "evaluator":{"adapter":{"kind":"custom","argv":["python3","-c",
                "import json,sys,os; assert os.environ['REVIEW']=='layout'; print(json.dumps({'approved':sys.argv[1]=='2','feedback':'use larger type','usage':{'input_tokens':7}}))","{{attempt}}"]},
                "env":{"REVIEW":"{{ctx.topic}}"},"prompt":"Review {{ctx.topic}}","timeout_s":10,"log_path":"review.log"}
        }]})).unwrap();
        let runner = runner(dir.path());
        runner.run(&flow.nodes, &flow.context, "").unwrap();
        let prompt = fs::read_to_string(dir.path().join("prompt.log")).unwrap();
        for text in [
            "Fix layout",
            "Prior evaluator critique",
            "use larger type",
            "Repair context",
            "log_tail",
        ] {
            assert!(prompt.contains(text), "{prompt}");
        }
        assert!(!prompt.contains("secret-123"));
        assert_eq!(
            fs::read_to_string(dir.path().join("answer.txt")).unwrap(),
            "done"
        );
        let telemetry = runner.build_run_telemetry("passed", 0, None);
        assert_eq!(telemetry.total_tokens, 24);
        let node = &telemetry.nodes[0];
        assert_eq!(node.attempts, 2);
        assert!(node.attempt_usage[0].failed && !node.attempt_usage[1].failed);
        assert_eq!(
            node.agent_usage.as_ref().unwrap().provider.as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn schema_and_exports_apply_only_to_verified_outputs() {
        let schema =
            json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]});
        for mut node in [
            json!({"type":"shell","command":["python3","-c","pass"],"outputs":["note.txt","answer.json"]}),
            json!({"type":"gate","packet_path":"packet.json","packet":{},"artifact_paths":["note.txt","answer.json"]}),
            json!({"type":"agent","adapter":"custom","argv":["python3","-c","from pathlib import Path; print(Path('source').read_text())"],
                "prompt":"answer","response_output_path":"answer.json","outputs":["note.txt"]}),
        ] {
            let dir = TempDir::new().unwrap();
            node["id"] = json!("check");
            node["output_schema"] = schema.clone();
            node["export_json"] = json!({"path":"answer.json","key":"answer"});
            let wf = json_workflow(dir.path(), json!({"nodes":[node]})).unwrap();
            fs::write(dir.path().join("note.txt"), "ordinary text").unwrap();
            for (doc, expected) in [
                (r#"{"count":"wrong"}"#, Some("schema_violation:/count")),
                ("{", Some("schema_violation:")),
                (r#"{"count":4}"#, None),
            ] {
                fs::write(dir.path().join("answer.json"), doc).unwrap();
                fs::write(dir.path().join("source"), doc).unwrap();
                let r = runner(dir.path());
                let result = r.run(&wf.nodes, &wf.context, "");
                if let Some(signature) = expected {
                    assert!(result.is_err());
                    assert_eq!(r.state().nodes["check"]["error_signature"], signature);
                    assert!(wf.context.get("answer").is_none());
                } else {
                    result.unwrap();
                    assert_eq!(wf.context.get("answer"), Some(json!({"count":4})));
                }
            }
        }
    }

    #[test]
    fn schema_requires_targets_at_load_and_validates_extensionless_responses() {
        let dir = TempDir::new().unwrap();
        let mut node = json!({"type":"agent","id":"check","prompt":"check","adapter":"custom",
            "argv":["python3","-c","print('false')"],"output_schema":{"type":"integer"}});
        for doc in [
            json!({"nodes":[node]}),
            json!({"nodes":[{"type":"splitter","id":"s","items":[],"template":node}]}),
        ] {
            assert!(json_workflow(dir.path(), doc)
                .err()
                .unwrap()
                .contains("output_schema"));
        }
        node["response_output_path"] = json!("out/verdict");
        let wf = json_workflow(dir.path(), json!({"nodes":[node]})).unwrap();
        let r = runner(dir.path());
        assert!(r.run(&wf.nodes, &wf.context, "").is_err());
        assert_eq!(
            r.state().nodes["check"]["error_signature"],
            "schema_violation:"
        );
    }

    #[test]
    fn schema_checks_all_selected_outputs_and_honors_explicit_target() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        for names in [["first.json", "second.json"], ["first", "second"]] {
            fs::write(dir.join(names[0]), "5").unwrap();
            fs::write(dir.join(names[1]), "false").unwrap();
            let mut node = json!({"type":"shell","id":"check","command":"true","outputs":names,"output_schema":{"type":"integer"}});
            let run_node = |n: &Value| {
                let wf = json_workflow(dir, json!({"nodes":[n]})).unwrap();
                runner(dir)
                    .run(&wf.nodes, &wf.context, "")
                    .map_err(|e| e.to_string())
            };
            assert!(run_node(&node).is_err());
            node["output_schema_path"] = json!(names[0]);
            run_node(&node).unwrap();
        }
    }

    #[test]
    fn parallel_files_and_same_key_exports_reduce_in_item_order() {
        let dir = TempDir::new().unwrap();
        let template = json!({"type":"shell","id":"item-{{item}}","outputs":["{{item}}.json"],
            "command":["python3","-c","import sys,time; from pathlib import Path; n=sys.argv[1]; time.sleep(int(n)*0.02); Path(n+'.json').write_text(n)","{{item}}"],
            "export_json":{"path":"{{item}}.json","key":"value"}});
        let doc = json!({"nodes":[
            {"id":"ordered","type":"splitter","items":[3,2,1],"max_workers":3,"template":template,
                "reduce":{"from_output":"{{item}}.json","to_ctx":"ordered","op":"collect_list"}},
            {"id":"sum","deps":["ordered"],"type":"splitter","items_from_ctx":"ordered","max_workers":3,"template":template,
                "reduce":{"from_ctx_key":"value","to_ctx":"total","op":"sum"}},
            {"id":"empty","type":"splitter","items":[],"template":template,
                "reduce":{"from_ctx_key":"value","to_ctx":"empty","op":"collect_list"}},
            {"id":"consume","type":"agent","deps":["sum"],"prompt":"sum={{ctx.total}}",
                "adapter":{"kind":"custom","argv":["python3","-c","import sys; print(sys.argv[1].splitlines()[0])","{{prompt}}"]},
                "response_output_path":"out.txt"}]});
        for _ in 0..2 {
            let wf = json_workflow(dir.path(), doc.clone()).unwrap();
            runner(dir.path()).run(&wf.nodes, &wf.context, "").unwrap();
            assert_eq!(wf.context.get("ordered"), Some(json!([3, 2, 1])));
            assert_eq!(wf.context.get("total"), Some(json!(6)));
            assert_eq!(wf.context.get("empty"), Some(json!([])));
            assert_eq!(
                fs::read_to_string(dir.path().join("out.txt"))
                    .unwrap()
                    .trim(),
                "sum=6"
            );
        }
    }

    fn temp_repo(dir: &Path) {
        use std::process::Command;
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Tester")
                .env("GIT_COMMITTER_NAME", "Tester")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        fs::write(dir.join("shared.txt"), "original\n").unwrap();
        git(&["add", "--", "shared.txt"]);
        let tree = git(&["write-tree"]);
        let commit = git(&["commit-tree", &tree, "-m", "test fixture"]);
        git(&["update-ref", "HEAD", &commit]);
    }

    #[test]
    fn declarative_file_fields_reject_parent_traversal_and_absolute_escapes() {
        check_path_escapes(false);
    }

    #[cfg(unix)]
    #[test]
    fn declarative_file_fields_reject_escaping_symlink_ancestors() {
        check_path_escapes(true);
    }

    fn check_path_escapes(link: bool) {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path().canonicalize().unwrap();
        let dir = outer.join("workflow");
        fs::create_dir(&dir).unwrap();
        temp_repo(&dir);
        fs::write(outer.join("outside.json"), "1").unwrap();
        #[cfg(unix)]
        if link {
            std::os::unix::fs::symlink(&outer, dir.join("link")).unwrap();
        }
        let parents = if link {
            vec![PathBuf::from("link")]
        } else {
            vec![PathBuf::from(".."), outer.clone()]
        };
        for parent in parents {
            let file = parent.join("outside.json");
            let task = json!({"type":"shell","id":"child","command":"true"});
            let cases = [
                json!({"type":"agent","id":"prompt","adapter":"custom","argv":["true"],"prompt_file":file}),
                json!({"type":"shell","id":"export","command":"true","export_json":{"key":"answer","path":file}}),
                json!({"type":"splitter","id":"reduce","items":[1],"template":task,
                    "reduce":{"from_output":file,"to_ctx":"answer","op":"sum"}}),
                json!({"type":"shell","id":"patch","isolation":"worktree",
                    "worktree":{"patch_dir":parent.join("patches"),"merge_policy":"patch"},"command":"echo changed > shared.txt"}),
            ];
            for node in cases {
                let wf = json_workflow(&dir, json!({"nodes":[node]})).unwrap();
                let error = runner(&dir).run(&wf.nodes, &wf.context, "").unwrap_err();
                assert!(
                    error.to_string().contains("must stay inside"),
                    "{node}: {error}"
                );
                assert!(wf.context.get("answer").is_none());
            }
        }
        assert!(!outer.join("patches").exists());
        assert_eq!(
            fs::read_to_string(dir.join("shared.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn failed_isolated_reduction_cannot_integrate_or_publish_changes() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        temp_repo(&dir);
        fs::write(
            dir.join(".git/info/exclude"),
            ".kgraph/\nstate*\nflow.json\ntelemetry*\n",
        )
        .unwrap();
        let status = || {
            std::process::Command::new("git")
                .current_dir(&dir)
                .args(["status", "--porcelain"])
                .output()
                .unwrap()
                .stdout
        };
        let wf = json_workflow(&dir, json!({"nodes":[{
            "type":"splitter","id":"reduce","items":[1],"isolation":"worktree",
            "worktree":{"merge_policy":"patch","keep_on_failure":true},
            "template":{"type":"shell","id":"child","command":"echo changed > shared.txt && echo false > result.json",
                "export_json":{"path":"result.json","key":"value"}},
            "reduce":{"from_ctx_key":"value","op":"sum","to_ctx":"total"}
        }]})).unwrap();
        let before = status();
        let error = runner(&dir).run(&wf.nodes, &wf.context, "").unwrap_err();
        assert!(
            error.to_string().contains("sum requires numbers"),
            "{error}"
        );
        assert_eq!(status(), before);
        assert_eq!(
            fs::read_to_string(dir.join("shared.txt")).unwrap(),
            "original\n"
        );
        assert!(!dir.join("result.json").exists());
        assert!(wf.context.get("value").is_none());
        assert!(wf.context.get("total").is_none());
        assert_eq!(
            fs::read_dir(dir.join(".kgraph/worktrees")).unwrap().count(),
            1
        );
    }

    #[test]
    fn parallel_worktrees_verify_before_cleanup_and_export_distinct_patches() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        temp_repo(&dir);
        let cmd = json!(["python3","-c",
            "import os,sys,json,time; from pathlib import Path; p=Path('shared.txt'); assert p.read_text()=='original\\n' and Path(os.environ['KGRAPH_WORKTREE']).resolve()==Path.cwd().resolve(); p.write_text(sys.argv[1]+'\\n'); time.sleep(.05); assert p.read_text().strip()==sys.argv[1]; Path('out.json').write_text(json.dumps(sys.argv[1])); print(json.dumps(sys.argv[1]))",
            "{{item}}"]);
        for (kind, task) in [
            (
                "shell",
                json!({"type":"shell","command":cmd,"outputs":["out.json"]}),
            ),
            (
                "agent",
                json!({"type":"agent","adapter":{"kind":"custom","argv":cmd},"prompt":"Edit {{item}}","response_output_path":"out.json"}),
            ),
        ] {
            let mut template = task;
            template["id"] = json!("worker-{{item}}");
            template["output_schema"] = json!({"type":"string"});
            let wf = json_workflow(&dir, json!({"nodes":[{
                "type":"splitter","id":kind,"max_workers":2,"items":["west","east"],"isolation":"worktree",
                "worktree":{"merge_policy":"none","patch_dir":kind},"template":template,
                "reduce":{"from_output":"out.json","op":"collect_list","to_ctx":"done"}
            }]})).unwrap();
            runner(&dir).run(&wf.nodes, &wf.context, "").unwrap();
            assert_eq!(wf.context.get("done"), Some(json!(["west", "east"])));
            assert_eq!(
                fs::read_to_string(dir.join("shared.txt")).unwrap(),
                "original\n"
            );
            assert!(!dir.join("out.json").exists());
            for name in ["west", "east"] {
                let patch = fs::read_to_string(
                    dir.join(kind)
                        .join(kind)
                        .join(format!("worker-{name}.patch")),
                )
                .unwrap();
                assert!(patch.contains(&format!("+{name}\n")), "{patch}");
            }
            assert_eq!(
                fs::read_dir(dir.join(".kgraph/worktrees")).unwrap().count(),
                0
            );
        }
    }

    #[test]
    fn nested_worktree_branches_can_reduce_a_disposable_file() {
        let dir = tempfile::tempdir().unwrap();
        temp_repo(dir.path());
        let wf = json_workflow(dir.path(), json!({"nodes":[{
            "id":"nested","type":"splitter","items":[4,2],"max_workers":2,"isolation":"worktree",
            "reduce":{"from_output":"answer.json","to_ctx":"answers","op":"collect_list"},
            "template":{"id":"branch-{{item}}","type":"subgraph","nodes":[{
                "id":"write","type":"shell","env":{"N":"{{item}}"},"command":["python3","-c","import os; open('answer.json','w').write(os.environ['N'])"],"outputs":["answer.json"]
            }]}
        }]})).unwrap();
        runner(dir.path()).run(&wf.nodes, &wf.context, "").unwrap();
        assert_eq!(wf.context.get("answers"), Some(json!([4, 2])));
        assert!(!dir.path().join("answer.json").exists());
    }

    #[test]
    fn isolated_agent_keeps_failed_worktree_and_supplies_diff_on_retry() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        temp_repo(&dir);
        let mut node = json!({"type":"agent","id":"isolated","adapter":"custom","prompt":"edit",
            "isolation":"worktree","worktree":{"keep_on_failure":true},"log_path":"worker.log",
            "max_retries":1,"argv":["python3","-c",
            "from pathlib import Path; import sys; Path('shared.txt').write_text('edit'); print('API_KEY=bad-secret',file=sys.stderr); assert sys.argv[1]=='2' and 'git_diff_stat' in sys.argv[2] and 'shared.txt' in sys.argv[2] and 'bad-secret' not in sys.argv[2]",
            "{{attempt}}","{{prompt}}"]});
        let wf = json_workflow(&dir, json!({"nodes":[node]})).unwrap();
        runner(&dir).run(&wf.nodes, &wf.context, "").unwrap();
        let bundle = kgraph_core::repair::get_repair_context(&wf.context, "isolated").unwrap();
        assert!(bundle.git_diff_stat.unwrap().contains("shared.txt"));
        assert!(!bundle.log_tail.unwrap().contains("bad-secret"));
        assert_eq!(
            fs::read_dir(dir.join(".kgraph/worktrees")).unwrap().count(),
            0
        );
        node["max_retries"] = json!(0);
        let wf = json_workflow(&dir, json!({"nodes":[node]})).unwrap();
        assert!(runner(&dir).run(&wf.nodes, &wf.context, "").is_err());
        assert_eq!(
            fs::read_to_string(dir.join(".kgraph/worktrees/isolated/shared.txt")).unwrap(),
            "edit"
        );
        assert_eq!(
            fs::read_to_string(dir.join("shared.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn container_reductions_keep_same_named_exports_private_to_each_branch() {
        let dir = TempDir::new().unwrap();
        let wf = json_workflow(dir.path(), json!({"nodes":[{
            "type":"splitter","id":"branches","items":["a","b"],"max_workers":2,
            "reduce":{"from_ctx_key":"answer","op":"collect_list","to_ctx":"answer"},
            "template":{"type":"subgraph","id":"{{item}}","nodes":[
                {"type":"shell","id":"write","command":["python3","-c","import json,sys; from pathlib import Path; Path(sys.argv[1]+'.json').write_text(json.dumps(sys.argv[1]))","{{item}}"],
                 "export_json":{"key":"answer","path":"{{item}}.json"}},
                {"type":"shell","id":"wait","deps":["write"],"command":["python3","-c","import time; time.sleep(.08)"]}
            ]}
        }]})).unwrap();
        runner(dir.path()).run(&wf.nodes, &wf.context, "").unwrap();
        assert_eq!(wf.context.get("answer"), Some(json!(["a", "b"])));
    }

    #[test]
    fn invalid_agent_and_reducer_specs_report_load_errors() {
        let dir = TempDir::new().unwrap();
        for node in [
            json!({"type":"agent","id":"a","adapter":"codex"}),
            json!({"type":"agent","id":"a","adapter":"codex","prompt":"a","prompt_file":"b"}),
            json!({"type":"agent","id":"a","adapter":{"kind":"custom","argv":[]},"prompt":"a"}),
            json!({"type":"agent","id":"a","adapter":"codex","prompt":"a","verify_cmd":"echo {{ctx.message}}"}),
            json!({"type":"splitter","id":"a","items":[],"template":{},"reduce":{"op":"sum","to_ctx":"a"}}),
            json!({"type":"splitter","id":"a","items":[],"template":{},"reduce":{"op":"sum","to_ctx":"a","from_output":"a","from_ctx_key":"b"}}),
        ] {
            assert!(
                json_workflow(dir.path(), json!({"nodes":[node]})).is_err(),
                "{node}"
            );
        }
    }

    #[test]
    fn loop_repair_reaches_dirty_agent_prompt() {
        let dir = TempDir::new().unwrap();
        let wf = json_workflow(dir.path(), json!({"nodes":[{
            "type":"loop_subgraph","id":"review","dirty_nodes":["fix"],"max_retries":1,
            "verify_json_path":"verdict.json","nodes":[{
                "type":"agent","id":"fix","adapter":"custom","prompt":"repair","prompt_artifact_path":"prompt.txt",
                "argv":["python3","-c","import json,sys; print(json.dumps({'ok':'Repair context' in sys.argv[1], 'error_signature':'needs_revision','detail':'change the label'}))","{{prompt}}"],
                "response_output_path":"verdict.json"
            }]
        }]})).unwrap();
        runner(dir.path()).run(&wf.nodes, &wf.context, "").unwrap();
        for id in ["review", "fix"] {
            let bundle = kgraph_core::repair::get_repair_context(&wf.context, id).unwrap();
            assert_eq!(bundle.error_signature, "needs_revision");
            assert_eq!(bundle.next_attempt, 2);
        }
        assert!(fs::read_to_string(dir.path().join("prompt.txt"))
            .unwrap()
            .contains("change the label"));
    }

    #[test]
    fn invalid_commands_context_templates_and_controls_report_load_errors() {
        let dir = TempDir::new().unwrap();
        for doc in [
            json!({"nodes":[],"preset":"bug_batch"}),
            json!({"default_context":[],"nodes":[]}),
            json!({"nodes":[{"type":"shell","id":"run","command":["ok",1]}]}),
            json!({"nodes":[{"type":"shell","id":"run","command":"{{ctx.unknown}}"}]}),
            json!({"nodes":[{"type":"shell","id":"run","command":"ok","expect_stdout_regex":"["}]}),
            json!({"nodes":[{"type":"splitter","id":"items","items":[],"items_from_ctx":"both","template":{}}]}),
            json!({"nodes":[{"type":"loop_subgraph","id":"verify","nodes":[]}]}),
        ] {
            assert!(json_workflow(dir.path(), doc).is_err())
        }
    }

    #[test]
    fn declarative_when_finally_budgets_protected_paths_and_schema() {
        let dir = TempDir::new().unwrap();
        let protected = dir.path().join("guard.txt");
        fs::write(&protected, "original").unwrap();

        // 1. `when` expressions + `finally` on failure + `[budgets]` parsing
        let wf = json_workflow(
            dir.path(),
            json!({
                "title": "v0.4.0 declarative",
                "default_context": {
                    "env": "staging",
                    "areas": ["api", "ui"],
                    "run_slow": false
                },
                "budgets": {
                    "max_cost_usd": 2.5,
                    "max_tokens": 50000,
                    "max_wall_time_s": 120.0
                },
                "nodes": [
                    {
                        "id": "skipped_slow",
                        "type": "shell",
                        "when": "ctx.run_slow && 'api' in ctx.areas",
                        "command": ["python3", "-c", "open('should_not_exist','w').write('x')"]
                    },
                    {
                        "id": "runs_api",
                        "type": "shell",
                        "deps": ["skipped_slow"],
                        "when": "ctx.env != 'prod' && 'api' in ctx.areas",
                        "command": ["python3", "-c", "open('ran_api.txt','w').write('ok')"],
                        "outputs": ["ran_api.txt"]
                    },
                    {
                        "id": "tamper",
                        "type": "shell",
                        "deps": ["runs_api"],
                        "immutable_paths": ["guard.txt"],
                        "command": ["python3", "-c", "open('guard.txt','w').write('tampered')"]
                    }
                ],
                "finally": [
                    {
                        "id": "on_fail_note",
                        "type": "shell",
                        "when": "ctx.__kgraph_run_status != 'passed'",
                        "command": ["python3", "-c", "import os; open('finally_ran.txt','w').write(os.environ['STATUS'])"],
                        "env": { "STATUS": "{{ctx.__kgraph_run_status}}" }
                    }
                ]
            }),
        )
        .unwrap();
        assert_eq!(wf.budgets.max_cost_usd, Some(2.5));
        assert_eq!(wf.budgets.max_tokens, Some(50000));
        assert_eq!(wf.budgets.max_wall_time_s, Some(120.0));
        assert_eq!(wf.finally_nodes.len(), 1);

        let mut cfg = RunnerConfig::new(dir.path().join("state.json"));
        cfg.logger = Some(Arc::new(|_| {}));
        cfg.finally_nodes = wf.finally_nodes;
        let r = GraphRunner::new(cfg).unwrap();
        let err = r.run(&wf.nodes, &wf.context, "").unwrap_err();
        assert_eq!(err.node_path, "tamper");
        assert!(err.reason.contains("Protected files were modified"), "{}", err.reason);
        assert!(!dir.path().join("should_not_exist").exists());
        assert_eq!(fs::read_to_string(dir.path().join("ran_api.txt")).unwrap(), "ok");
        assert_eq!(fs::read_to_string(dir.path().join("finally_ran.txt")).unwrap(), "failed");

        // 2. JSON schema exposes v0.4.0 fields cleanly
        let schema = workflow_json_schema();
        assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert!(schema["properties"]["budgets"].is_object());
        assert!(schema["properties"]["finally"].is_object());
        let node_props = &schema["$defs"]["node"]["properties"];
        for key in ["when", "retry_backoff_s", "transient_retries", "oscillation_breaker", "immutable_paths", "max_feedback_bytes"] {
            assert!(node_props.get(key).is_some(), "schema missing {key}");
        }
    }
}
