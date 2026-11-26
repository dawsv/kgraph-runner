//! Native in-process timings for scripts/benchmark_rust_vs_python.py.
use kgraph_core::{
    engine::{GraphRunner, RunnerConfig},
    hash::{clear_hash_cache, hash_globs, hash_paths},
    verifiers::{inspect_png, verify_png_file},
    Context, Node, TaskKind, TaskNode,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{env, error::Error, fs, path::Path, sync::Arc, time::Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn hashes(root: &Path) -> Result<Value> {
    let mut result = json!({});
    let patterns = ["**/*.bin".to_owned()];
    for operation in ["paths", "globs"] {
        clear_hash_cache();
        for cache in ["cold", "warm"] {
            let start = Instant::now();
            let digest = match operation {
                "paths" => hash_paths(&["."], Some(root), None, &[], true)?,
                _ => hash_globs(&patterns, Some(root), None, &[], true)?,
            };
            result[operation][cache] =
                json!({"seconds": start.elapsed().as_secs_f64(), "digest": digest});
        }
    }
    Ok(result)
}

fn pngs(root: &Path) -> Result<Value> {
    let mut files = fs::read_dir(root)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    let start = Instant::now();
    let checks: Vec<_> = files.iter().map(|p| verify_png_file(p, 16, 16)).collect();
    let seconds = start.elapsed().as_secs_f64();
    let mut images = Vec::new();
    for (path, check) in files.iter().zip(checks) {
        let stats = inspect_png(path)?;
        images.push(json!({
            "name": path.file_name().and_then(|s| s.to_str()), "passed": check.ok, "signature": check.error_signature,
            "width": stats.width, "height": stats.height, "bbox": stats.bbox,
            "opaque_ratio": stats.opaque_ratio
        }));
    }
    Ok(json!({"seconds": seconds, "images": images}))
}

#[derive(Deserialize)]
struct Workflow {
    nodes: Vec<NodeSpec>,
}

#[derive(Deserialize)]
struct NodeSpec {
    id: String,
    deps: Vec<String>,
}

fn dag(spec: &Path, state: &Path) -> Result<Value> {
    let workflow: Workflow = serde_json::from_slice(&fs::read(spec)?)?;
    let nodes: Vec<_> = workflow
        .nodes
        .into_iter()
        .map(|spec| {
            let mut node = TaskNode::new(
                spec.id,
                TaskKind::Code,
                Arc::new(|_, _| Ok(Value::Null)),
                Arc::new(|_| Ok("input".into())),
            );
            node.deps = spec.deps;
            Node::Task(node)
        })
        .collect();
    let ctx = Context::new();
    let mut result = json!({});
    for phase in ["plan", "run", "cached"] {
        let runner = GraphRunner::new(RunnerConfig {
            max_parallel_nodes: 8,
            logger: Some(Arc::new(|_| {})),
            ..RunnerConfig::new(state)
        })?;
        let start = Instant::now();
        if phase == "plan" {
            let entries = runner.plan(&nodes, &ctx, "")?;
            result[phase] = json!({"seconds": start.elapsed().as_secs_f64(), "plan": entries});
        } else {
            runner.run(&nodes, &ctx, "")?;
            let seconds = start.elapsed().as_secs_f64();
            let events: Vec<_> = runner
                .trace()
                .into_iter()
                .map(|e| json!({"node": e["node"], "status": e["status"]}))
                .collect();
            result[phase] = json!({"seconds": seconds, "events": events});
        }
    }
    Ok(result)
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let result = match args.as_slice() {
        [op, root] if op == "hash" => hashes(Path::new(root))?,
        [op, root] if op == "png" => pngs(Path::new(root))?,
        [op, spec, state] if op == "dag" => dag(Path::new(spec), Path::new(state))?,
        _ => {
            return Err(
                "usage: bench_core hash DIR | png DIR | dag WORKFLOW.json STATE.json".into(),
            )
        }
    };
    println!("{result}");
    Ok(())
}
