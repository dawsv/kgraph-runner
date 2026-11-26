//! Packet, verification, and repair state shared by workflow builders.

use kgraph_core::files::{read_object, read_optional_object, write_json};
use kgraph_core::gate::{subagent_gate, ArtifactVerifierFn, PacketFn, SubagentGateOptions};
use kgraph_core::hash::{sha256_file, sha256_json};
use kgraph_core::types::{
    Context, GraphFailure, InputHashFn, LoopSubgraphNode, OutputsFn, RetryRoute, TaskKind,
    TaskNode, TaskRunError, VerifyOutcome,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type StringFn = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;
pub type BoolFn = Arc<dyn Fn() -> Result<bool, String> + Send + Sync>;
pub type CheckFn = Arc<dyn Fn(&Context) -> Result<VerifyOutcome, String> + Send + Sync>;

pub fn file_hash(path: Option<&Path>) -> Option<String> {
    let p = path?;
    if p.is_file() {
        sha256_file(p).ok()
    } else {
        None
    }
}

pub fn fingerprint(values: Value) -> String {
    sha256_json(&values)
}

pub fn files_hash(paths: &[PathBuf]) -> String {
    let entries: Vec<Value> = paths
        .iter()
        .map(|p| {
            json!([
                p.to_string_lossy().to_string(),
                file_hash(Some(p.as_path()))
            ])
        })
        .collect();
    fingerprint(json!([entries]))
}

pub fn read_optional(path: &Path) -> Result<Map<String, Value>, String> {
    read_optional_object(path)
}

pub fn revision(path: &Path) -> Result<u64, String> {
    // Delivery verifiers validate revision fields and generate repair feedback.
    // Only a readable metadata object can supply the fallback revision.
    Ok(read_optional(path)?
        .get("revision")
        .and_then(Value::as_u64)
        .unwrap_or(0))
}

#[derive(Deserialize)]
struct Receipt {
    input_hash: String,
    artifact_hash: String,
}

fn read_receipt(path: &Path) -> Result<Option<Receipt>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // Deserialize the object explicitly: serde can also decode structs from positional arrays.
    let fields: Map<String, Value> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Invalid receipt in {}: {e}", path.display()))?;
    let receipt: Receipt = serde_json::from_value(Value::Object(fields))
        .map_err(|e| format!("Invalid receipt in {}: {e}", path.display()))?;
    if receipt.input_hash.trim().is_empty()
        || receipt.artifact_hash.len() != 64
        || !receipt
            .artifact_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("Invalid receipt hashes in {}", path.display()));
    }
    Ok(Some(receipt))
}

pub fn with_retry_route(
    mut loop_node: LoopSubgraphNode,
    source: impl Into<String>,
    target: impl Into<String>,
    label: &str,
) -> LoopSubgraphNode {
    loop_node.retry_route = Some(RetryRoute {
        from: source.into(),
        to: target.into(),
        label: format!("{} (max {})", label, loop_node.max_retries),
    });
    loop_node
}

/// Persistent repair feedback and rejected-output tracker across CLI processes.
#[derive(Clone)]
pub struct Repairs {
    pub path: PathBuf,
    pub input_hash: StringFn,
    pub artifact_hash: StringFn,
    pub metadata: Option<PathBuf>,
    pub minimum_revision: u64,
}

impl Repairs {
    pub fn new(
        path: PathBuf,
        input_hash: StringFn,
        artifacts: Vec<PathBuf>,
        metadata: Option<PathBuf>,
        artifact_hash: Option<StringFn>,
        minimum_revision: u64,
    ) -> Self {
        let artifact_hash =
            artifact_hash.unwrap_or_else(|| Arc::new(move || Ok(files_hash(&artifacts))));
        Self {
            path,
            input_hash,
            artifact_hash,
            metadata,
            minimum_revision,
        }
    }

    pub fn state(&self) -> Result<Map<String, Value>, String> {
        let state = read_optional(&self.path)?;
        let expected = (self.input_hash)()?;
        if state.get("input_hash").and_then(Value::as_str) == Some(expected.as_str()) {
            Ok(state)
        } else {
            Ok(Map::new())
        }
    }

    pub fn instructions(&self) -> Result<Map<String, Value>, String> {
        let state = self.state()?;
        let rev = state
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .max(self.minimum_revision);
        let feedback = state
            .get("feedback")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mut out = Map::new();
        out.insert("revision".to_string(), json!(rev));
        out.insert("repair_feedback".to_string(), feedback);
        Ok(out)
    }

    pub fn waiting_reason(&self) -> Result<Option<String>, String> {
        let state = self.state()?;
        let expected = state
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .max(self.minimum_revision);
        if let Some(meta) = &self.metadata {
            if expected > revision(meta)? {
                let name = meta
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| meta.display().to_string());
                return Ok(Some(format!(
                    "{name} must declare revision {expected} or newer"
                )));
            }
        }
        if let Some(rejected) = state.get("rejected_hash").and_then(Value::as_str) {
            if rejected == (self.artifact_hash)()? {
                return Ok(Some(
                    "The rejected artifacts still have the same contents".to_string(),
                ));
            }
        }
        Ok(None)
    }

    #[allow(clippy::result_large_err)]
    pub fn repair(
        &self,
        ctx: &Context,
        signature: &str,
        detail: &str,
        dirty: &[&str],
        max_retries: usize,
    ) -> Result<Vec<String>, GraphFailure> {
        let state = self
            .state()
            .map_err(|e| GraphFailure::new(self.parent_label(), e))?;
        let mut feedback: Vec<Value> = state
            .get("feedback")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let unresolved = state
            .get("consecutive_repairs")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if unresolved > 0 {
            if let Some(last_sig) = feedback
                .last()
                .and_then(|v| v.get("error_signature"))
                .and_then(Value::as_str)
            {
                if last_sig == signature {
                    return Ok(Vec::new());
                }
            }
        }
        if unresolved >= max_retries {
            return Err(GraphFailure::new(
                self.parent_label(),
                format!("Exhausted {max_retries} repairs ({signature}): {detail}"),
            ));
        }
        let mut number = state
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .max(self.minimum_revision);
        if let Some(meta) = &self.metadata {
            number =
                number.max(revision(meta).map_err(|e| GraphFailure::new(self.parent_label(), e))?);
        }
        number += 1;
        feedback.push(json!({
            "revision": number,
            "error_signature": signature,
            "detail": detail,
        }));
        let mut all_feedback = ctx
            .get("repair_feedback")
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        all_feedback.insert(
            self.path.to_string_lossy().into_owned(),
            Value::Array(feedback.clone()),
        );
        ctx.insert("repair_feedback", Value::Object(all_feedback));
        let in_hash = (self.input_hash)().map_err(|e| GraphFailure::new(self.parent_label(), e))?;
        let art_hash =
            (self.artifact_hash)().map_err(|e| GraphFailure::new(self.parent_label(), e))?;
        write_json(
            &self.path,
            &json!({
                "input_hash": in_hash,
                "revision": number,
                "feedback": feedback,
                "rejected_hash": art_hash,
                "consecutive_repairs": unresolved + 1,
            }),
            true,
        )
        .map_err(|e| GraphFailure::new(self.parent_label(), e))?;
        Ok(dirty.iter().map(|s| (*s).to_string()).collect())
    }

    pub fn resolved(&self) -> Result<(), String> {
        let mut state = self.state()?;
        if state
            .get("consecutive_repairs")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        {
            state.insert("rejected_hash".to_string(), Value::Null);
            state.insert("consecutive_repairs".to_string(), json!(0));
            write_json(&self.path, &state, true)?;
        }
        Ok(())
    }

    fn parent_label(&self) -> String {
        self.path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repair".to_string())
    }
}

pub struct PacketWorkerOptions {
    pub node_id: String,
    pub packet_path: PathBuf,
    pub artifacts: OutputsFn,
    pub input_hash: StringFn,
    pub packet: PacketFn,
    pub deps: Vec<String>,
    pub repairs: Option<Repairs>,
    pub enabled: Option<BoolFn>,
    pub verify: Option<ArtifactVerifierFn>,
    pub resource: Option<String>,
}

/// Bind a completed worker delivery to the inputs it was generated for via a `.receipt.json`.
pub fn packet_worker(opts: PacketWorkerOptions) -> TaskNode {
    let receipt = opts.packet_path.with_extension("receipt.json");
    let enabled: BoolFn = opts.enabled.unwrap_or_else(|| Arc::new(|| Ok(true)));
    let repairs_verify = opts.repairs.clone();
    let receipt_verify = receipt.clone();
    let input_hash_verify = opts.input_hash.clone();
    let custom_verify = opts.verify;

    let verify_delivery: ArtifactVerifierFn = Arc::new(move |ctx: &Context, paths: &[PathBuf]| {
        let mut reason = match &repairs_verify {
            Some(repairs) => match repairs.waiting_reason() {
                Ok(r) => r,
                Err(e) => return VerifyOutcome::fail("repair_state_error", e),
            },
            None => None,
        };
        let previous = match read_receipt(&receipt_verify) {
            Ok(receipt) => receipt,
            Err(e) => return VerifyOutcome::fail("invalid_receipt", e),
        };
        if let Some(previous) = previous {
            let current_hash = match (input_hash_verify)() {
                Ok(h) => h,
                Err(e) => return VerifyOutcome::fail("input_hash_error", e),
            };
            if previous.input_hash != current_hash
                && previous.artifact_hash == files_hash(paths)
                && reason.is_none()
            {
                reason = Some(
                    "The source inputs changed. Update the existing artifacts for the current inputs."
                        .to_string(),
                );
            }
        }
        if reason.is_none() {
            if let Some(verify) = &custom_verify {
                return verify(ctx, paths);
            }
        }
        match reason {
            Some(msg) => VerifyOutcome::fail("", msg),
            None => VerifyOutcome::pass("Artifacts delivered"),
        }
    });

    let packet_inner = opts.packet;
    let input_hash_pkt = opts.input_hash.clone();
    let artifacts_pkt = opts.artifacts.clone();
    let repairs_pkt = opts.repairs.clone();
    let write_packet: PacketFn = Arc::new(move |ctx: &Context| {
        let mut doc = match packet_inner(ctx)? {
            Value::Object(map) => map,
            _ => return Err("Worker packet must be a JSON object".to_string()),
        };
        let paths = (artifacts_pkt)(ctx)?;
        doc.insert("input_hash".to_string(), Value::String((input_hash_pkt)()?));
        doc.insert(
            "outputs".to_string(),
            Value::Array(
                paths
                    .iter()
                    .map(|p| Value::String(p.to_string_lossy().into_owned()))
                    .collect(),
            ),
        );
        doc.insert(
            "delivery_note".to_string(),
            Value::String(
                "If a source input changed but your revalidated output has the same content, \
                 include this packet's input_hash in a delivered JSON file."
                    .to_string(),
            ),
        );
        if let Some(repairs) = &repairs_pkt {
            for (k, v) in repairs.instructions()? {
                doc.insert(k, v);
            }
        }
        Ok(Value::Object(doc))
    });

    let input_hash_gate = opts.input_hash.clone();
    let gate_hash_fn: InputHashFn = Arc::new(move |_ctx| (input_hash_gate)());
    let mut gate = subagent_gate(SubagentGateOptions {
        id: opts.node_id,
        packet_path: opts.packet_path,
        artifact_paths: opts.artifacts.clone(),
        packet_fn: write_packet,
        input_hash_fn: gate_hash_fn,
        deps: opts.deps,
        verify_fn: Some(verify_delivery),
        resource: opts.resource,
        cacheable: true,
    });

    let run_gate = gate.run_fn;
    let enabled_run = enabled.clone();
    let receipt_run = receipt.clone();
    let input_hash_run = opts.input_hash.clone();
    let artifacts_run = opts.artifacts.clone();
    gate.kind = TaskKind::Worker;
    gate.run_fn = Arc::new(move |ctx, attempt| {
        if !(enabled_run)().map_err(TaskRunError::other)? {
            return Ok(Value::String("Worker disabled".to_string()));
        }
        let detail = run_gate(ctx, attempt)?;
        let paths = (artifacts_run)(ctx).map_err(TaskRunError::other)?;
        let in_hash = (input_hash_run)().map_err(TaskRunError::other)?;
        write_json(
            &receipt_run,
            &json!({
                "input_hash": in_hash,
                "artifact_hash": files_hash(&paths),
            }),
            true,
        )
        .map_err(TaskRunError::other)?;
        Ok(detail)
    });

    let input_hash_node = opts.input_hash;
    let repairs_hash = opts.repairs;
    let enabled_hash = enabled.clone();
    gate.input_hash_fn = Arc::new(move |_ctx| {
        let rep = match &repairs_hash {
            Some(r) => Value::Object(r.instructions()?),
            None => Value::Null,
        };
        Ok(fingerprint(json!([
            (input_hash_node)()?,
            rep,
            (enabled_hash)()?
        ])))
    });

    let artifacts_out = opts.artifacts;
    gate.outputs_fn = Arc::new(move |ctx| {
        if (enabled)()? {
            let mut out = (artifacts_out)(ctx)?;
            out.push(receipt.clone());
            Ok(out)
        } else {
            Ok(Vec::new())
        }
    });

    gate
}

pub fn record_verification(
    node_id: impl Into<String>,
    path: PathBuf,
    input_hash: StringFn,
    check: CheckFn,
    deps: Vec<String>,
    resource: Option<String>,
    cacheable: bool,
) -> TaskNode {
    let path_run = path.clone();
    let path_out = path;
    let mut node = TaskNode::new(
        node_id,
        TaskKind::Verifier,
        Arc::new(move |ctx, _attempt| {
            let outcome = check(ctx).map_err(TaskRunError::other)?;
            let mut results = ctx
                .get("verification_results")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            results.insert(
                path_run.to_string_lossy().into_owned(),
                json!([outcome.ok, outcome.error_signature, outcome.detail]),
            );
            ctx.insert("verification_results", Value::Object(results));
            write_json(
                &path_run,
                &json!({
                    "ok": outcome.ok,
                    "error_signature": outcome.error_signature,
                    "detail": outcome.detail,
                }),
                true,
            )
            .map_err(TaskRunError::other)?;
            let desc = if outcome.error_signature.is_empty() {
                outcome.detail
            } else {
                format!("{}: {}", outcome.error_signature, outcome.detail)
            };
            Ok(Value::String(desc))
        }),
        Arc::new(move |_ctx| (input_hash)()),
    );
    node.deps = deps;
    node.resource = resource;
    node.cacheable = cacheable;
    node.outputs_fn = Arc::new(move |_ctx| Ok(vec![path_out.clone()]));
    node.verify_fn = Some(Arc::new(|_ctx, val| {
        VerifyOutcome::pass(val.as_str().unwrap_or("ok"))
    }));
    node
}

pub fn verification(path: &Path) -> Result<VerifyOutcome, String> {
    let rec = read_object(path)?;
    let ok = rec
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("Missing boolean 'ok' in {}", path.display()))?;
    let sig = rec
        .get("error_signature")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = rec
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(VerifyOutcome {
        ok,
        error_signature: sig,
        detail,
    })
}

pub fn require_bug_id(value: &str) -> Result<String, String> {
    let bug_id = kgraph_core::files::require_id(value)?;
    let upper = bug_id.to_ascii_uppercase();
    let is_reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.len() == 4
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if is_reserved {
        return Err(format!(
            "Invalid bug ID {:?}: reserved Windows filename",
            bug_id
        ));
    }
    Ok(bug_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn malformed_receipts_cannot_recertify_existing_artifacts() {
        let dir = tempdir().unwrap();
        let artifact = dir.path().join("delivery.txt");
        fs::write(&artifact, "stale output").unwrap();
        let packet = dir.path().join("request.json");
        let receipt = packet.with_extension("receipt.json");
        let worker = packet_worker(PacketWorkerOptions {
            node_id: "deliver".into(),
            packet_path: packet,
            artifacts: Arc::new(move |_| Ok(vec![artifact.clone()])),
            input_hash: Arc::new(|| Ok("current inputs".into())),
            packet: Arc::new(|_| Ok(json!({}))),
            deps: vec![],
            repairs: None,
            enabled: None,
            verify: None,
            resource: None,
        });
        let ctx = Context::new();
        // Initial delivery with no receipt creates a valid certification.
        (worker.run_fn)(&ctx, 0).unwrap();
        assert!(read_receipt(&receipt).unwrap().is_some());
        for corrupt in [
            "{",
            "[]",
            "{}",
            r#"{"input_hash":"prior"}"#,
            r#"{"input_hash":"prior","artifact_hash":42}"#,
            r#"{"input_hash":"prior","artifact_hash":"bogus"}"#,
        ] {
            fs::write(&receipt, corrupt).unwrap();
            let error = (worker.run_fn)(&ctx, 0).unwrap_err();
            assert!(error.to_string().contains("Invalid receipt"), "{error}");
            assert_eq!(fs::read_to_string(&receipt).unwrap(), corrupt);
        }
        fs::remove_file(&receipt).unwrap();
        fs::create_dir(&receipt).unwrap();
        assert!((worker.run_fn)(&ctx, 0).is_err());
        assert!(receipt.is_dir());
    }
}
