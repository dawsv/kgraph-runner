//! A workflow gate that publishes a request when its artifacts need attention.

use crate::files::write_json;
use crate::types::{
    Context, GraphFailure, InputHashFn, OutputsFn, TaskKind, TaskNode, TaskRunError, VerifyOutcome,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

pub type ArtifactVerifierFn = Arc<dyn Fn(&Context, &[PathBuf]) -> VerifyOutcome + Send + Sync>;
pub type PacketFn = Arc<dyn Fn(&Context) -> Result<Value, String> + Send + Sync>;

pub struct SubagentGateOptions {
    pub id: String,
    pub packet_path: PathBuf,
    pub artifact_paths: OutputsFn,
    pub packet_fn: PacketFn,
    pub input_hash_fn: InputHashFn,
    pub deps: Vec<String>,
    pub verify_fn: Option<ArtifactVerifierFn>,
    pub resource: Option<String>,
    pub cacheable: bool,
}

impl SubagentGateOptions {
    pub fn new(
        id: impl Into<String>,
        packet_path: impl Into<PathBuf>,
        artifact_paths: OutputsFn,
        packet_fn: PacketFn,
        input_hash_fn: InputHashFn,
    ) -> Self {
        Self {
            id: id.into(),
            packet_path: packet_path.into(),
            artifact_paths,
            packet_fn,
            input_hash_fn,
            deps: Vec::new(),
            verify_fn: None,
            resource: None,
            cacheable: true,
        }
    }
}

pub fn subagent_gate(opts: SubagentGateOptions) -> TaskNode {
    let node_id = opts.id.clone();
    let resolve = opts.artifact_paths.clone();
    let run = Arc::new(move |ctx: &Context, _| {
        let paths = resolve(ctx).map_err(TaskRunError::other)?;
        let missing: Vec<_> = paths
            .iter()
            .filter(|p| !p.exists())
            .map(|p| p.display().to_string())
            .collect();
        let reason = if !missing.is_empty() {
            format!("Waiting for artifacts: {}", missing.join(", "))
        } else if let Some(verify) = &opts.verify_fn {
            let outcome = verify(ctx, &paths);
            if outcome.ok {
                return Ok(Value::String(outcome.detail));
            }
            if !outcome.detail.is_empty() {
                outcome.detail
            } else if !outcome.error_signature.is_empty() {
                outcome.error_signature
            } else {
                "Artifact verification failed".into()
            }
        } else {
            return Ok(Value::String("Artifacts present".into()));
        };

        let packet = (opts.packet_fn)(ctx).map_err(TaskRunError::other)?;
        write_json(&opts.packet_path, &packet, true).map_err(TaskRunError::other)?;
        let mut failure = GraphFailure::new(&node_id, reason)
            .with_packets(Some(opts.packet_path.display().to_string()), Vec::new());
        failure.awaiting_subagents = true;
        failure.efficiency.agent_dispatches = 1;
        Err(failure.into())
    });

    let mut node = TaskNode::new(opts.id, TaskKind::Gate, run, opts.input_hash_fn);
    node.outputs_fn = opts.artifact_paths;
    node.deps = opts.deps;
    node.resource = opts.resource;
    node.cacheable = opts.cacheable;
    node.verify_fn = Some(Arc::new(|_, result| match result.as_str() {
        Some(detail) => VerifyOutcome::pass(detail),
        None => VerifyOutcome::fail(
            "invalid_gate_result",
            "Expected artifact verification detail",
        ),
    }));
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn options(packet: PathBuf) -> SubagentGateOptions {
        SubagentGateOptions::new(
            "approval",
            packet,
            Arc::new(|ctx| {
                serde_json::from_value(ctx.get("paths").unwrap()).map_err(|e| e.to_string())
            }),
            Arc::new(|_| Ok(json!({"action": "review"}))),
            Arc::new(|_| Ok("revision".into())),
        )
    }

    #[test]
    fn missing_artifact_pauses_with_atomic_packet_and_dispatch_count() {
        let dir = tempdir().unwrap();
        let packet = dir.path().join("packets/request.json");
        let missing = dir.path().join("missing");
        let ctx = Context::from_value(json!({"paths": [dir.path(), missing]})).unwrap();
        let mut opts = options(packet.clone());
        opts.verify_fn = Some(Arc::new(|_, _| {
            panic!("missing artifacts must skip verification")
        }));
        let node = subagent_gate(opts);

        let Err(TaskRunError::Graph(failure)) = (node.run_fn)(&ctx, 1) else {
            panic!("expected pause")
        };
        assert_eq!(failure.node_path, "approval");
        assert!(failure.awaiting_subagents);
        assert_eq!(
            failure.reason,
            format!("Waiting for artifacts: {}", missing.display())
        );
        assert_eq!(failure.packet_path.as_deref(), packet.to_str());
        assert_eq!(failure.packet_paths, [packet.display().to_string()]);
        assert_eq!(failure.efficiency.agent_dispatches, 1);
        assert_eq!(
            fs::read_to_string(&packet).unwrap(),
            "{\n  \"action\": \"review\"\n}\n"
        );
        assert_eq!(fs::read_dir(packet.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn packet_builder_failure_preserves_existing_bytes() {
        let dir = tempdir().unwrap();
        let packet = dir.path().join("request.json");
        fs::write(&packet, "previous packet\n").unwrap();
        let ctx = Context::from_value(json!({"paths": [dir.path().join("missing")]})).unwrap();
        let mut opts = options(packet.clone());
        opts.packet_fn = Arc::new(|_| Err("cannot build packet".into()));
        let node = subagent_gate(opts);
        assert!(
            matches!((node.run_fn)(&ctx, 1), Err(TaskRunError::Other { detail, .. }) if detail == "cannot build packet")
        );
        assert_eq!(fs::read(&packet).unwrap(), b"previous packet\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn invalid_artifacts_pause_and_valid_artifacts_return_verifier_detail() {
        let dir = tempdir().unwrap();
        let artifact = dir.path().join("review.txt");
        let ctx = Context::from_value(json!({"paths": [artifact]})).unwrap();
        let mut opts = options(dir.path().join("request.json"));
        opts.verify_fn = Some(Arc::new(|_, paths| {
            if fs::read_to_string(&paths[0]).unwrap() == "approved" {
                VerifyOutcome::pass("Review approved")
            } else {
                VerifyOutcome::fail("pending", "Approval required")
            }
        }));
        let node = subagent_gate(opts);
        fs::write(&artifact, "pending").unwrap();
        assert!(
            matches!((node.run_fn)(&ctx, 1), Err(TaskRunError::Graph(f)) if f.reason == "Approval required")
        );
        fs::write(&artifact, "approved").unwrap();
        let result = (node.run_fn)(&ctx, 2).unwrap();
        assert_eq!(result, json!("Review approved"));
        assert_eq!(
            node.verify_fn.unwrap()(&ctx, &result),
            VerifyOutcome::pass("Review approved")
        );
        assert_eq!((node.outputs_fn)(&ctx).unwrap(), [artifact]);
    }

    #[test]
    fn present_artifacts_without_custom_verifier_skip_packet_creation() {
        let dir = tempdir().unwrap();
        let packet = dir.path().join("request.json");
        let ctx = Context::from_value(json!({"paths": [dir.path()]})).unwrap();
        let mut opts = options(packet.clone());
        opts.packet_fn = Arc::new(|_| panic!("must skip dispatch"));
        assert_eq!(
            (subagent_gate(opts).run_fn)(&ctx, 1).unwrap(),
            json!("Artifacts present")
        );
        assert!(!packet.exists());
    }
}
