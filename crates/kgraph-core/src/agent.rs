//! Literal-argv agent workers with deterministic verification and optional evaluator retries.

use crate::files::write_text;
use crate::repair::redact_secrets;
use crate::shell::{run_command_streaming, CommandSpec};
use crate::types::{
    AgentUsage, Context, EfficiencyCounts, InputHashFn, OutputsFn, TaskKind, TaskNode, TaskResult,
    TaskRunError, VerifyFn, VerifyOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAdapter {
    Codex {
        bin: Option<String>,
        model: Option<String>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Claude {
        bin: Option<String>,
        model: Option<String>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Agy {
        bin: Option<String>,
        model: Option<String>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    /// Placeholders are expanded once within each argument. The staged output file is
    /// optional: an adapter can write its answer there and still emit usage on stdout.
    Custom { argv: Vec<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluatorConfig {
    pub adapter: AgentAdapter,
    pub prompt: String,
    pub timeout_s: Option<f64>,
    pub log_path: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Relative paths (including paths returned by outputs_fn) are relative to cwd.
/// Static and dynamic outputs are both required. Attempt numbers in prompts and results start at 1.
pub struct AgentTaskOptions {
    pub id: String,
    pub adapter: AgentAdapter,
    pub prompt: String,
    pub prompt_file: Option<PathBuf>,
    pub input_hash_fn: InputHashFn,
    pub outputs: Vec<PathBuf>,
    pub outputs_fn: Option<OutputsFn>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub log_path: Option<PathBuf>,
    pub prompt_artifact_path: Option<PathBuf>,
    pub response_output_path: Option<PathBuf>,
    pub timeout_s: Option<f64>,
    pub max_retries: usize,
    pub deps: Vec<String>,
    pub resource: Option<String>,
    pub cacheable: bool,
    pub deterministic_verify_fn: Option<VerifyFn>,
    pub evaluator: Option<EvaluatorConfig>,
}

impl AgentTaskOptions {
    pub fn new(
        id: impl Into<String>,
        adapter: AgentAdapter,
        prompt: impl Into<String>,
        input_hash_fn: InputHashFn,
    ) -> Self {
        Self {
            id: id.into(),
            adapter,
            prompt: prompt.into(),
            input_hash_fn,
            prompt_file: None,
            outputs: vec![],
            outputs_fn: None,
            cwd: None,
            env: BTreeMap::new(),
            log_path: None,
            prompt_artifact_path: None,
            response_output_path: None,
            timeout_s: None,
            max_retries: 0,
            deps: vec![],
            resource: None,
            cacheable: true,
            deterministic_verify_fn: None,
            evaluator: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentOutput {
    pub text: String,
    pub usage: AgentUsage,
}

fn number(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| value[*name].as_u64())
}

fn reported_usage(event: &Value) -> AgentUsage {
    let usage = event
        .get("usage")
        .or_else(|| event.get("tokens"))
        .unwrap_or(event);
    let usage = usage
        .get("tokens")
        .filter(|v| v.is_object())
        .unwrap_or(usage);
    let input = number(usage, &["input_tokens", "prompt_tokens", "input"]);
    let output = number(usage, &["output_tokens", "completion_tokens", "output"]);
    // A total-only counter has no direction. Preserve it once in the input bucket.
    let total_only = if input.is_none() && output.is_none() {
        usage
            .as_u64()
            .or_else(|| number(usage, &["tokens", "total_tokens", "total"]))
    } else {
        None
    };
    let inclusive_cache = number(usage, &["cached_input_tokens"])
        .or_else(|| {
            usage
                .pointer("/input_tokens_details/cached_tokens")?
                .as_u64()
        })
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")?
                .as_u64()
        });
    let cached_tokens = inclusive_cache
        .or_else(|| {
            number(
                usage,
                &[
                    "cache_read_input_tokens",
                    "cache_read_tokens",
                    "cached_tokens",
                    "cached",
                ],
            )
        })
        .unwrap_or_default();
    let non_cached_input = if let Some(input) = input {
        input.saturating_sub(inclusive_cache.unwrap_or_default())
    } else {
        total_only.unwrap_or_default().saturating_sub(cached_tokens)
    };
    AgentUsage {
        model: event["model"]
            .as_str()
            .or(usage["model"].as_str())
            .or_else(|| {
                let models = event["modelUsage"].as_object()?;
                (models.len() == 1).then(|| models.keys().next().unwrap().as_str())
            })
            .map(str::to_owned),
        provider: event["provider"]
            .as_str()
            .or(usage["provider"].as_str())
            .map(str::to_owned),
        input_tokens: non_cached_input
            .saturating_add(number(usage, &["cache_creation_input_tokens"]).unwrap_or_default()),
        output_tokens: output.unwrap_or(0),
        cached_tokens,
        cost_usd: [event, usage]
            .into_iter()
            .find_map(|v| v["total_cost_usd"].as_f64().or(v["cost_usd"].as_f64()))
            .unwrap_or_default(),
        tool_calls: number(usage, &["tool_calls"])
            .or_else(|| number(event, &["tool_calls"]))
            .unwrap_or_default(),
    }
}

fn message_text(message: &Value) -> Option<String> {
    if let Some(text) = message.as_str().or(message["content"].as_str()) {
        return Some(text.to_owned());
    }
    let parts: Vec<_> = message["content"]
        .as_array()?
        .iter()
        .filter(|part| matches!(part["type"].as_str(), Some("text" | "output_text")))
        .filter_map(|part| part["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn event_text(event: &Value) -> Option<String> {
    if matches!(event["type"].as_str(), Some("user" | "tool" | "system")) {
        return None;
    }
    if event.get("approved").is_some() {
        return Some(event.to_string());
    }
    let item = &event["item"];
    if item["type"] == "agent_message" {
        return item["text"].as_str().map(str::to_owned);
    }
    if let Some(message) = event
        .get("message")
        .or_else(|| event.pointer("/choices/0/message"))
    {
        if event["type"] == "assistant" || message["role"] == "assistant" || message.is_string() {
            return message_text(message);
        }
    }
    for name in ["result", "response", "output_text", "text", "output"] {
        if let Some(text) = event[name].as_str() {
            return Some(text.to_owned());
        }
    }
    // Custom adapters can return a structured verdict inside their result envelope.
    event
        .get("result")
        .filter(|result| result.is_object())
        .map(Value::to_string)
}

/// Extract the last assistant answer and reported usage. Preserve unstructured stdout verbatim.
pub fn parse_agent_output(stdout: &str) -> AgentOutput {
    let mut answer = None;
    let mut total = AgentUsage::default();
    let mut observed_calls: u64 = 0;
    for event in serde_json::Deserializer::from_str(stdout).into_iter::<Value>() {
        let Ok(event @ Value::Object(_)) = event else {
            // Mixed diagnostics and prose remain intact, even after a valid JSON line.
            return AgentOutput {
                text: stdout.to_owned(),
                usage: AgentUsage::default(),
            };
        };
        let event = if event["event"] == "result" && event["result"].is_object() {
            &event["result"]
        } else {
            &event
        };
        if let Some(text) = event_text(event) {
            answer = Some(text);
        }
        let message = &event["message"];
        let mut usage = reported_usage(event);
        if event["type"] == "assistant" && message.is_object() {
            usage.add_assign(&reported_usage(message));
            observed_calls += message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "tool_use")
                .count() as u64;
        }
        if event["type"] == "item.completed"
            && matches!(
                event["item"]["type"].as_str(),
                Some("command_execution" | "mcp_tool_call" | "web_search" | "file_change")
            )
        {
            observed_calls += 1;
        }
        // Claude's terminal result reports cumulative usage across earlier assistant events.
        if event["type"] == "result" && event["usage"].is_object() {
            usage.provider = usage.provider.or_else(|| total.provider.take());
            usage.model = usage.model.or_else(|| total.model.take());
            total = usage;
        } else {
            total.add_assign(&usage);
        }
    }
    if total.tool_calls == 0 {
        total.tool_calls = observed_calls;
    }
    AgentOutput {
        text: answer.unwrap_or_else(|| stdout.to_owned()),
        usage: total,
    }
}

struct Invocation<'a> {
    prompt: &'a str,
    prompt_artifact: Option<&'a Path>,
    cwd: &'a Path,
    attempt: usize,
    env: &'a BTreeMap<String, String>,
    timeout_s: Option<f64>,
    log_path: Option<&'a Path>,
}

struct Reply {
    output: AgentOutput,
    code: i32,
    stderr: String,
}

fn expand(arg: &str, bindings: &[(&str, &str)]) -> String {
    let mut rest = arg;
    let mut result = String::new();
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some((key, replacement)) = bindings.iter().find(|(key, _)| rest.starts_with(key)) {
            result.push_str(replacement);
            rest = &rest[key.len()..];
        } else {
            result.push_str("{{");
            rest = &rest[2..];
        }
    }
    result.push_str(rest);
    result
}

impl AgentAdapter {
    fn dispatch(&self, call: Invocation<'_>) -> Result<Reply, TaskRunError> {
        let scratch = tempfile::tempdir().map_err(|e| TaskRunError::other(e.to_string()))?;
        let prompt_file = call
            .prompt_artifact
            .map(|p| call.cwd.join(p))
            .unwrap_or_else(|| scratch.path().join("prompt.txt"));
        let output_file = scratch.path().join("response.txt");
        write_text(&prompt_file, call.prompt).map_err(TaskRunError::other)?;
        let (argv, provider, model) = match self {
            Self::Custom { argv } => {
                let bindings = [
                    ("{{prompt}}", call.prompt),
                    ("{{prompt_file}}", &prompt_file.to_string_lossy()),
                    ("{{output_file}}", &output_file.to_string_lossy()),
                    ("{{cwd}}", &call.cwd.to_string_lossy()),
                    ("{{attempt}}", &call.attempt.to_string()),
                ];
                (
                    argv.iter().map(|arg| expand(arg, &bindings)).collect(),
                    "custom",
                    None,
                )
            }
            Self::Codex {
                bin,
                model,
                extra_args,
            }
            | Self::Claude {
                bin,
                model,
                extra_args,
            }
            | Self::Agy {
                bin,
                model,
                extra_args,
            } => {
                let (provider, flags): (&str, &[&str]) = match self {
                    Self::Codex { .. } => ("codex", &["exec", "--json"]),
                    Self::Claude { .. } => ("claude", &["-p", "--output-format", "json"]),
                    Self::Agy { .. } => ("agy", &["--output-format", "json"]),
                    Self::Custom { .. } => unreachable!(),
                };
                let mut argv = vec![bin.as_deref().unwrap_or(provider).to_owned()];
                argv.extend(flags.iter().map(|flag| (*flag).to_owned()));
                if let Some(model) = model {
                    argv.extend(["--model".to_owned(), model.clone()]);
                }
                argv.extend(extra_args.iter().cloned());
                argv.push(
                    if matches!(self, Self::Agy { .. }) {
                        "-p"
                    } else {
                        "--"
                    }
                    .to_owned(),
                );
                argv.push(call.prompt.to_owned());
                (argv, provider, model.clone())
            }
        };
        let log_path = call.log_path.map(|path| call.cwd.join(path));
        let process = run_command_streaming(
            &CommandSpec::Args(argv),
            Some(call.cwd),
            call.env,
            call.timeout_s,
            log_path.as_deref(),
        )?;
        let mut output = parse_agent_output(&process.stdout);
        output
            .usage
            .provider
            .get_or_insert_with(|| provider.to_owned());
        output.usage.model = output.usage.model.or(model);
        if output_file.exists() && process.returncode == 0 {
            output.text = fs::read_to_string(output_file)
                .map_err(|e| TaskRunError::other(format!("Reading adapter output: {e}")))?;
        }
        Ok(Reply {
            output,
            code: process.returncode,
            stderr: process.stderr,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Verdict {
    approved: bool,
    error_signature: Option<String>,
    feedback: Option<String>,
}

fn verdict(text: &str) -> Option<Verdict> {
    fn from_object(value: Value) -> Option<Verdict> {
        if !value.is_object() {
            return None;
        }
        serde_json::from_value(value).ok()
    }
    if let Ok(value) = serde_json::from_str(text) {
        return from_object(value);
    }
    let (before, fenced) = text.split_once("```json")?;
    let (body, after) = fenced.split_once("```")?;
    if before.contains("```") || after.contains("```") {
        return None;
    }
    serde_json::from_str(body.trim()).ok().and_then(from_object)
}

const FEEDBACK: &str = "__kgraph_agent_feedback";

fn update_feedback(ctx: &Context, id: &str, feedback: Option<Value>) {
    let mut map = ctx.0.write().unwrap_or_else(|e| e.into_inner());
    if let Some(feedback) = feedback {
        let entries = map.entry(FEEDBACK).or_insert_with(|| json!({}));
        if !entries.is_object() {
            *entries = json!({});
        }
        entries
            .as_object_mut()
            .unwrap()
            .insert(id.to_owned(), feedback);
    } else if let Some(entries) = map.get_mut(FEEDBACK).and_then(Value::as_object_mut) {
        entries.remove(id);
    }
}

fn check_process(reply: &Reply, role: &str) -> VerifyOutcome {
    if reply.code == 0 {
        VerifyOutcome::pass("Agent completed")
    } else {
        let message = if reply.stderr.trim().is_empty() {
            &reply.output.text
        } else {
            &reply.stderr
        };
        VerifyOutcome::fail(
            format!("{role}_exit_{}", reply.code),
            format!("{role} exited with code {}: {}", reply.code, message.trim()),
        )
    }
}

fn dispatch_error(error: TaskRunError) -> VerifyOutcome {
    match error {
        TaskRunError::Timeout(detail) => VerifyOutcome::fail("timeout", detail),
        TaskRunError::Other { signature, detail } => VerifyOutcome::fail(signature, detail),
        TaskRunError::Graph(failure) => VerifyOutcome::fail("agent_error", failure.to_string()),
    }
}

fn evaluate(
    config: &EvaluatorConfig,
    call: Invocation<'_>,
    ctx: &Context,
    id: &str,
    result: &mut Value,
    usage: &mut AgentUsage,
) -> VerifyOutcome {
    result["evaluator"] = json!({"exit_code": null});
    let reply = match config.adapter.dispatch(call) {
        Ok(reply) => reply,
        Err(e) => {
            // A failed dispatch can return before the evaluator reports any usage.
            result["__kgraph_usage_records"] = json!([usage]);
            return dispatch_error(e);
        }
    };
    result["__kgraph_usage_records"] = json!([usage, reply.output.usage]);
    usage.add_assign(&reply.output.usage);
    result["evaluator"] = json!({"exit_code": reply.code, "text": redact_secrets(&reply.output.text), "__kgraph_usage": reply.output.usage});
    if reply.code != 0 {
        return check_process(&reply, "evaluator");
    }
    let Some(mut review) = verdict(&reply.output.text) else {
        return VerifyOutcome::fail(
            "evaluator_invalid_verdict",
            "Evaluator must return a JSON object with a boolean approved field",
        );
    };
    let feedback = review.feedback.as_deref().map(redact_secrets);
    let signature = review.error_signature.as_deref().map(redact_secrets);
    if review.feedback != feedback || review.error_signature != signature {
        // JSON escapes can hide a credential in raw text. Re-encode the
        // sanitized verdict in that case, preserving clean replies verbatim.
        review.feedback = feedback;
        review.error_signature = signature;
        result["evaluator"]["text"] =
            json!(serde_json::to_string(&review).expect("serializable verdict"));
    }
    result["evaluator"]["approved"] = json!(review.approved);
    result["evaluator"]["feedback"] = json!(review.feedback);
    if review.approved {
        update_feedback(ctx, id, None);
        return VerifyOutcome::pass("Evaluator approved");
    }
    let sig = review
        .error_signature
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "evaluator_rejected".into());
    let detail = review
        .feedback
        .unwrap_or_else(|| "Evaluator rejected the response".into());
    result["evaluator"]["error_signature"] = json!(sig);
    update_feedback(
        ctx,
        id,
        Some(json!({"error_signature": sig, "feedback": detail})),
    );
    VerifyOutcome::fail(sig, detail)
}

fn working_directory(cwd: Option<&Path>) -> Result<PathBuf, String> {
    std::path::absolute(cwd.unwrap_or_else(|| Path::new("."))).map_err(|e| e.to_string())
}

fn attempt(
    opts: &AgentTaskOptions,
    outputs_fn: &OutputsFn,
    ctx: &Context,
    count: usize,
) -> TaskResult {
    let cwd = working_directory(opts.cwd.as_deref()).map_err(TaskRunError::other)?;
    let outputs = outputs_fn(ctx).map_err(TaskRunError::other)?;
    let mut prompt = match &opts.prompt_file {
        Some(path) => fs::read_to_string(cwd.join(path))
            .map_err(|e| TaskRunError::other(format!("Reading prompt {}: {e}", path.display())))?,
        None => opts.prompt.clone(),
    };
    if !outputs.is_empty() {
        prompt.push_str("\n\nExpected outputs:");
        for path in &outputs {
            prompt.push_str(&format!("\n- {}", path.display()));
        }
    }
    let node_path = crate::engine::active_node_path().unwrap_or_else(|| opts.id.clone());
    for (key, title) in [
        ("__kgraph_repair", "Repair context"),
        (FEEDBACK, "Prior evaluator critique"),
    ] {
        if let Some(context) = ctx.get(key).and_then(|v| v.get(&node_path).cloned()) {
            prompt.push_str(&format!("\n\n{title}:\n{context}"));
        }
    }

    let mut usage = AgentUsage::default();
    let mut result =
        json!({"text": "", "attempt": count, "exit_code": null, "deterministic_passed": false});
    let call = Invocation {
        prompt: &prompt,
        prompt_artifact: opts.prompt_artifact_path.as_deref(),
        cwd: &cwd,
        attempt: count,
        env: &opts.env,
        timeout_s: opts.timeout_s,
        log_path: opts.log_path.as_deref(),
    };
    let mut outcome = match opts.adapter.dispatch(call) {
        Err(e) => dispatch_error(e),
        Ok(reply) => {
            let mut outcome = check_process(&reply, "agent");
            result["text"] = json!(reply.output.text);
            result["exit_code"] = json!(reply.code);
            usage = reply.output.usage;
            if outcome.ok {
                if let Some(path) = &opts.response_output_path {
                    if let Err(e) = write_text(&cwd.join(path), &reply.output.text) {
                        outcome = VerifyOutcome::fail("agent_response_write", e);
                    }
                }
            }
            outcome
        }
    };
    result["__kgraph_usage"] = json!(usage);
    if outcome.ok {
        if let Some(path) = outputs.iter().find(|p| !p.exists()) {
            outcome = VerifyOutcome::fail(
                "missing_output",
                format!("Required output is missing: {}", path.display()),
            );
        } else if let Some(verify) = &opts.deterministic_verify_fn {
            outcome = verify(ctx, &result);
            result["deterministic_passed"] = json!(outcome.ok);
        }
    }
    if outcome.ok {
        if let Some(config) = &opts.evaluator {
            let review_prompt = format!("{}\n\nGenerator task:\n{prompt}\n\nGenerator response:\n{}\n\nReturn a JSON object with boolean approved and optional string error_signature and feedback.", config.prompt, result["text"].as_str().unwrap_or_default());
            let mut env = opts.env.clone();
            env.extend(config.env.clone());
            outcome = evaluate(
                config,
                Invocation {
                    prompt: &review_prompt,
                    prompt_artifact: None,
                    cwd: &cwd,
                    attempt: count,
                    env: &env,
                    timeout_s: config.timeout_s.or(opts.timeout_s),
                    log_path: config.log_path.as_deref(),
                },
                ctx,
                &node_path,
                &mut result,
                &mut usage,
            );
            outcome.error_signature = redact_secrets(&outcome.error_signature);
            outcome.detail = redact_secrets(&outcome.detail);
            result["__kgraph_usage"] = json!(usage);
        }
    }
    result["verification"] = json!({"ok": outcome.ok, "error_signature": outcome.error_signature, "detail": outcome.detail});
    Ok(result)
}

pub fn agent_task(opts: AgentTaskOptions) -> TaskNode {
    let static_outputs = opts.outputs.clone();
    let dynamic = opts.outputs_fn.clone();
    let response = opts.response_output_path.clone();
    let cwd = opts.cwd.clone();
    let outputs: OutputsFn = Arc::new(move |ctx| {
        let cwd = working_directory(cwd.as_deref())?;
        let extra = dynamic
            .as_ref()
            .map(|f| f(ctx))
            .transpose()?
            .unwrap_or_default();
        let mut paths = Vec::new();
        for p in static_outputs.iter().chain(&extra).chain(&response) {
            let path = cwd.join(p);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    });
    let run_outputs = outputs.clone();
    let id = opts.id.clone();
    let input = opts.input_hash_fn.clone();
    let opts = Arc::new(opts);
    let run_options = opts.clone();
    let mut node = TaskNode::new(
        id,
        TaskKind::Worker,
        Arc::new(move |ctx, index| attempt(&run_options, &run_outputs, ctx, index + 1)),
        input,
    );
    node.outputs_fn = outputs;
    node.deps = opts.deps.clone();
    node.resource = opts.resource.clone();
    node.max_retries = opts.max_retries;
    node.cacheable = opts.cacheable;
    // Each subprocess enforces its own timeout and reaps its children before returning.
    node.verify_fn = Some(Arc::new(|_, result| {
        let check = &result["verification"];
        let detail = check["detail"]
            .as_str()
            .unwrap_or("Missing agent verification");
        if check["ok"] == true {
            VerifyOutcome::pass(detail)
        } else {
            VerifyOutcome::fail(
                check["error_signature"]
                    .as_str()
                    .unwrap_or("invalid_agent_result"),
                detail,
            )
        }
    }));
    node.efficiency_fn = Some(Arc::new(|result| EfficiencyCounts {
        agent_dispatches: 1 + usize::from(result.get("evaluator").is_some()),
        script_verifiers_passed: usize::from(result["deterministic_passed"] == true),
        ..Default::default()
    }));
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{GraphRunner, RunnerConfig};
    use crate::types::Node;
    use tempfile::tempdir;

    #[test]
    fn parses_codex_jsonl_and_counts_only_completed_tool_calls() {
        let stream = r#"{"type":"item.started","item":{"type":"command_execution","id":"t"}}
{"type":"item.completed","item":{"type":"command_execution","id":"t"}}
{"type":"item.completed","item":{"type":"agent_message","text":"draft"}}
{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,"output_tokens":3}}
{"type":"item.completed","item":{"type":"agent_message","text":"answer"}}
{"type":"turn.completed","usage":{"input_tokens":8,"cached_input_tokens":2,"output_tokens":5}}"#;
        let output = parse_agent_output(stream);
        assert_eq!(output.text, "answer");
        assert_eq!(
            output.usage,
            AgentUsage {
                input_tokens: 12,
                cached_tokens: 6,
                output_tokens: 8,
                tool_calls: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn inclusive_openai_cache_fields_and_anthropic_cache_buckets() {
        for usage in [
            json!({"input_tokens":12,"cached_input_tokens":5,"output_tokens":3}),
            json!({"input_tokens":12,"input_tokens_details":{"cached_tokens":5},"output_tokens":3}),
            json!({"prompt_tokens":12,"prompt_tokens_details":{"cached_tokens":5},"completion_tokens":3}),
            json!({"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":5,"output_tokens":3}),
        ] {
            let parsed = parse_agent_output(&json!({"usage":usage}).to_string()).usage;
            assert_eq!(
                (
                    parsed.input_tokens,
                    parsed.cached_tokens,
                    parsed.total_tokens()
                ),
                (7, 5, 15)
            );
        }
    }

    #[test]
    fn claude_result_supersedes_message_usage_and_retains_model() {
        let terminal = r#"{"type":"result","result":"final","total_cost_usd":0.25,"usage":{"input_tokens":7,"cache_read_input_tokens":5,"output_tokens":4},"modelUsage":{"sonnet":{}}}"#;
        let single = parse_agent_output(terminal);
        let stream = format!(
            "{}\n{terminal}",
            r#"{"type":"assistant","message":{"model":"sonnet","content":[{"type":"tool_use"}],"usage":{"input_tokens":7,"output_tokens":1}}}"#
        );
        let parsed = parse_agent_output(&stream);
        assert_eq!(single.text, "final");
        assert_eq!(parsed.text, single.text);
        assert_eq!(
            parsed.usage,
            AgentUsage {
                model: Some("sonnet".into()),
                input_tokens: 7,
                cached_tokens: 5,
                output_tokens: 4,
                cost_usd: 0.25,
                tool_calls: 1,
                ..Default::default()
            }
        );
        assert_eq!(single.usage.input_tokens, parsed.usage.input_tokens);
    }

    #[test]
    fn parses_generic_usage_agy_and_unstructured_stdout() {
        let parsed = parse_agent_output(
            r#"{"text":"ok","provider":"local","model":"demo","tokens":{"input":12,"output":4,"cached":2},"tool_calls":3,"cost_usd":0.5}"#,
        );
        assert_eq!(parsed.text, "ok");
        assert_eq!(
            parsed.usage,
            AgentUsage {
                provider: Some("local".into()),
                model: Some("demo".into()),
                input_tokens: 12,
                output_tokens: 4,
                cached_tokens: 2,
                tool_calls: 3,
                cost_usd: 0.5
            }
        );
        assert_eq!(
            parse_agent_output(r#"{"tokens":23}"#).usage.total_tokens(),
            23
        );
        let agy = parse_agent_output(
            r#"{"event":"result","result":{"response":"done","usage":{"cache_read_tokens":9}}}"#,
        );
        assert_eq!(agy.text, "done");
        assert_eq!(agy.usage.cached_tokens, 9);
        for text in [
            "plain text\n",
            "{partial JSON",
            "log\n{\"result\":\"hidden\"}\n",
        ] {
            assert_eq!(
                parse_agent_output(text),
                AgentOutput {
                    text: text.into(),
                    ..Default::default()
                }
            );
        }
    }

    fn python(script: &str, args: &[&str]) -> AgentAdapter {
        AgentAdapter::Custom {
            argv: ["python3", "-c", script]
                .into_iter()
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect(),
        }
    }

    fn options(dir: &Path, adapter: AgentAdapter) -> AgentTaskOptions {
        let mut opts = AgentTaskOptions::new(
            "generate",
            adapter,
            "Write an answer.",
            Arc::new(|_| Ok("input".into())),
        );
        opts.cwd = Some(dir.to_owned());
        opts.cacheable = false;
        opts
    }

    fn evaluator(adapter: AgentAdapter) -> EvaluatorConfig {
        EvaluatorConfig {
            adapter,
            prompt: "Review the answer".into(),
            timeout_s: Some(5.0),
            log_path: None,
            env: BTreeMap::new(),
        }
    }

    fn runner(dir: &Path) -> GraphRunner {
        GraphRunner::new(RunnerConfig::new(dir.join("state.json"))).unwrap()
    }

    fn verified(node: &TaskNode, ctx: &Context, result: &Value) -> VerifyOutcome {
        node.verify_fn.as_ref().unwrap()(ctx, result)
    }

    #[test]
    fn custom_adapter_uses_literal_arguments_and_cleans_scratch_files() {
        let dir = tempdir().unwrap();
        let script = r#"import json, os, pathlib, sys
prompt, artifact, output, cwd, attempt = sys.argv[1:]
assert prompt == pathlib.Path(artifact).read_text()
assert os.path.samefile(cwd, os.getcwd())
assert attempt == 'attempt=1'
assert ('literal {{' + 'attempt}}; $(touch injected)') in prompt and 'repair this' in prompt
assert 'dynamic.txt' in prompt and 'static.txt' in prompt
pathlib.Path('record').write_text(json.dumps([artifact, output]))
for path in ('static.txt','dynamic.txt'): pathlib.Path(path).touch()
pathlib.Path(output).write_text('agent answer')
print(json.dumps({'usage':{'input_tokens':12,'output_tokens':3},'text':'staging takes precedence'}))"#;
        let mut opts = options(
            dir.path(),
            python(
                script,
                &[
                    "{{prompt}}",
                    "{{prompt_file}}",
                    "{{output_file}}",
                    "{{cwd}}",
                    "attempt={{attempt}}",
                ],
            ),
        );
        fs::write(
            dir.path().join("source"),
            "literal {{attempt}}; $(touch injected)",
        )
        .unwrap();
        opts.prompt_file = Some("source".into());
        opts.outputs = vec!["static.txt".into()];
        opts.outputs_fn = Some(Arc::new(|_| Ok(vec!["dynamic.txt".into()])));
        opts.response_output_path = Some("nested/answer".into());
        opts.log_path = Some("agent.log".into());
        let ctx = Context::new();
        ctx.insert("__kgraph_repair", json!({"generate":"repair this"}));
        let node = agent_task(opts);
        let result = (node.run_fn)(&ctx, 0).unwrap();
        assert!(verified(&node, &ctx, &result).ok, "{result}");
        assert_eq!(result["attempt"], 1);
        assert_eq!(result["text"], "agent answer");
        assert_eq!(result["__kgraph_usage"]["provider"], "custom");
        assert_eq!(result["__kgraph_usage"]["input_tokens"], 12);
        assert_eq!(result["__kgraph_usage"]["output_tokens"], 3);
        assert_eq!(
            fs::read_to_string(dir.path().join("nested/answer")).unwrap(),
            "agent answer"
        );
        assert!(!dir.path().join("injected").exists());
        assert!(fs::read_to_string(dir.path().join("agent.log"))
            .unwrap()
            .contains("input_tokens"));
        for path in serde_json::from_str::<Vec<PathBuf>>(
            &fs::read_to_string(dir.path().join("record")).unwrap(),
        )
        .unwrap()
        {
            assert!(!path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn named_cli_adapters_pass_prompt_as_one_argument_and_write_response() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let bin = dir.path().join("mock agent");
        fs::write(&bin, "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps({'result':json.dumps(sys.argv[1:]),'usage':{'output_tokens':2}}))\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let settings = || {
            (
                Some(bin.to_str().unwrap().to_owned()),
                Some("chosen-model".into()),
                vec!["--test-extra".into()],
            )
        };
        let (bin, model, extra_args) = settings();
        let codex = AgentAdapter::Codex {
            bin,
            model,
            extra_args,
        };
        let (bin, model, extra_args) = settings();
        let claude = AgentAdapter::Claude {
            bin,
            model,
            extra_args,
        };
        let (bin, model, extra_args) = settings();
        let agy = AgentAdapter::Agy {
            bin,
            model,
            extra_args,
        };
        for (adapter, provider, prefix, separator) in [
            (codex, "codex", vec!["exec", "--json"], "--"),
            (
                claude,
                "claude",
                vec!["-p", "--output-format", "json"],
                "--",
            ),
            (agy, "agy", vec!["--output-format", "json"], "-p"),
        ] {
            let mut opts = options(dir.path(), adapter);
            opts.prompt = "-x 'quoted'\n$(touch danger)".into();
            opts.response_output_path = Some("answer".into());
            opts.prompt_artifact_path = Some("prompt".into());
            let node = agent_task(opts);
            let ctx = Context::new();
            let result = (node.run_fn)(&ctx, 0).unwrap();
            assert!(verified(&node, &ctx, &result).ok, "{result}");
            let prompt = fs::read_to_string(dir.path().join("prompt")).unwrap();
            let answer = fs::read_to_string(dir.path().join("answer")).unwrap();
            assert_eq!(result["text"], answer);
            let args: Vec<String> = serde_json::from_str(&answer).unwrap();
            assert_eq!(
                args,
                [
                    prefix,
                    vec![
                        "--model",
                        "chosen-model",
                        "--test-extra",
                        separator,
                        &prompt
                    ]
                ]
                .concat()
            );
            assert_eq!(result["__kgraph_usage"]["provider"], provider);
            assert_eq!(result["__kgraph_usage"]["model"], "chosen-model");
            assert_eq!(result["__kgraph_usage"]["output_tokens"], 2);
            assert!(!dir.path().join("danger").exists());
        }
    }

    #[test]
    fn deterministic_failure_short_circuits_evaluator() {
        let dir = tempdir().unwrap();
        let mut opts = options(dir.path(), python("print('draft')", &[]));
        opts.deterministic_verify_fn = Some(Arc::new(|_, result| {
            assert_eq!(result["text"], "draft\n");
            VerifyOutcome::fail("lint_failed", "missing newline in file")
        }));
        opts.evaluator = Some(evaluator(python("open('evaluated','w').close()", &[])));
        let runner = runner(dir.path());
        let error = runner
            .run(&[Node::Task(agent_task(opts))], &Context::new(), "")
            .unwrap_err();
        assert!(!error.circuit_broken);
        assert_eq!(
            runner.state().nodes["generate"]["error_signature"],
            "lint_failed"
        );
        assert_eq!(runner.efficiency().agent_dispatches, 1);
        assert_eq!(runner.efficiency().script_verifiers_passed, 0);
        assert!(!dir.path().join("evaluated").exists());
    }

    #[test]
    fn missing_output_short_circuits_both_checks() {
        let dir = tempdir().unwrap();
        let mut opts = options(dir.path(), python("print('ok')", &[]));
        opts.outputs = vec!["absent".into()];
        opts.deterministic_verify_fn = Some(Arc::new(|_, _| panic!("unexpected verify")));
        opts.evaluator = Some(evaluator(python("raise RuntimeError()", &[])));
        let node = agent_task(opts);
        let ctx = Context::new();
        let result = (node.run_fn)(&ctx, 0).unwrap();
        assert_eq!(
            verified(&node, &ctx, &result).error_signature,
            "missing_output"
        );
        assert!(result.get("evaluator").is_none());
    }

    #[test]
    fn graph_retries_with_critique_then_approves() {
        let dir = tempdir().unwrap();
        let generate = r#"import json,pathlib,sys
prompt = pathlib.Path(sys.argv[1]).read_text()
pathlib.Path('prompt-' + sys.argv[2]).write_text(prompt)
answer = 'corrected' if 'handle empty input' in prompt and 'missing_edge_case' in prompt else 'draft'
print(json.dumps({'result':answer, 'provider':'writer','model':'draft','tokens':{'input':10,'output':4}, 'cost_usd':0.5}))"#;
        let review = r#"import json,os,sys
assert os.environ['STAGE'] == 'review' and os.environ['SHARED'] == 'yes'
ok = 'Generator response:\ncorrected' in sys.argv[1]
verdict = {'approved': ok, 'error_signature': None if ok else 'missing_edge_case sk-sensitive', 'feedback': None if ok else 'handle empty input Bearer abc123'}
print(json.dumps({'result':'```json\n' + json.dumps(verdict) + '\n```', 'provider':'reviewer','model':'critic','usage':{'input_tokens':5,'output_tokens':2},'total_cost_usd':0.25}))"#;
        let mut opts = options(
            dir.path(),
            python(generate, &["{{prompt_file}}", "{{attempt}}"]),
        );
        opts.response_output_path = Some("answer".into());
        opts.env.extend([
            ("SHARED".into(), "yes".into()),
            ("STAGE".into(), "generate".into()),
        ]);
        opts.max_retries = 2;
        opts.deterministic_verify_fn = Some(Arc::new(|_, result| {
            assert_eq!(result["__kgraph_usage"]["input_tokens"], 10);
            VerifyOutcome::pass("valid")
        }));
        let mut config = evaluator(python(review, &["{{prompt}}"]));
        config.env.insert("STAGE".into(), "review".into());
        opts.evaluator = Some(config);
        let mut node = agent_task(opts);
        // Observe the actual attempt results the runner receives.
        let verify = node.verify_fn.take().unwrap();
        node.verify_fn = Some(Arc::new(move |ctx, result| {
            ctx.insert(format!("attempt-{}", result["attempt"]), result.clone());
            verify(ctx, result)
        }));
        let runner = runner(dir.path());
        let ctx = Context::new();
        ctx.insert(
            FEEDBACK,
            json!({"sibling":{"feedback":"private sibling feedback"}}),
        );
        runner.run(&[Node::Task(node)], &ctx, "").unwrap();
        assert_eq!(runner.state().nodes["generate"]["attempts"], 2);
        assert_eq!(runner.efficiency().agent_dispatches, 4);
        assert_eq!(runner.efficiency().script_verifiers_passed, 2);
        assert_eq!(
            fs::read_to_string(dir.path().join("answer")).unwrap(),
            "corrected"
        );
        let first = fs::read_to_string(dir.path().join("prompt-1")).unwrap();
        let second = fs::read_to_string(dir.path().join("prompt-2")).unwrap();
        assert!(!first.contains("Prior evaluator critique"));
        assert!(second.contains("handle empty input") && !second.contains("private sibling"));
        assert_eq!(
            ctx.get(FEEDBACK).unwrap(),
            json!({"sibling":{"feedback":"private sibling feedback"}})
        );
        assert!(second.contains("[redacted]"));
        for evidence in [&second, &ctx.get("attempt-1").unwrap().to_string()] {
            assert!(!evidence.contains("abc123") && !evidence.contains("sk-sensitive"));
        }
        let run = runner.build_run_telemetry("passed", 0, None);
        assert_eq!(
            run.nodes[0].error_signatures,
            ["missing_edge_case [redacted]"]
        );
        assert_eq!(run.total_tokens, 42);
        let analysis = crate::telemetry::analyze_telemetry(&[run]);
        for (key, tokens, cost) in [("writer/draft", 28, 1.), ("reviewer/critic", 14, 0.5)] {
            let model = &analysis.models[key];
            assert_eq!(
                (
                    model.calls,
                    model.tokens,
                    model.cost_usd,
                    model.failure_burn_usd
                ),
                (2, tokens, cost, cost / 2.)
            );
        }
        for (key, approved) in [("attempt-1", false), ("attempt-2", true)] {
            let result = ctx.get(key).unwrap();
            assert_eq!(result["evaluator"]["approved"], approved);
            let records = result["__kgraph_usage_records"].as_array().unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(
                (&records[0]["model"], &records[1]["model"]),
                (&json!("draft"), &json!("critic"))
            );
            assert_eq!(result["__kgraph_usage"]["input_tokens"], 15);
            assert_eq!(result["__kgraph_usage"]["output_tokens"], 6);
            assert_eq!(result["__kgraph_usage"]["cost_usd"], 0.75);
        }
    }

    #[test]
    fn parallel_subgraphs_isolate_feedback_and_repair_for_identical_child_ids() {
        use crate::repair::{attach_repair_context, build_repair_context, get_repair_context};
        let dir = tempdir().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut stages = Vec::new();
        for branch in ["left", "right"] {
            let cwd = dir.path().join(branch);
            fs::create_dir(&cwd).unwrap();
            let mut opts = options(&cwd, python("print('draft')", &[]));
            opts.id = "agent".into();
            opts.max_retries = 1;
            opts.prompt_artifact_path = Some("prompt".into());
            opts.evaluator = Some(evaluator(python(
                "import json,sys; print(json.dumps({'approved':sys.argv[1]=='2','feedback':'critique-'+sys.argv[2]}))",
                &["{{attempt}}", branch],
            )));
            let mut task = agent_task(opts);
            task.timeout_s = Some(10.); // The callback runs on a separate thread.
            let barrier = barrier.clone();
            task.repair_fn = Some(Arc::new(move |ctx, attempt, signature, detail| {
                let repair = build_repair_context(
                    "agent",
                    attempt,
                    1,
                    signature,
                    &format!("repair-{branch}: {detail}"),
                    None,
                    None,
                );
                attach_repair_context(ctx, &repair);
                // Both first-attempt critiques must coexist before either agent retries.
                barrier.wait();
                Ok(())
            }));
            stages.push(Node::Subgraph(crate::types::SubgraphNode::new(
                branch,
                vec![Node::Task(task)],
            )));
        }
        let ctx = Context::new();
        let runner = GraphRunner::new(RunnerConfig {
            max_parallel_nodes: 2,
            ..RunnerConfig::new(dir.path().join("state.json"))
        })
        .unwrap();
        runner.run(&stages, &ctx, "").unwrap();
        for (branch, other) in [("left", "right"), ("right", "left")] {
            let prompt = fs::read_to_string(dir.path().join(branch).join("prompt")).unwrap();
            let critique = prompt.split_once("Prior evaluator critique").unwrap().1;
            assert!(critique.contains(&format!("critique-{branch}")));
            assert!(prompt.contains(&format!("repair-{branch}")));
            assert!(
                !prompt.contains(&format!("critique-{other}"))
                    && !prompt.contains(&format!("repair-{other}"))
            );
            let path = format!("{branch}/agent");
            assert_eq!(get_repair_context(&ctx, &path).unwrap().node_path, path);
        }
        assert!(get_repair_context(&ctx, "agent").is_none());
    }

    #[test]
    fn repeated_evaluator_signature_trips_circuit_breaker() {
        let dir = tempdir().unwrap();
        let mut opts = options(dir.path(), python("print('draft')", &[]));
        opts.max_retries = 4;
        opts.evaluator = Some(evaluator(python(
            r#"print('{"approved":false,"error_signature":"missing_edge_case","feedback":"try again"}')"#,
            &[],
        )));
        let runner = runner(dir.path());
        let ctx = Context::new();
        assert!(
            runner
                .run(&[Node::Task(agent_task(opts))], &ctx, "")
                .unwrap_err()
                .circuit_broken
        );
        let record = &runner.state().nodes["generate"];
        assert_eq!(record["attempts"], 2);
        assert_eq!(record["error_signature"], "missing_edge_case");
        assert_eq!(
            ctx.get(FEEDBACK).unwrap()["generate"]["feedback"],
            "try again"
        );
        assert_eq!(runner.efficiency().agent_dispatches, 4);
    }

    #[test]
    fn evaluator_rejects_invalid_verdicts_and_nonzero_exit() {
        let dir = tempdir().unwrap();
        for (script, signature) in [
            ("print('ok')", "evaluator_invalid_verdict"),
            ("print('[true,null,null]')", "evaluator_invalid_verdict"),
            (
                r#"print('{"feedback":"no decision"}')"#,
                "evaluator_invalid_verdict",
            ),
            (
                r#"print('{"approved":"true"}')"#,
                "evaluator_invalid_verdict",
            ),
            (r#"print('{"approved":false}')"#, "evaluator_rejected"),
            (
                r#"print('{"approved":true}'); raise SystemExit(9)"#,
                "evaluator_exit_9",
            ),
        ] {
            let mut opts = options(dir.path(), python("print('draft')", &[]));
            opts.evaluator = Some(evaluator(python(script, &[])));
            let node = agent_task(opts);
            let ctx = Context::new();
            let result = (node.run_fn)(&ctx, 0).unwrap();
            assert_eq!(
                verified(&node, &ctx, &result).error_signature,
                signature,
                "{result}"
            );
            assert_eq!(
                node.efficiency_fn.as_ref().unwrap()(&result).agent_dispatches,
                2
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn generator_timeout_reaps_process_and_descendant() {
        use std::time::Duration;
        let dir = tempdir().unwrap();
        let mut opts = options(
            dir.path(),
            python(
                r#"import os,pathlib,time
child = os.fork()
if child:
    pathlib.Path('started').write_text(str(os.getpid()))
time.sleep(0.9)
pathlib.Path('survived').touch()"#,
                &[],
            ),
        );
        opts.timeout_s = Some(0.5);
        opts.response_output_path = Some("answer".into());
        let runner = runner(dir.path());
        runner
            .run(&[Node::Task(agent_task(opts))], &Context::new(), "")
            .unwrap_err();
        assert_eq!(
            runner.state().nodes["generate"]["error_signature"],
            "timeout"
        );
        let pid: libc::pid_t = fs::read_to_string(dir.path().join("started"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        std::thread::sleep(Duration::from_millis(950));
        assert!(!dir.path().join("survived").exists());
        assert!(!dir.path().join("answer").exists());
    }
}
