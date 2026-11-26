//! Load native workflow graphs from a TOML or JSON document.

use crate::bug_fix::{
    create_bug_batch_graph, create_desktop_bug_fix_subgraph, outputs, resolve_path, BugBatchConfig,
    DesktopBugFixConfig,
};
use crate::common::{fingerprint, verification, with_retry_route};
use crate::histoury_bug_fix::{create_histoury_bug_fix_graph, parse_command, HistouryBugFixConfig};
use crate::regression::{create_game_regression_graph, GameRegressionConfig, VisualCheckConfig};
use crate::version_history::{create_version_history_subgraph, VersionHistoryConfig};
use kgraph_core::files::{read_object, require_id};
use kgraph_core::gate::{subagent_gate, SubagentGateOptions};
use kgraph_core::hash::{hash_globs, hash_paths};
use kgraph_core::shell::{run_command_streaming, shell_task, CommandSpec, ShellTaskOptions};
use kgraph_core::types::*;
use kgraph_core::verifiers::verify_png_file;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct LoadedWorkflow {
    pub title: Option<String>,
    pub nodes: Vec<Node>,
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

fn validate_node_commands(node: &Value) -> Result<(), String> {
    for key in ["command", "verify_cmd"] {
        validate_shell_template(key, &node[key])?;
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
    let mut result = String::new();
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
        let spec_hash = fingerprint(self.value.clone());
        let base = self.base.to_owned();
        let globs = self.strings("watch_globs")?;
        let paths = self.paths("watch_paths")?;
        Ok(Arc::new(move |_| {
            Ok(fingerprint(json!([
                spec_hash,
                hash_globs(&globs, Some(&base), None, &exclude, true)?,
                hash_paths(&paths, Some(&base), None, &exclude, true)?
            ])))
        }))
    }

    fn shell(&self, id: &str) -> Result<Node, String> {
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
        let pngs = self
            .or::<Vec<Value>>("verify_pngs", vec![])?
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
            .collect::<Result<Vec<_>, String>>()?;
        // Verified PNGs also participate in output freshness on subsequent runs.
        let mut tracked = paths.clone();
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
            VerifyOutcome::pass("Command passed")
        }));
        Ok(Node::Task(shell_task(opts)))
    }

    fn gate(&self, id: &str) -> Result<Node, String> {
        let paths = self.paths("artifact_paths")?;
        if paths.is_empty() {
            return Err("A gate requires artifact_paths".into());
        }
        let packet = self.get::<Map<String, Value>>("packet")?;
        let packet_path = self.path("packet_path")?;
        let mut tracked = paths.clone();
        tracked.push(packet_path.clone());
        let mut opts = SubagentGateOptions::new(
            id,
            packet_path,
            outputs(paths),
            Arc::new(move |_| Ok(Value::Object(packet.clone()))),
            self.hash(tracked)?,
        );
        opts.deps = self.strings("deps")?;
        opts.resource = self.or("resource", None)?;
        opts.cacheable = self.or("cacheable", true)?;
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Checks {
            require_resolved: bool,
            require_approved: bool,
            require_fields: Vec<String>,
        }
        if let Some(checks) = self.or::<Option<Checks>>("verify_json", None)? {
            opts.verify_fn = Some(Arc::new(move |_, artifacts| {
                for path in artifacts {
                    let doc = match read_object(path) {
                        Ok(doc) => doc,
                        Err(e) => return VerifyOutcome::fail("invalid_json", e),
                    };
                    for (key, required) in [
                        ("resolved", checks.require_resolved),
                        ("approved", checks.require_approved),
                    ] {
                        if required && doc.get(key) != Some(&Value::Bool(true)) {
                            return VerifyOutcome::fail(
                                "invalid_json",
                                format!("{} requires {key} = true", path.display()),
                            );
                        }
                    }
                    for key in &checks.require_fields {
                        if doc.get(key).is_none_or(Value::is_null) {
                            return VerifyOutcome::fail(
                                "invalid_json",
                                format!("{} requires {key}", path.display()),
                            );
                        }
                    }
                }
                VerifyOutcome::pass("JSON artifacts verified")
            }))
        }
        Ok(Node::Task(subagent_gate(opts)))
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
        let mut n = LoopSubgraphNode::new(
            id,
            children,
            Arc::new(move |_| {
                if let Some(cmd) = &cmd {
                    match run_command_streaming(cmd, Some(&cwd), &BTreeMap::new(), timeout, None) {
                        Ok(out) if out.returncode != 0 => {
                            return Ok(VerifyOutcome::fail(
                                format!("exit_{}", out.returncode),
                                format!("{}{}", out.stdout, out.stderr),
                            ))
                        }
                        Err(e) => return Ok(VerifyOutcome::fail("verify_cmd", e.to_string())),
                        _ => (),
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
        n.deps = self.strings("deps")?;
        n.resource = self.or("resource", None)?;
        let dirty = self.or(
            "dirty_nodes",
            n.nodes
                .iter()
                .map(|n| n.id().to_owned())
                .collect::<Vec<_>>(),
        )?;
        n.repair_fn = Some(Arc::new(move |_, _, _, _| Ok(dirty.clone())));
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

fn build_node(
    spec: &Value,
    base: &Path,
    ctx: &Context,
    item: Option<&Value>,
) -> Result<Node, String> {
    let mut fields = spec
        .as_object()
        .ok_or("Each node must be an object")?
        .clone();
    // A nested splitter binds its own item when it expands. Keep its child template intact.
    let template = fields.remove("template");
    let children = fields.remove("nodes");
    let rendered = render(&Value::Object(fields), ctx, item)?;
    let s = Spec {
        value: &rendered,
        base,
    };
    let id = require_id(&s.get::<String>("id")?)?;
    let build = || {
        build_nodes(
            children.as_ref().ok_or("Missing child nodes")?,
            base,
            ctx,
            item,
        )
    };
    match s.get::<String>("type")?.as_str() {
        "shell" => s.shell(&id),
        "gate" => s.gate(&id),
        "subgraph" => {
            let mut n = SubgraphNode::new(id, build()?);
            n.deps = s.strings("deps")?;
            n.resource = s.or("resource", None)?;
            Ok(Node::Subgraph(n))
        }
        "loop_subgraph" => s.review_loop(&id, build()?),
        "splitter" => {
            let items = s.or::<Option<Vec<Value>>>("items", None)?;
            let key = s.or::<Option<String>>("items_from_ctx", None)?;
            let items_fn: SplitterItemsFn = match (items, key) {
                (Some(items), None) => Arc::new(move |_| Ok(items.clone())),
                (None, Some(key)) => Arc::new(move |ctx| {
                    ctx.get(&key)
                        .and_then(|v| v.as_array().cloned())
                        .ok_or_else(|| format!("items_from_ctx {key} must identify an array"))
                }),
                _ => return Err("Specify exactly one of items and items_from_ctx".into()),
            };
            let template = template
                .filter(Value::is_object)
                .ok_or("Missing splitter template object")?;
            let dir = base.to_owned();
            let mut n = SplitterNode::new(
                id,
                items_fn,
                Arc::new(move |item, ctx| build_node(&template, &dir, ctx, Some(item))),
            );
            n.max_workers = s.or("max_workers", n.max_workers)?;
            if n.max_workers == 0 {
                return Err("max_workers must be positive".into());
            }
            n.deps = s.strings("deps")?;
            n.resource = s.or("resource", None)?;
            Ok(Node::Splitter(n))
        }
        kind => Err(format!("{id}: unknown node type {kind:?}")),
    }
}
fn build_nodes(
    raw: &Value,
    base: &Path,
    ctx: &Context,
    item: Option<&Value>,
) -> Result<Vec<Node>, String> {
    raw.as_array()
        .ok_or("nodes must be an array")?
        .iter()
        .map(|v| build_node(v, base, ctx, item))
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

pub fn load_declarative_workflow(
    path: &Path,
    ctx_inline: Option<&str>,
    ctx_file: Option<&Path>,
) -> Result<LoadedWorkflow, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let doc: Value = match path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => {
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?
        }
        Some("toml") => toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?,
        _ => {
            return Err(format!(
                "Expected a TOML or JSON workflow: {}",
                path.display()
            ))
        }
    };
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
    let preset = fields.remove("preset");
    // Inspect command sources before rendering erases the boundary between code and data.
    if let Some(nodes) = explicit.as_ref().and_then(Value::as_array) {
        for node in nodes {
            validate_node_commands(node)?;
        }
    }
    if preset.is_some() {
        validate_preset_commands(&fields)?;
        if let Some(config) = fields.get("config").and_then(Value::as_object) {
            validate_preset_commands(config)?;
        }
    }
    let rendered = render(&Value::Object(fields), &context, None)?;
    let spec = Spec {
        base: &base,
        value: &rendered,
    };
    let nodes = match (explicit, preset) {
        (Some(raw), None) => build_nodes(&raw, &base, &context, None)?,
        (None, Some(Value::String(name))) => {
            // Accept a nested config table as well as fields alongside preset.
            if let Some(cfg) = rendered.get("config") {
                let mut fields = rendered.as_object().unwrap().clone();
                fields.extend(cfg.as_object().ok_or("config must be an object")?.clone());
                Spec {
                    base: &base,
                    value: &Value::Object(fields),
                }
                .preset(&name)?
            } else {
                spec.preset(&name)?
            }
        }
        _ => return Err("Specify either a nodes array or a named preset".into()),
    };
    let qa_checklist_path = spec
        .optional_path("qa_checklist_path")?
        .or_else(|| graph_qa_checklist(&nodes));
    Ok(LoadedWorkflow {
        title: spec.or("title", None)?,
        nodes,
        context,
        qa_checklist_path,
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
}
