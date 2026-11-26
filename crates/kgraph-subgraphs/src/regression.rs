//! Game regression suites with command logs, resumable screenshot verification, and manual QA.

use crate::bug_fix::{
    command_json, configured_tester, outputs, qa_hash, remove_file, resolve_path,
};
use crate::common::{files_hash, fingerprint};
use kgraph_core::files::{read_object, require_id, write_json, write_text};
use kgraph_core::gate::{subagent_gate, SubagentGateOptions};
use kgraph_core::hash::{hash_globs, hash_paths};
use kgraph_core::qa_metrics::{init_qa_checklist, write_qa_scorecard};
use kgraph_core::shell::{shell_task, CommandSpec, ShellTaskOptions};
use kgraph_core::types::*;
use kgraph_core::verifiers::verify_png_file;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct VisualCheckConfig {
    pub id: String,
    pub path: Option<PathBuf>,
    pub min_width: Option<u32>,
    pub min_height: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct GameRegressionConfig {
    pub suite_id: String,
    pub work_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub build_cmd: Option<CommandSpec>,
    pub test_cmd: CommandSpec,
    pub watch_globs: Vec<String>,
    pub visual_checks: Vec<VisualCheckConfig>,
    pub min_width: u32,
    pub min_height: u32,
    pub timeout_s: Option<f64>,
    pub deps: Vec<String>,
    pub tester: Option<String>,
}

impl Default for GameRegressionConfig {
    fn default() -> Self {
        Self {
            suite_id: String::new(),
            work_dir: PathBuf::new(),
            repo_dir: ".".into(),
            build_cmd: None,
            test_cmd: CommandSpec::Args(vec![]),
            watch_globs: vec![],
            visual_checks: vec![],
            min_width: 16,
            min_height: 16,
            timeout_s: None,
            deps: vec![],
            tester: None,
        }
    }
}

#[derive(Serialize)]
struct VisualCheck {
    id: String,
    path: PathBuf,
    min_width: u32,
    min_height: u32,
}

impl VisualCheck {
    fn verify(&self) -> VerifyOutcome {
        verify_png_file(&self.path, self.min_width, self.min_height)
    }
}

fn dimension(v: u32, name: &str) -> Result<u32, String> {
    if v == 0 {
        Err(format!("{name} must be a positive integer"))
    } else {
        Ok(v)
    }
}

const GENERATED: [&str; 10] = [
    "preflight.json",
    "build.log",
    "regression.log",
    "visual_packet.json",
    "regression_report.json",
    "regression_report.md",
    "manual_qa.json",
    "qa_scorecard.json",
    "qa_scorecard.md",
    "manual_qa.json.lock",
];

struct Suite {
    cfg: GameRegressionConfig,
    checks: Vec<VisualCheck>,
    excluded: Vec<PathBuf>,
    settings: Map<String, Value>,
    config_hash: String,
}

impl Suite {
    fn new(mut cfg: GameRegressionConfig) -> Result<Self, String> {
        require_id(&cfg.suite_id)?;
        let build_cmd = command_json(cfg.build_cmd.as_ref(), "build_cmd")?;
        let test_cmd = command_json(Some(&cfg.test_cmd), "test_cmd")?;
        cfg.work_dir = resolve_path(&cfg.work_dir)?;
        cfg.repo_dir = resolve_path(&cfg.repo_dir)?;
        cfg.tester = configured_tester(cfg.tester.as_deref())?;
        dimension(cfg.min_width, "min_width")?;
        dimension(cfg.min_height, "min_height")?;
        let mut excluded: Vec<_> = GENERATED
            .iter()
            .map(|name| cfg.work_dir.join(name))
            .collect();
        let mut paths = HashSet::new();
        let mut ids = HashSet::new();
        let mut checks = vec![];
        for check in &cfg.visual_checks {
            let id = require_id(&check.id)?;
            let relative = check
                .path
                .clone()
                .unwrap_or_else(|| Path::new("visual").join(format!("{id}.png")));
            let path = resolve_path(&cfg.work_dir.join(relative))?;
            if !ids.insert(id.clone()) || !paths.insert(path.clone()) {
                return Err(format!("Duplicate visual check ID or path for {id:?}"));
            }
            if excluded.contains(&path) {
                return Err(
                    "Visual check paths must be separate from generated workflow files".into(),
                );
            }
            checks.push(VisualCheck {
                id,
                path,
                min_width: dimension(check.min_width.unwrap_or(cfg.min_width), "min_width")?,
                min_height: dimension(check.min_height.unwrap_or(cfg.min_height), "min_height")?,
            });
        }
        excluded.extend(paths);
        let settings = json!({"suite_id":cfg.suite_id, "repo_dir":cfg.repo_dir, "build_cmd":build_cmd,
            "test_cmd":test_cmd, "watch_globs":cfg.watch_globs, "visual_checks":checks, "timeout_s":cfg.timeout_s});
        let config_hash = fingerprint(json!([settings]));
        Ok(Self {
            cfg,
            checks,
            excluded,
            config_hash,
            settings: settings.as_object().unwrap().clone(),
        })
    }

    fn path(&self, file: &str) -> PathBuf {
        self.cfg.work_dir.join(file)
    }

    fn source_hash(&self) -> Result<String, String> {
        let repo = &self.cfg.repo_dir;
        if !repo.is_dir() {
            return Err(format!(
                "Repository directory does not exist: {}",
                repo.display()
            ));
        }
        if self.cfg.watch_globs.is_empty() {
            hash_paths(
                std::slice::from_ref(repo),
                Some(repo),
                None,
                &self.excluded,
                true,
            )
        } else {
            hash_globs(
                &self.cfg.watch_globs,
                Some(repo),
                None,
                &self.excluded,
                true,
            )
        }
    }

    fn clear_reports(&self) -> Result<(), String> {
        remove_file(&self.path("regression_report.json"))?;
        remove_file(&self.path("regression_report.md"))
    }

    fn preflight(self: &Arc<Self>) -> TaskNode {
        let run = self.clone();
        let hash = self.clone();
        let mut task = TaskNode::new(
            "preflight",
            TaskKind::Code,
            Arc::new(move |_, _| {
                run.clear_reports().map_err(TaskRunError::other)?;
                let mut data = run.settings.clone();
                data.insert(
                    "source_hash".into(),
                    json!(run.source_hash().map_err(TaskRunError::other)?),
                );
                data.insert("config_hash".into(), json!(run.config_hash));
                write_json(&run.path("preflight.json"), &data, true)
                    .map_err(TaskRunError::other)?;
                Ok(Value::Null)
            }),
            Arc::new(move |_| Ok(fingerprint(json!([hash.config_hash, hash.source_hash()?])))),
        );
        task.outputs_fn = outputs(vec![self.path("preflight.json")]);
        task
    }

    fn command(
        self: &Arc<Self>,
        stage: &str,
        cmd: CommandSpec,
        log: &str,
        parent: &str,
        inputs: Vec<PathBuf>,
    ) -> TaskNode {
        let path = self.path(log);
        let mut opts =
            ShellTaskOptions::new(stage, cmd, Arc::new(move |_| Ok(files_hash(&inputs))));
        opts.cwd = Some(self.cfg.repo_dir.clone());
        opts.timeout_s = self.cfg.timeout_s;
        opts.deps = vec![parent.into()];
        opts.log_path = Some(path.clone());
        opts.outputs_fn = outputs(vec![path]);
        let mut task = shell_task(opts);
        let run = task.run_fn;
        let suite = self.clone();
        task.run_fn = Arc::new(move |ctx, attempt| {
            suite.clear_reports().map_err(TaskRunError::other)?;
            run(ctx, attempt)
        });
        task
    }

    fn visual_gate(self: &Arc<Self>, inputs: Vec<PathBuf>) -> TaskNode {
        let packet = self.clone();
        let mut opts = SubagentGateOptions::new(
            "capture_visual_evidence",
            self.path("visual_packet.json"),
            outputs(self.checks.iter().map(|c| c.path.clone()).collect()),
            Arc::new(move |_| {
                packet.clear_reports()?;
                let paths: Map<String, Value> = packet
                    .checks
                    .iter()
                    .map(|c| (c.id.clone(), json!(c.path)))
                    .collect();
                Ok(
                    json!({"stage":"capture_visual_evidence", "suite_id":packet.cfg.suite_id, "repo_dir":packet.cfg.repo_dir,
                    "outputs":paths, "visual_checks":packet.checks, "instructions":[
                        "Capture each screenshot at the specified path and minimum dimensions.",
                        "Use an 8-bit non-interlaced PNG with visible content for each check."]}),
                )
            }),
            Arc::new(move |_| Ok(files_hash(&inputs))),
        );
        opts.deps = vec!["run_regression".into()];
        let suite = self.clone();
        opts.verify_fn = Some(Arc::new(move |_, _| {
            let failures: Vec<_> = suite
                .checks
                .iter()
                .map(VisualCheck::verify)
                .filter(|r| !r.ok)
                .collect();
            match failures.first() {
                Some(first) => VerifyOutcome::fail(
                    &first.error_signature,
                    failures
                        .iter()
                        .map(|r| r.detail.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                None => {
                    VerifyOutcome::pass(format!("Verified {} visual check(s)", suite.checks.len()))
                }
            }
        }));
        subagent_gate(opts)
    }

    fn report(&self) -> Result<(), String> {
        let id = &self.cfg.suite_id;
        let mut cases = vec![
            json!({"id":id, "title":format!("Re-test {id}"), "kind":"regression-suite", "automated_status":"pass", "ai_attempts":1}),
        ];
        let mut evidence = vec![];
        for check in &self.checks {
            let result = check.verify();
            if !result.ok {
                self.clear_reports()?;
                return Err("A screenshot changed after verification. Rerun to verify the current evidence.".into());
            }
            let mut item = json!(check);
            item["ok"] = json!(true);
            item["error_signature"] = json!(result.error_signature);
            item["detail"] = json!(result.detail);
            evidence.push(item);
            let case_id = if &check.id == id {
                format!("visual/{id}")
            } else {
                check.id.clone()
            };
            cases.push(json!({"id":case_id, "title":format!("Visual check: {}",check.id), "kind":"visual", "automated_status":"pass", "ai_attempts":1}));
        }
        let mut logs = Map::new();
        if self.cfg.build_cmd.is_some() {
            logs.insert("build".into(), log_summary(&self.path("build.log"))?);
        }
        logs.insert(
            "regression".into(),
            log_summary(&self.path("regression.log"))?,
        );
        let preflight = read_object(&self.path("preflight.json"))?;
        let source = preflight
            .get("source_hash")
            .and_then(Value::as_str)
            .ok_or("Missing preflight source hash")?;
        let config = preflight
            .get("config_hash")
            .and_then(Value::as_str)
            .ok_or("Missing preflight config hash")?;
        let qa = self.path("manual_qa.json");
        init_qa_checklist(
            &qa,
            id,
            &cases,
            "game-regression",
            Some(&format!("Game regression for {id}")),
            self.cfg.tester.as_deref(),
            Some(&fingerprint(json!([source, config]))),
            None,
            true,
        )?;
        write_qa_scorecard(&qa, None, None, false, None)?;
        write_json(
            &self.path("regression_report.json"),
            &json!({"suite_id":id, "status":"passed", "preflight":preflight, "logs":logs, "visual_checks":evidence}),
            true,
        )?;
        let mut markdown = format!("# Game regression report for {id}\n\nPassed.\n\nSource fingerprint `{source}`\n\nSuite configuration fingerprint `{config}`\n");
        for (label, title) in [("build", "Build"), ("regression", "Regression")] {
            if let Some(log) = logs.get(label) {
                markdown.push_str(&format!("\n## {title} log\n\n[{label}.log]({label}.log)\n"));
                if let Some(tail) = log["tail"].as_str().filter(|s| !s.is_empty()) {
                    markdown.push('\n');
                    for line in tail.lines() {
                        markdown.push_str(&format!("    {line}\n"));
                    }
                }
            }
        }
        if !evidence.is_empty() {
            markdown.push_str("\n## Visual evidence\n\n");
            for item in evidence {
                markdown.push_str(&format!(
                    "- `{}` — {} (`{}`)\n",
                    item["id"].as_str().unwrap(),
                    item["detail"].as_str().unwrap(),
                    item["path"].as_str().unwrap()
                ));
            }
        }
        write_text(&self.path("regression_report.md"), &markdown)
    }
}

fn log_summary(path: &Path) -> Result<Value, String> {
    const TAIL_BYTES: u64 = 4096;
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let size = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(
        json!({"path":path, "size_bytes":size, "tail":String::from_utf8_lossy(&buf).trim_end_matches(|c:char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)), "truncated":size > TAIL_BYTES}),
    )
}

pub fn create_game_regression_subgraph(cfg: GameRegressionConfig) -> Result<SubgraphNode, String> {
    let suite = Arc::new(Suite::new(cfg)?);
    let mut nodes = vec![Node::Task(suite.preflight())];
    let mut inputs = vec![suite.path("preflight.json")];
    if let Some(build) = &suite.cfg.build_cmd {
        nodes.push(Node::Task(suite.command(
            "compile_or_build",
            build.clone(),
            "build.log",
            "preflight",
            inputs.clone(),
        )));
        inputs.push(suite.path("build.log"));
    }
    nodes.push(Node::Task(suite.command(
        "run_regression",
        suite.cfg.test_cmd.clone(),
        "regression.log",
        nodes.last().unwrap().id(),
        inputs.clone(),
    )));
    inputs.push(suite.path("regression.log"));
    if !suite.checks.is_empty() {
        nodes.push(Node::Task(suite.visual_gate(inputs.clone())));
        inputs.extend(suite.checks.iter().map(|c| c.path.clone()));
    }
    let report_suite = suite.clone();
    let tester = suite.cfg.tester.clone();
    let mut report = TaskNode::new(
        "regression_report",
        TaskKind::Code,
        Arc::new(move |_, _| {
            report_suite.report().map_err(TaskRunError::other)?;
            Ok(Value::Null)
        }),
        Arc::new(move |_| {
            Ok(qa_hash(
                vec![json!("qa-v1"), json!(files_hash(&inputs))],
                tester.as_deref(),
            ))
        }),
    );
    report.deps = vec![nodes.last().unwrap().id().into()];
    report.outputs_fn = outputs(vec![
        suite.path("regression_report.json"),
        suite.path("regression_report.md"),
    ]);
    nodes.push(Node::Task(report));
    let mut graph = SubgraphNode::new(&suite.cfg.suite_id, nodes);
    graph.deps = suite.cfg.deps.clone();
    graph.qa_checklist_path = Some(suite.path("manual_qa.json"));
    Ok(graph)
}

pub fn create_game_regression_graph(cfg: GameRegressionConfig) -> Result<Vec<Node>, String> {
    Ok(vec![Node::Subgraph(create_game_regression_subgraph(cfg)?)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::engine::{GraphRunner, RunnerConfig};
    use kgraph_core::qa_metrics::{record_qa_result, QaResultInput};
    use std::fs;
    use tempfile::TempDir;

    // Grayscale PNGs with two IDAT chunks generated by tests/_support.py.
    const VALID: &str = "89504e470d0a1a0a0000000d4948445200000018000000120800000000648741420000000949444154789c63f85f8f1d308cf1348b8000000009494441544a0c0d0900ba0edf51996626cb0000000049454e44ae426082";
    const SMALL: &str = "89504e470d0a1a0a0000000d4948445200000017000000120800000000958c1acf0000000949444154789c63f85f8f15308cff27da3800000009494441548a0f4a710051fbd66362977bf00000000049454e44ae426082";
    const BLANK: &str = "89504e470d0a1a0a0000000d4948445200000018000000120800000000648741420000000849444154789c63a8c7011846805ea23d000000094944415425868604006916d65119de038c0000000049454e44ae426082";

    fn png(path: &Path, hex: &str) {
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    struct Fixture {
        _temp: TempDir,
        cfg: GameRegressionConfig,
        #[cfg_attr(not(unix), allow(dead_code))]
        state: PathBuf,
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let repo = root.join("game repo");
            fs::create_dir(&repo).unwrap();
            fs::write(repo.join("game.txt"), "lives = 3\n").unwrap();
            let cfg = GameRegressionConfig {
                suite_id: "game-smoke".into(),
                work_dir: repo.join("regression outputs"),
                repo_dir: repo,
                test_cmd: CommandSpec::Shell("echo regression passed".into()),
                tester: Some("Alex".into()),
                ..Default::default()
            };
            Self {
                _temp: temp,
                cfg,
                state: root.join("state.json"),
            }
        }

        fn run(&self, status: &str) -> (Vec<String>, Option<GraphFailure>) {
            let mut cfg = RunnerConfig::new(&self.state);
            cfg.logger = Some(Arc::new(|_| {}));
            cfg.max_worker_calls = Some(0);
            let runner = GraphRunner::new(cfg).unwrap();
            let graph = create_game_regression_graph(self.cfg.clone()).unwrap();
            let error = runner.run(&graph, &Context::new(), "").err();
            assert_eq!(
                error.as_ref().map_or("passed", GraphFailure::status_str),
                status,
                "{error:?}"
            );
            let cached = runner
                .trace()
                .into_iter()
                .filter(|v| v["status"] == "cached")
                .map(|v| v["node"].as_str().unwrap().to_owned())
                .collect();
            (cached, error)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.cfg.work_dir.join(name)
        }
        fn read(&self, name: &str) -> Map<String, Value> {
            read_object(&self.file(name)).unwrap()
        }
        fn source(&self, contents: &str) {
            fs::write(self.cfg.repo_dir.join("game.txt"), contents).unwrap();
        }
    }

    #[test]
    fn validates_ids_dimensions_commands_path_collisions_and_topology() {
        let f = Fixture::new();
        let mut cfg = f.cfg;
        cfg.deps = vec!["setup".into()];
        let suite = create_game_regression_subgraph(cfg.clone()).unwrap();
        assert_eq!(suite.deps, ["setup"]);
        assert_eq!(
            suite.nodes.iter().map(Node::id).collect::<Vec<_>>(),
            ["preflight", "run_regression", "regression_report"]
        );
        assert_eq!(
            suite.qa_checklist_path,
            Some(cfg.work_dir.join("manual_qa.json"))
        );
        cfg.build_cmd = Some(CommandSpec::Shell("echo build".into()));
        cfg.visual_checks = vec![VisualCheckConfig {
            id: "menu".into(),
            ..Default::default()
        }];
        let suite = create_game_regression_subgraph(cfg.clone()).unwrap();
        assert_eq!(
            suite.nodes.iter().map(Node::id).collect::<Vec<_>>(),
            [
                "preflight",
                "compile_or_build",
                "run_regression",
                "capture_visual_evidence",
                "regression_report"
            ]
        );
        for pair in suite.nodes.windows(2) {
            assert_eq!(pair[1].deps(), [pair[0].id()]);
        }
        let invalid = |cfg| create_game_regression_subgraph(cfg).err().unwrap();
        assert!(invalid(GameRegressionConfig {
            suite_id: "../smoke".into(),
            ..cfg.clone()
        })
        .contains("Invalid ID"));
        assert!(invalid(GameRegressionConfig {
            min_height: 0,
            ..cfg.clone()
        })
        .contains("positive integer"));
        assert!(invalid(GameRegressionConfig {
            test_cmd: CommandSpec::Shell(" ".into()),
            ..cfg.clone()
        })
        .contains("test_cmd"));
        let menu = &cfg.visual_checks[0];
        let collision = VisualCheckConfig {
            id: "other".into(),
            path: Some("unused/../visual/menu.png".into()),
            ..Default::default()
        };
        for duplicate in [menu.clone(), collision] {
            assert!(invalid(GameRegressionConfig {
                visual_checks: vec![menu.clone(), duplicate],
                ..cfg.clone()
            })
            .contains("Duplicate visual check"));
        }
        cfg.visual_checks[0].path = Some("preflight.json".into());
        assert!(invalid(cfg).contains("separate from generated"));
    }

    #[test]
    fn visual_checks_measure_visible_colors_for_transparent_and_indexed_images() {
        let f = Fixture::new();
        let check = VisualCheck {
            id: "menu".into(),
            path: f.file("menu.png"),
            min_width: 16,
            min_height: 16,
        };
        let rgba_hidden = "89504e470d0a1a0a0000000d49484452000000100000001008060000001ff3ff610000000d49444154789c6360606266600462b2e951cc9e10d10000000e4944415403460d183560b81800002ff505814bb6d2cf0000000049454e44ae426082";
        let rgba_visible = "89504e470d0a1a0a0000000d49484452000000100000001008060000001ff3ff610000000e49444154789c6360606266606462fe4f363d550deba30000000e494441546ac0a801a3060c1703007cc48501b7da8f300000000049454e44ae426082";
        let duplicate_palette = "89504e470d0a1a0a0000000d4948445200000010000000100803000000282d0f5300000006504c54450305070305079791a3170000000749444154789c6360604483b1b2fa750000000849444154235b000044d00081bb23cc930000000049454e44ae426082";
        let distinct_palette = "89504e470d0a1a0a0000000d4948445200000010000000100803000000282d0f5300000006504c54450305070905079a0626c10000000749444154789c6360604483b1b2fa750000000849444154235b000044d00081bb23cc930000000049454e44ae426082";
        for (data, expected_signature) in [
            (rgba_hidden, "blank_png"),
            (rgba_visible, ""),
            (duplicate_palette, "blank_png"),
            (distinct_palette, ""),
        ] {
            png(&check.path, data);
            let result = check.verify();
            assert_eq!(result.error_signature, expected_signature);
            assert_eq!(result.ok, expected_signature.is_empty());
        }
    }

    #[test]
    fn log_tail_respects_byte_boundary_and_replaces_invalid_utf8() {
        let f = Fixture::new();
        let log = f.cfg.repo_dir.join("sample.log");
        fs::write(
            &log,
            [b"head\xc3".as_slice(), b"\xa9", &vec![b'x'; 4094], b"\n"].concat(),
        )
        .unwrap();
        let summary = log_summary(&log).unwrap();
        assert_eq!(summary["truncated"], true);
        assert_eq!(summary["size_bytes"], 4101);
        assert_eq!(summary["tail"], format!("�{}", "x".repeat(4094)));
    }

    #[cfg(unix)]
    #[test]
    fn warm_run_ignores_generated_files_source_edits_reset_manual_results() {
        let mut f = Fixture::new();
        f.cfg.build_cmd = Some(CommandSpec::Shell("echo build passed".into()));
        assert!(f.run("passed").0.is_empty());
        let before = f.read("preflight.json");
        let report = f.read("regression_report.json");
        assert_eq!(report["preflight"], json!(before));
        assert_eq!(report["logs"]["build"]["tail"], "build passed");
        assert_eq!(report["logs"]["regression"]["tail"], "regression passed");
        assert_eq!(report["visual_checks"], json!([]));
        assert!(fs::read_to_string(f.file("regression_report.md"))
            .unwrap()
            .contains("    regression passed"));
        let qa = f.read("manual_qa.json");
        assert_eq!(qa["workflow_type"], "game-regression");
        assert_eq!(
            qa["build_ref"],
            json!(fingerprint(json!([
                before["source_hash"],
                before["config_hash"]
            ])))
        );
        record_qa_result(
            &f.file("manual_qa.json"),
            &QaResultInput {
                case_id: "game-smoke".into(),
                manual_qa: "pass".into(),
                resolved_on_attempt: None,
                gap_type: None,
                notes: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            f.run("passed").0,
            [
                "game-smoke/preflight",
                "game-smoke/compile_or_build",
                "game-smoke/run_regression",
                "game-smoke/regression_report"
            ]
        );
        f.source("lives = 5\n");
        assert!(f.run("passed").0.is_empty());
        let after = f.read("preflight.json");
        assert_eq!(before["config_hash"], after["config_hash"]);
        assert_ne!(before["source_hash"], after["source_hash"]);
        assert_eq!(
            f.read("manual_qa.json")["cases"][0]["manual_qa"],
            "not_tested"
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_globs_and_config_changes_invalidate_only_required_stages() {
        let mut f = Fixture::new();
        f.cfg.watch_globs = vec!["*.txt".into(), "assets/**/*".into()];
        f.run("passed");
        let preflight = f.read("preflight.json");
        fs::write(f.cfg.repo_dir.join("ignored.md"), "notes").unwrap();
        fs::remove_file(f.file("regression_report.md")).unwrap();
        assert_eq!(
            f.run("passed").0,
            ["game-smoke/preflight", "game-smoke/run_regression"]
        );
        assert!(f.file("regression_report.md").is_file());
        assert!(!f.file("build.log").exists());
        assert!(!f.file("visual_packet.json").exists());
        f.cfg.timeout_s = Some(10.0);
        assert!(f.run("passed").0.is_empty());
        let changed = f.read("preflight.json");
        assert_ne!(preflight["config_hash"], changed["config_hash"]);
        assert_eq!(preflight["source_hash"], changed["source_hash"]);
        f.source("edit");
        assert!(f.run("passed").0.is_empty());
        assert_ne!(
            f.read("preflight.json")["source_hash"],
            changed["source_hash"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_failures_clear_prior_reports_and_skip_downstream_stages() {
        let mut f = Fixture::new();
        f.cfg.visual_checks = vec![VisualCheckConfig {
            id: "menu".into(),
            ..Default::default()
        }];
        f.cfg.build_cmd = Some(CommandSpec::Args(vec![
            "sh".into(),
            "-c".into(),
            "echo build failed >&2; exit 7".into(),
        ]));
        let (_, failure) = f.run("failed");
        assert!(failure
            .unwrap()
            .to_string()
            .contains("game-smoke/compile_or_build"));
        assert!(fs::read_to_string(f.file("build.log"))
            .unwrap()
            .contains("build failed"));
        for path in [
            "regression.log",
            "visual_packet.json",
            "regression_report.json",
            "regression_report.md",
        ] {
            assert!(!f.file(path).exists());
        }
        f.cfg.build_cmd = Some(CommandSpec::Shell("echo build passed".into()));
        f.cfg.test_cmd = CommandSpec::Shell(
            "test \"$(cat game.txt)\" = 'lives = 3' || { echo 'command failed' >&2; exit 7; }"
                .into(),
        );
        png(&f.file("visual/menu.png"), VALID);
        f.run("passed");
        assert!(f.file("regression_report.json").is_file());
        f.source("lives = 0");
        let (_, failure) = f.run("failed");
        assert!(failure
            .unwrap()
            .to_string()
            .contains("game-smoke/run_regression"));
        assert!(fs::read_to_string(f.file("regression.log"))
            .unwrap()
            .contains("command failed"));
        for path in [
            "visual_packet.json",
            "regression_report.json",
            "regression_report.md",
        ] {
            assert!(!f.file(path).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn visual_gate_checks_each_delivery_excludes_evidence_from_sources_and_rechecks_before_reporting(
    ) {
        let mut f = Fixture::new();
        f.cfg.visual_checks = vec![
            VisualCheckConfig {
                id: "menu".into(),
                ..Default::default()
            },
            VisualCheckConfig {
                id: "game-smoke".into(),
                path: Some("captures/level.png".into()),
                min_width: Some(24),
                min_height: None,
            },
        ];
        let (_, failure) = f.run("awaiting_subagents");
        assert_eq!(
            failure.unwrap().packet_path.as_deref(),
            f.file("visual_packet.json").to_str()
        );
        let menu = f.file("visual/menu.png");
        let level = f.file("captures/level.png");
        let packet = f.read("visual_packet.json");
        assert_eq!(packet["stage"], "capture_visual_evidence");
        assert_eq!(packet["outputs"], json!({"menu":menu, "game-smoke":level}));
        assert_eq!(packet["visual_checks"][1]["min_width"], 24);
        png(&menu, VALID);
        for (data, reason) in [
            (VALID, "CRC"),
            (BLANK, "solid color"),
            (SMALL, "minimum 24×16px"),
        ] {
            png(&level, data);
            if reason == "CRC" {
                let mut bytes = fs::read(&level).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                fs::write(&level, bytes).unwrap();
            }
            let (cached, failure) = f.run("awaiting_subagents");
            assert_eq!(
                cached,
                ["game-smoke/preflight", "game-smoke/run_regression"]
            );
            let failure = failure.unwrap();
            assert!(
                failure.reason.contains(reason),
                "expected {reason}: {failure}"
            );
            assert_eq!(failure.reason, verify_png_file(&level, 24, 16).detail);
            assert!(!f.file("regression_report.json").exists());
        }
        png(&level, VALID);
        assert_eq!(
            f.run("passed").0,
            ["game-smoke/preflight", "game-smoke/run_regression"]
        );
        let report = f.read("regression_report.json");
        assert_eq!(report["visual_checks"][0]["ok"], true);
        assert_eq!(
            report["visual_checks"][1]["detail"],
            json!(verify_png_file(&level, 24, 16).detail)
        );
        let score = f.read("qa_scorecard.json");
        assert_eq!(score["cases"][0]["kind"], "regression-suite");
        assert_eq!(score["cases"][1]["id"], "menu");
        assert_eq!(score["cases"][2]["id"], "visual/game-smoke");
        assert!(fs::read_to_string(f.file("qa_scorecard.md"))
            .unwrap()
            .contains("Visual check: menu"));
        assert!(fs::read_to_string(f.file("regression_report.md"))
            .unwrap()
            .contains("24×18px"));
        assert!(f
            .run("passed")
            .0
            .contains(&"game-smoke/capture_visual_evidence".into()));
        fs::remove_file(menu).unwrap();
        let fresh = create_game_regression_subgraph(f.cfg.clone()).unwrap();
        let Node::Task(report) = fresh.nodes.last().unwrap() else {
            panic!("missing report")
        };
        let err = (report.run_fn)(&Context::new(), 1).unwrap_err();
        assert!(err
            .to_string()
            .contains("screenshot changed after verification"));
        assert_eq!(
            f.run("awaiting_subagents").0,
            ["game-smoke/preflight", "game-smoke/run_regression"]
        );
        assert!(!f.file("regression_report.json").exists());
        assert!(!f.file("regression_report.md").exists());
    }
}
