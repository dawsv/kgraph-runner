//! Select, verify, review, and release a Histoury bug-fix batch.

use crate::bug_fix::{
    bug_qa_case, command_json, configured_tester, create_desktop_bug_fix_subgraph, outputs,
    qa_hash, resolve_path, DesktopBugFixConfig,
};
use crate::common::{
    file_hash, files_hash, fingerprint, packet_worker, read_optional, record_verification,
    require_bug_id, revision, verification, with_retry_route, CheckFn, PacketWorkerOptions,
    Repairs, StringFn,
};
use crate::version_history::{
    create_version_history_subgraph, require_release_id, VersionHistoryConfig,
};
use kgraph_core::files::{read_object, write_json, write_text};
use kgraph_core::gate::{subagent_gate, SubagentGateOptions};
use kgraph_core::hash::{hash_globs, sha256_bytes, sha256_file};
use kgraph_core::qa_metrics::{
    default_tester, init_qa_checklist, record_qa_results, write_qa_scorecard, QaResultInput,
};
use kgraph_core::shell::CommandSpec;
use kgraph_core::types::*;
use kgraph_core::verifiers::inspect_png;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct HistouryBugFixConfig {
    pub batch_id: String,
    pub work_dir: PathBuf,
    pub repos: BTreeMap<String, PathBuf>,
    pub product: String,
    pub bugs: Option<Vec<Map<String, Value>>>,
    pub build_cmds: BTreeMap<String, CommandSpec>,
    pub test_cmds: BTreeMap<String, CommandSpec>,
    pub watch_globs: Vec<String>,
    pub verify_fns: BTreeMap<String, CheckFn>,
    pub builder_script: Option<PathBuf>,
    pub bug_reports_url: String,
    pub version_history_url: String,
    pub max_fix_retries: usize,
    pub max_package_retries: usize,
    pub max_review_retries: usize,
    pub timeout_s: Option<f64>,
    pub reviewer: Option<String>,
    pub tester: Option<String>,
}

impl Default for HistouryBugFixConfig {
    fn default() -> Self {
        Self {
            batch_id: String::new(),
            work_dir: ".".into(),
            repos: BTreeMap::new(),
            product: "desktop".into(),
            bugs: None,
            build_cmds: BTreeMap::new(),
            test_cmds: BTreeMap::new(),
            watch_globs: vec![],
            verify_fns: BTreeMap::new(),
            builder_script: None,
            bug_reports_url: "https://admin.histoury.com/bug-reports".into(),
            version_history_url: "https://admin.histoury.com/version-history".into(),
            max_fix_retries: 4,
            max_package_retries: 3,
            max_review_retries: 3,
            timeout_s: Some(300.0),
            reviewer: None,
            tester: None,
        }
    }
}

fn read(path: &Path) -> Result<Value, String> {
    read_object(path).map(Value::Object)
}
fn text<'a>(doc: &'a Value, field: &str) -> Result<&'a str, String> {
    doc[field]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("Provide a non-empty {field}"))
}
pub(crate) fn parse_command(value: &Value, key: &str) -> Result<Option<CommandSpec>, String> {
    let cmd = match value {
        Value::Null => None,
        Value::String(s) => Some(CommandSpec::Shell(s.clone())),
        v => Some(CommandSpec::Args(
            serde_json::from_value(v.clone())
                .map_err(|_| format!("{key} must be a command string or argv"))?,
        )),
    };
    command_json(cmd.as_ref(), key)?;
    Ok(cmd)
}

struct Batch {
    cfg: HistouryBugFixConfig,
    platforms: &'static [&'static str],
    reviewer: String,
    batch: PathBuf,
    evidence: PathBuf,
    media: PathBuf,
    review: PathBuf,
    feedback: PathBuf,
    manifest: PathBuf,
    summaries: [PathBuf; 2],
    config_hash: String,
}

impl Batch {
    fn selected(&self) -> Result<Vec<Value>, String> {
        let doc = read(&self.batch)?;
        if doc["batch_id"] != self.cfg.batch_id || doc["product"] != self.cfg.product {
            return Err(format!(
                "Expected batch_id {} and product {}",
                self.cfg.batch_id, self.cfg.product
            ));
        }
        let bugs = doc["bugs"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("Provide a non-empty bugs list")?;
        let mut ids = BTreeSet::new();
        bugs.iter()
            .cloned()
            .map(|mut bug| {
                if !bug.is_object() {
                    return Err("Each bug must be an object".into());
                }
                let id = require_bug_id(text(&bug, "id")?)?;
                if !ids.insert(id.to_ascii_lowercase()) {
                    return Err(format!("Duplicate bug ID {id}"));
                }
                if bug["repo"].is_null() && self.cfg.repos.len() == 1 {
                    bug["repo"] = json!(self.cfg.repos.keys().next())
                }
                if !bug["repo"]
                    .as_str()
                    .is_some_and(|r| self.cfg.repos.contains_key(r))
                {
                    return Err(format!(
                        "Bug {id}: choose a repo from {:?}",
                        self.cfg.repos.keys()
                    ));
                }
                for field in ["title", "description", "platform", "expected"] {
                    text(&bug, field).map_err(|e| format!("Bug {id}: {e}"))?;
                }
                if !self
                    .platforms
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(bug["platform"].as_str().unwrap().trim()))
                {
                    return Err(format!(
                        "Bug {id}: platform must be one of {}",
                        self.platforms.join(", ")
                    ));
                }
                if text(&bug, "repro_steps").is_err()
                    && !bug["repro_steps"].as_array().is_some_and(|steps| {
                        !steps.is_empty()
                            && steps
                                .iter()
                                .all(|s| s.as_str().is_some_and(|s| !s.trim().is_empty()))
                    })
                {
                    return Err(format!(
                        "Bug {id}: provide reproduction steps as text or a list of steps"
                    ));
                }
                if bug.get("requires_ui_capture").is_none() {
                    bug["requires_ui_capture"] = json!(true)
                }
                if !bug["requires_ui_capture"].is_boolean() {
                    return Err(format!("Bug {id}: requires_ui_capture must be a boolean"));
                }
                parse_command(&bug["repro_cmd"], "repro_cmd")?;
                Ok(bug)
            })
            .collect()
    }

    fn select(self: &Arc<Self>) -> Node {
        let fetch = if let Some(bugs) = &self.cfg.bugs {
            let doc = json!({"batch_id":self.cfg.batch_id,"product":self.cfg.product,"bugs":bugs});
            let hash = fingerprint(json!([self.config_hash, doc]));
            let batch = self.batch.clone();
            let mut n = TaskNode::new(
                "fetch_bug_reports",
                TaskKind::Code,
                Arc::new(move |_, _| {
                    write_json(&batch, &doc, true).map_err(TaskRunError::other)?;
                    Ok(Value::Null)
                }),
                Arc::new(move |_| Ok(hash.clone())),
            );
            n.outputs_fn = outputs(vec![self.batch.clone()]);
            n
        } else {
            let b = self.clone();
            let hash = self.config_hash.clone();
            packet_worker(PacketWorkerOptions {
                node_id: "fetch_bug_reports".into(),
                packet_path: self.cfg.work_dir.join("bug_reports_packet.json"),
                artifacts: outputs(vec![self.batch.clone()]),
                input_hash: Arc::new(move || Ok(hash.clone())),
                packet: Arc::new(move |_| {
                    Ok(json!({
                        "stage":"fetch_bug_reports","bug_reports_url":b.cfg.bug_reports_url,"batch_id":b.cfg.batch_id,
                        "product":b.cfg.product,"platforms":b.platforms,"repos":b.cfg.repos.keys().collect::<Vec<_>>(),
                        "instructions":["Select the assigned reports for this product from Admin and write bug_batch.json.",
                        "Include a repro_cmd if the workflow has no build, test command, or verifier for this bug."],
                        "schema":{"batch_id":b.cfg.batch_id,"product":b.cfg.product,"bugs":[{"id":"bug-123","title":"Bug title","description":"Observed behavior","platform":b.platforms[0],"repo":b.cfg.repos.keys().next(),"repro_steps":["Step to reproduce"],"expected":"Expected behavior","requires_ui_capture":true}]}
                    }))
                }),
                deps: vec![],
                repairs: None,
                enabled: None,
                verify: None,
                resource: None,
            })
        };
        let b = self.clone();
        let h = self.clone();
        let mut check = TaskNode::new(
            "verify_bug_batch",
            TaskKind::Verifier,
            Arc::new(move |_, _| {
                b.selected()
                    .map(|bugs| {
                        json!(format!(
                            "Selected {} bug reports for {}",
                            bugs.len(),
                            b.cfg.product
                        ))
                    })
                    .map_err(|e| {
                        TaskRunError::with_sig(
                            "invalid_bug_batch",
                            format!("invalid_bug_batch: {e}"),
                        )
                    })
            }),
            Arc::new(move |_| {
                Ok(fingerprint(json!([
                    h.config_hash,
                    file_hash(Some(&h.batch))
                ])))
            }),
        );
        check.verify_fn = Some(Arc::new(|_, v| {
            VerifyOutcome::pass(v.as_str().unwrap_or_default())
        }));
        check.deps = vec!["fetch_bug_reports".into()];
        Node::Subgraph(SubgraphNode::new(
            "select_bugs",
            vec![Node::Task(fetch), Node::Task(check)],
        ))
    }

    fn ui_bugs(&self) -> Result<Vec<Value>, String> {
        Ok(self
            .selected()?
            .into_iter()
            .filter(|b| b["requires_ui_capture"] == true)
            .collect())
    }
    fn screenshot(&self, id: &str, stage: &str) -> PathBuf {
        self.media.join(format!("{id}-{stage}.png"))
    }
    fn fix_dir(&self, id: &str) -> PathBuf {
        self.cfg.work_dir.join("bugs").join(id)
    }

    fn fix_splitter(self: &Arc<Self>) -> Node {
        let items = self.clone();
        let b = self.clone();
        let mut n = SplitterNode::new(
            "fix_bugs",
            Arc::new(move |_| {
                if items.batch.try_exists().map_err(|e| e.to_string())? {
                    items.selected()
                } else {
                    Ok(vec![])
                }
            }),
            Arc::new(move |bug, _| {
                let id = text(bug, "id")?;
                let repo = text(bug, "repo")?;
                let cfg = &b.cfg;
                let feedback = Value::Object(read_optional(&b.feedback)?);
                let child = create_desktop_bug_fix_subgraph(DesktopBugFixConfig {
                    bug_spec: bug.as_object().ok_or("Expected bug object")?.clone(),
                    work_dir: cfg.work_dir.clone(),
                    repo_dir: cfg.repos.get(repo).ok_or("Unknown repo")?.clone(),
                    repro_cmd: parse_command(&bug["repro_cmd"], "repro_cmd")?,
                    build_cmd: cfg
                        .build_cmds
                        .get(id)
                        .or_else(|| cfg.build_cmds.get(repo))
                        .cloned(),
                    test_cmd: cfg
                        .test_cmds
                        .get(id)
                        .or_else(|| cfg.test_cmds.get(repo))
                        .cloned(),
                    verify_fn: cfg
                        .verify_fns
                        .get(id)
                        .or_else(|| cfg.verify_fns.get(repo))
                        .cloned(),
                    watch_globs: cfg.watch_globs.clone(),
                    max_retries: cfg.max_fix_retries,
                    timeout_s: cfg.timeout_s,
                    review_feedback: feedback["fixes"][id].as_object().cloned(),
                    tester: cfg.tester.clone(),
                    ..Default::default()
                })?;
                Ok(Node::LoopSubgraph(child))
            }),
        );
        n.max_workers = 1;
        n.resource = Some("repo".into());
        Node::Splitter(n)
    }
}

struct Capture {
    batch: Arc<Batch>,
    stage: &'static str,
    worker: &'static str,
    revision_key: &'static str,
    provenance: PathBuf,
}
impl Capture {
    fn paths(&self) -> Result<Vec<PathBuf>, String> {
        let mut v = vec![self.provenance.clone()];
        for bug in self.batch.ui_bugs()? {
            v.push(self.batch.screenshot(text(&bug, "id")?, self.stage))
        }
        Ok(v)
    }
    fn hash(&self) -> Result<String, String> {
        let b = &self.batch;
        let mut paths = vec![b.batch.clone()];
        if self.stage == "after" {
            let bugs = b.ui_bugs()?;
            for bug in &bugs {
                paths.push(b.fix_dir(text(bug, "id")?).join("final_summary.json"))
            }
            for bug in &bugs {
                paths.push(b.screenshot(text(bug, "id")?, "before"))
            }
            paths.push(b.evidence.join("before_provenance.json"));
            if b.feedback.is_file() {
                paths.push(b.feedback.clone())
            }
        }
        Ok(files_hash(&paths))
    }
    fn check(&self, repairs: &Repairs) -> Result<VerifyOutcome, String> {
        let doc = match read(&self.provenance) {
            Ok(doc) => doc,
            Err(e) => return Ok(VerifyOutcome::fail("invalid_provenance", e)),
        };
        let expected = repairs.instructions()?["revision"]
            .as_u64()
            .unwrap_or_default();
        if !doc["revision"].as_u64().is_some_and(|r| r >= expected) || !doc["captures"].is_object()
        {
            return Ok(VerifyOutcome::fail(
                "invalid_provenance",
                "Provide the requested revision and a captures object",
            ));
        }
        let bugs = self.batch.ui_bugs()?;
        for bug in &bugs {
            let id = text(bug, "id")?;
            let capture = &doc["captures"][id];
            let path = self.batch.screenshot(id, self.stage);
            for key in [
                "path",
                self.revision_key,
                "platform",
                "captured_at",
                "screen_state",
            ] {
                if text(capture, key).is_err() {
                    return Ok(VerifyOutcome::fail(
                        "invalid_provenance",
                        format!("{id} needs {key} in {}", self.provenance.display()),
                    ));
                }
            }
            let real = resolve_path(&path)?;
            if resolve_path(&self.batch.cfg.work_dir.join(text(capture, "path")?))? != real
                || !real.starts_with(&self.batch.cfg.work_dir)
                || !text(capture, "platform")?
                    .trim()
                    .eq_ignore_ascii_case(text(bug, "platform")?.trim())
            {
                return Ok(VerifyOutcome::fail(
                    "invalid_provenance",
                    format!(
                        "{id}: capture must match path {} and platform {}",
                        path.display(),
                        text(bug, "platform")?
                    ),
                ));
            }
            if let Err(e) = inspect_png(&path) {
                return Ok(VerifyOutcome::fail(
                    format!("invalid_{}_png", self.stage),
                    format!("{}: {e}", path.display()),
                ));
            }
            if self.stage == "after"
                && sha256_file(&path)? == sha256_file(&self.batch.screenshot(id, "before"))?
            {
                return Ok(VerifyOutcome::fail(
                    "after_matches_before",
                    format!("{id}: capture the verified screen for {}", path.display()),
                ));
            }
        }
        Ok(VerifyOutcome::pass(format!(
            "Validated {} {} captures and their provenance",
            bugs.len(),
            self.stage
        )))
    }

    #[allow(clippy::result_large_err)] // LoopRepairFn returns GraphFailure by value.
    fn graph(b: &Arc<Batch>, stage: &'static str, dependency: &str) -> Node {
        let before = stage == "before";
        let c = Arc::new(Self {
            batch: b.clone(),
            stage,
            worker: if before {
                "capture_baseline"
            } else {
                "capture_verified_after"
            },
            revision_key: if before {
                "source_revision"
            } else {
                "verified_revision"
            },
            provenance: b.evidence.join(format!("{stage}_provenance.json")),
        });
        let h = c.clone();
        let source: StringFn = Arc::new(move || h.hash());
        let a = c.clone();
        let artifacts: StringFn = Arc::new(move || Ok(files_hash(&a.paths()?)));
        let repairs = Repairs::new(
            b.evidence.join(format!("{stage}_repair_state.json")),
            source.clone(),
            vec![],
            Some(c.provenance.clone()),
            Some(artifacts.clone()),
            0,
        );
        let out = c.clone();
        let pkt = c.clone();
        let mut worker = packet_worker(PacketWorkerOptions {
            node_id: c.worker.into(),
            packet_path: b.evidence.join(format!("{stage}_packet.json")),
            artifacts: Arc::new(move |_| out.paths()),
            input_hash: source.clone(),
            repairs: Some(repairs.clone()),
            deps: vec![],
            verify: None,
            enabled: None,
            resource: None,
            packet: Arc::new(move |_| {
                fs::create_dir_all(&pkt.batch.media).map_err(|e| e.to_string())?;
                Ok(
                    json!({"stage":pkt.worker,"bugs":pkt.batch.ui_bugs()?,"bug_batch_path":pkt.batch.batch,"media_dir":pkt.batch.media,
                    "provenance_path":pkt.provenance,"capture_fields":["path",pkt.revision_key,"platform","captured_at","screen_state"],
                    "instructions":[if before {"Capture the affected runtime screen before the first edit. Preserve this baseline."}else{"Capture the verified fix on the same platform, viewport, and screen state as the baseline."},
                    "Use genuine runtime screenshots and exclude secrets and personal information.",
                    format!("Save media/<bug_id>-{stage}.png and write the provenance with the requested integer revision and a captures object keyed by bug ID.")] }),
                )
            }),
        });
        let run = worker.run_fn;
        let run_capture = c.clone();
        worker.run_fn = Arc::new(move |ctx, attempt| {
            if run_capture
                .batch
                .ui_bugs()
                .map_err(TaskRunError::other)?
                .is_empty()
            {
                write_json(
                    &run_capture.provenance,
                    &json!({"revision":0,"captures":{}}),
                    true,
                )
                .map_err(TaskRunError::other)?;
                Ok(json!("Batch has no UI captures"))
            } else {
                run(ctx, attempt)
            }
        });
        let prior = worker.outputs_fn;
        let out = c.clone();
        worker.outputs_fn = Arc::new(move |ctx| {
            if out.batch.ui_bugs()?.is_empty() {
                Ok(vec![out.provenance.clone()])
            } else {
                prior(ctx)
            }
        });
        let result = b.evidence.join(format!("{stage}_result.json"));
        let verifier = format!("verify_{stage}");
        let r = repairs.clone();
        let check_repairs = repairs.clone();
        let check = c.clone();
        let verify = record_verification(
            &verifier,
            result.clone(),
            Arc::new(move || {
                Ok(fingerprint(json!([
                    source()?,
                    artifacts()?,
                    r.instructions()?["revision"]
                ])))
            }),
            Arc::new(move |_| check.check(&check_repairs)),
            vec![c.worker.into()],
            None,
            true,
        );
        let r = repairs.clone();
        let mut n = LoopSubgraphNode::new(
            format!("capture_{stage}"),
            vec![Node::Task(worker), Node::Task(verify)],
            Arc::new(move |_| {
                let verdict = verification(&result)?;
                if verdict.ok {
                    r.resolved()?
                }
                Ok(verdict)
            }),
        );
        n.deps = vec![dependency.into()];
        n.max_retries = 3;
        let v = verifier.clone();
        let worker = c.worker;
        n.repair_fn = Some(Arc::new(move |ctx, _, sig, detail| {
            repairs.repair(ctx, sig, detail, &[worker, &v], 3)
        }));
        Node::LoopSubgraph(with_retry_route(
            n,
            verifier,
            worker,
            "if fails → recapture",
        ))
    }
}
impl Batch {
    fn fix_paths(&self, id: &str) -> [PathBuf; 4] {
        let dir = self.fix_dir(id);
        [
            "final_summary.json",
            "build_result.json",
            "test_result.json",
            "verification.json",
        ]
        .map(|name| dir.join(name))
    }

    fn fix_evidence(&self, bug: &Value, fix: &Value) -> Result<Value, String> {
        let repo = &self.cfg.repos[text(bug, "repo")?];
        let mut changed = Map::new();
        let mut logs = Map::new();
        for name in fix["changed_files"]
            .as_array()
            .ok_or("Missing changed_files in fix summary")?
        {
            let name = name.as_str().ok_or("Invalid changed file")?;
            let path = resolve_path(&repo.join(name))?;
            if path.starts_with(repo) {
                changed.insert(name.into(), json!(file_hash(Some(&path))));
            }
        }
        for (name, path) in fix["log_paths"]
            .as_object()
            .ok_or("Missing log_paths in fix summary")?
        {
            logs.insert(
                name.clone(),
                json!(file_hash(Some(Path::new(
                    path.as_str().ok_or("Invalid log path")?
                )))),
            );
        }
        Ok(
            json!({"changed_file_sha256":changed,"watched_source_sha256":hash_globs(&self.cfg.watch_globs,Some(repo),None,&[],true)?,"log_sha256":logs}),
        )
    }

    fn review_hash_inputs(&self) -> Result<String, String> {
        let mut paths = vec![
            self.batch.clone(),
            self.manifest.clone(),
            self.evidence.join("before_provenance.json"),
            self.evidence.join("after_provenance.json"),
        ];
        let mut evidence = vec![];
        let bugs = self.selected()?;
        for bug in &bugs {
            let sources = self.fix_paths(text(bug, "id")?);
            evidence.push(self.fix_evidence(bug, &read(&sources[0])?)?);
            paths.extend(sources);
        }
        for bug in &bugs {
            if bug["requires_ui_capture"] == true {
                for stage in ["before", "after"] {
                    paths.push(self.screenshot(text(bug, "id")?, stage))
                }
            }
        }
        paths.push(text(&read(&self.manifest)?, "zip_path")?.into());
        if self.feedback.is_file() {
            paths.push(self.feedback.clone())
        }
        Ok(qa_hash(
            vec![
                json!("qa-v1"),
                json!(self.config_hash),
                json!(self.cfg.version_history_url),
                json!(files_hash(&paths)),
                json!(evidence),
            ],
            self.cfg.tester.as_deref(),
        ))
    }

    fn prepare_review(&self) -> Result<(), String> {
        let bugs = self.selected()?;
        let pkg = read(&self.manifest)?;
        let zip = text(&pkg, "zip_path")?;
        let digest = text(&pkg, "zip_sha256")?;
        if sha256_file(Path::new(zip))? != digest {
            return Err("ZIP bytes do not match package_manifest.json".into());
        }
        let touched: BTreeMap<_, _> = self
            .cfg
            .repos
            .iter()
            .filter(|(name, _)| {
                bugs.iter()
                    .any(|b| b["repo"].as_str() == Some(name.as_str()))
            })
            .collect();
        let round = read_optional(&self.feedback)?
            .get("round")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let title = format!("Histoury bug-fix review for {}", self.cfg.batch_id);
        let mut lines = vec![
            format!("# {title}"),
            String::new(),
            format!("Product: {}", self.cfg.product),
        ];
        if round > 0 {
            lines.push(format!("Review round: {}", round + 1))
        }
        lines.push(format!("\n## Version History ZIP\n\n- ZIP: `{zip}`\n- SHA-256: `{digest}`\n- Admin page: {}\n\n## Repositories\n",self.cfg.version_history_url));
        for (repo, path) in &touched {
            lines.push(format!("- {repo}: `{}`", path.display()))
        }
        let mut fixes = vec![];
        let mut cases = vec![];
        for bug in bugs {
            let id = text(&bug, "id")?;
            let paths = self.fix_paths(id);
            let fix = read(&paths[0])?;
            let build = read(&paths[1])?;
            let tests = read(&paths[2])?;
            let check = read(&paths[3])?;
            let mut item = self.fix_evidence(&bug, &fix)?;
            lines.push(format!("\n## {id} — {}\n\n{}\n\n- Repository: {}\n- Build: {}\n- Tests: {}\n- Verification: {}\n- Verified on attempt {} (revision {})",
                text(&bug,"title")?,text(&fix,"summary")?,text(&bug,"repo")?,text(&build,"detail")?,text(&tests,"detail")?,text(&check,"detail")?,fix["attempts"],fix["revision"]));
            for (name, path) in fix["log_paths"].as_object().unwrap() {
                lines.push(format!("- {name} log: `{}`", path.as_str().unwrap()))
            }
            for name in fix["changed_files"].as_array().unwrap() {
                lines.push(format!("- Changed file: `{}`", name.as_str().unwrap()))
            }
            if bug["requires_ui_capture"] == true {
                for (stage, label) in [("before", "Before"), ("after", "After")] {
                    let path = self.screenshot(id, stage);
                    item["screenshots"][stage] = json!({"path":path,"sha256":sha256_file(&path)?});
                    lines.push(format!("- {label} screenshot: `{}`", path.display()));
                }
            }
            cases.push(bug_qa_case(
                bug.as_object().unwrap(),
                fix.as_object().unwrap(),
            ));
            item["bug"] = bug;
            item["fix"] = fix;
            item["build"] = build;
            item["tests"] = tests;
            item["verification"] = check;
            fixes.push(item);
        }
        let qa = self.review.join("manual_qa.json");
        init_qa_checklist(
            &qa,
            &self.cfg.batch_id,
            &cases,
            "histoury-bug-fix",
            Some(&title),
            self.cfg.tester.as_deref(),
            None,
            None,
            true,
        )?;
        write_qa_scorecard(&qa, None, None, false, None)?;
        lines.push("\n## Manual re-test\n\n[QA Scorecard](qa_scorecard.md) · [Checklist](manual_qa.json)\n".into());
        let mut summary = json!({"batch_id":self.cfg.batch_id,"product":self.cfg.product,"bugs":fixes,"touched_repos":touched,
            "before_provenance":read(&self.evidence.join("before_provenance.json"))?,"after_provenance":read(&self.evidence.join("after_provenance.json"))?,
            "package":pkg,"version_history_url":self.cfg.version_history_url});
        if round > 0 {
            summary["review_round"] = json!(round + 1)
        }
        write_json(&self.summaries[0], &summary, true)?;
        write_text(&self.summaries[1], &lines.join("\n"))
    }
    fn prepare_node(self: &Arc<Self>) -> Node {
        let run = self.clone();
        let hash = self.clone();
        let mut n = TaskNode::new(
            "prepare_review_bundle",
            TaskKind::Code,
            Arc::new(move |_, _| {
                run.prepare_review().map_err(TaskRunError::other)?;
                Ok(Value::Null)
            }),
            Arc::new(move |_| hash.review_hash_inputs()),
        );
        n.outputs_fn = outputs(self.summaries.to_vec());
        n.deps = vec!["version_history_package".into()];
        Node::Task(n)
    }
}

fn qa_results(raw: &Value) -> Result<Vec<QaResultInput>, String> {
    let entries: Vec<(&str, &Value)> = match raw {
        Value::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .map(|v| Ok((text(v, "id").or_else(|_| text(v, "case_id"))?, v)))
            .collect::<Result<_, String>>()?,
        _ => return Err("manual_qa must be a mapping or list of QA results".into()),
    };
    let mut seen = BTreeSet::new();
    entries.into_iter().map(|(id,v)|{
        if id.trim().is_empty() || !seen.insert(id){return Err("manual_qa results must identify unique bug IDs".into())}
        let result=if let Some(s)=v.as_str(){json!({"manual_qa":s})}else{v.clone()};
        serde_json::from_value(json!({"case_id":id,"manual_qa":result.get("manual_qa").unwrap_or(&result["result"]),
            "resolved_on_attempt":result["resolved_on_attempt"],"gap_type":result["gap_type"],"notes":result["notes"]})).map_err(|e|e.to_string())
    }).collect()
}

struct Decision {
    doc: Value,
    verdict: VerifyOutcome,
    feedback: Option<Value>,
}
fn failure(sig: &str, detail: impl std::fmt::Display) -> VerifyOutcome {
    VerifyOutcome::fail(sig, format!("{sig}: {detail}"))
}
fn store_context(ctx: &Context, key: &str, path: &Path, value: Value) {
    let mut state = ctx.0.write().unwrap_or_else(|e| e.into_inner());
    let map = state.entry(key).or_insert_with(|| json!({}));
    if !map.is_object() {
        *map = json!({})
    }
    map[path.to_string_lossy().as_ref()] = value;
}
impl Batch {
    fn approval(&self) -> PathBuf {
        self.review.join("approval.json")
    }
    fn approval_packet(&self) -> PathBuf {
        self.review.join("user_approval_packet.json")
    }
    fn stale(&self, current: &str, review_hash: &str) -> Result<bool, String> {
        let prior = Value::Object(read_optional(&self.approval_packet())?);
        Ok(prior["review_sha256"] != review_hash
            || prior["stale_approval_sha256"] == current
            || read_optional(&self.feedback)?
                .get("rejected_approval_sha256")
                .and_then(Value::as_str)
                == Some(current))
    }
    fn review_packet(&self) -> Result<Value, String> {
        let hash = files_hash(&self.summaries);
        let stale = match file_hash(Some(&self.approval())) {
            Some(s) if self.stale(&s, &hash)? => Some(s),
            _ => None,
        };
        let bugs: Vec<_> = self
            .selected()?
            .iter()
            .map(|bug| json!({"id":bug["id"],"title":bug["title"]}))
            .collect();
        Ok(
            json!({"stage":"user_verify_gate","review_summary":self.summaries[1],"review_summary_json":self.summaries[0],
            "review_sha256":hash,"manual_qa_checklist":self.review.join("manual_qa.json"),"qa_scorecard":self.review.join("qa_scorecard.md"),
            "bugs":bugs,"stale_approval_sha256":stale,"outputs":[self.approval()],
            "instructions":[format!("{} must verify the fixes, before and after screenshots, and ZIP in review_summary.md.",self.reviewer),
            "Write approval.json with your decisions and review_sha256 from this packet. Set either action to false to skip it.",
            "Record your re-test in manual_qa.json or include a manual_qa mapping of bug IDs to results.",
            "To request fixes, set approved to false and optionally include notes and rejected_bugs. With no selected bugs or manual failures, the whole batch returns for fixes."],
            "approval_example":{"approved":true,"approved_by":self.reviewer,"approve_upload_zip":true,"approve_commit_main":true,"notes":"","review_sha256":hash}}),
        )
    }

    fn decision(&self, sync: bool) -> Result<Decision, String> {
        let invalid = |e: String| failure("invalid_approval", e).detail;
        let raw = fs::read(self.approval()).map_err(|e| invalid(e.to_string()))?;
        let doc: Value = serde_json::from_slice(&raw).map_err(|e| invalid(e.to_string()))?;
        let approved = doc["approved"]
            .as_bool()
            .ok_or("invalid_approval: Provide an approved boolean")?;
        let by = if approved {
            doc.get("approved_by")
        } else {
            doc.get("rejected_by").or_else(|| doc.get("approved_by"))
        };
        let by = match by {
            Some(v) => v.as_str(),
            None if !approved => Some(self.reviewer.as_str()),
            _ => None,
        }
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("invalid_approval: Provide a reviewer name")?;
        let notes = doc
            .get("notes")
            .map_or(Some(""), Value::as_str)
            .ok_or("invalid_approval: Provide text notes")?;
        for action in ["approve_upload_zip", "approve_commit_main"] {
            if doc.get(action).is_some_and(|v| !v.is_boolean()) {
                return Err(format!("invalid_approval: {action} must be boolean"));
            }
        }
        let mut rejected = vec![];
        let mut selected = vec![];
        if !approved {
            for bug in self.selected()? {
                selected.push(text(&bug, "id")?.to_owned())
            }
            if let Some(ids) = doc.get("rejected_bugs") {
                for id in ids
                    .as_array()
                    .ok_or("invalid_approval: rejected_bugs must list selected bug IDs")?
                {
                    let id = id
                        .as_str()
                        .ok_or("invalid_approval: rejected_bugs must list selected bug IDs")?
                        .to_owned();
                    if !selected.contains(&id) {
                        return Err(
                            "invalid_approval: rejected_bugs must list selected bug IDs".into()
                        );
                    }
                    if !rejected.contains(&id) {
                        rejected.push(id)
                    }
                }
            }
        }
        let review_hash = files_hash(&self.summaries);
        let current = sha256_bytes(&raw);
        let matches = doc
            .get("review_sha256")
            .map_or(!self.feedback.is_file(), |h| h == &review_hash);
        if !matches || self.stale(&current, &review_hash)? {
            return Err(
                "stale_approval: Review the current bundle and write a fresh approval.".into(),
            );
        }
        let mut failures = Map::new();
        if sync {
            let results = doc
                .get("manual_qa")
                .map(qa_results)
                .transpose()
                .map_err(invalid)?
                .unwrap_or_default();
            let checklist = record_qa_results(
                &self.review.join("manual_qa.json"),
                &results,
                (!results.is_empty()).then_some(by),
            )
            .map_err(invalid)?;
            for case in checklist
                .cases
                .into_iter()
                .filter(|c| c.manual_qa == "fail")
            {
                if !rejected.contains(&case.id) {
                    rejected.push(case.id.clone())
                }
                failures.insert(case.id.clone(), json!(case));
            }
        }
        let (verdict, feedback) = if approved {
            (
                VerifyOutcome::pass(format!("Release review approved by {by}")),
                None,
            )
        } else {
            (
                failure(
                    "user_rejected",
                    format!("{by} requested fixes before release."),
                ),
                Some(json!({"notes":notes,
                "rejected_bugs":if rejected.is_empty(){selected}else{rejected},"manual_qa":failures,"rejected_by":by,"rejected_approval_sha256":current})),
            )
        };
        Ok(Decision {
            doc,
            verdict,
            feedback,
        })
    }

    fn gate(self: &Arc<Self>) -> Node {
        let p = self.clone();
        let h = self.clone();
        let v = self.clone();
        let mut opts = SubagentGateOptions::new(
            "user_verify_gate",
            self.approval_packet(),
            outputs(vec![self.approval()]),
            Arc::new(move |_| p.review_packet()),
            Arc::new(move |_| {
                let mut paths = h.summaries.to_vec();
                paths.extend([h.approval_packet(), h.approval()]);
                Ok(files_hash(&paths))
            }),
        );
        opts.verify_fn = Some(Arc::new(move |ctx, _| {
            let (verdict, feedback) = match v.decision(true) {
                Ok(d) => (d.verdict, d.feedback),
                Err(e) => (
                    VerifyOutcome::fail(
                        e.split(':')
                            .next()
                            .unwrap_or("invalid_approval")
                            .to_string(),
                        e,
                    ),
                    None,
                ),
            };
            store_context(
                ctx,
                "verification_results",
                &v.approval(),
                json!([verdict.ok, verdict.error_signature, verdict.detail]),
            );
            if let Some(f) = feedback {
                store_context(ctx, "repair_feedback", &v.approval(), f);
                VerifyOutcome::pass(verdict.detail)
            } else {
                verdict
            }
        }));
        opts.deps = vec!["prepare_review_bundle".into()];
        opts.cacheable = false;
        let mut n = subagent_gate(opts);
        let run = n.run_fn;
        let checklist = self.review.join("manual_qa.json");
        n.run_fn = Arc::new(move |ctx, attempt| {
            let result = run(ctx, attempt);
            let score = write_qa_scorecard(&checklist, None, None, false, None);
            if result.is_ok() {
                score.map_err(TaskRunError::other)?;
            }
            result
        });
        Node::Task(n)
    }
    fn review_outcome(&self, ctx: &Context) -> Result<VerifyOutcome, String> {
        let all = ctx
            .get("verification_results")
            .ok_or("Missing review verification")?;
        let (ok, error_signature, detail) =
            serde_json::from_value(all[self.approval().to_string_lossy().as_ref()].clone())
                .map_err(|e| e.to_string())?;
        Ok(VerifyOutcome {
            ok,
            error_signature,
            detail,
        })
    }
    #[allow(clippy::result_large_err)] // LoopRepairFn returns GraphFailure by value.
    fn repair_review(
        &self,
        ctx: &Context,
        sig: &str,
        detail: &str,
    ) -> Result<Vec<String>, GraphFailure> {
        let error = |e: String| GraphFailure::new("fix_and_review_loop", e);
        let previous = Value::Object(read_optional(&self.feedback).map_err(error)?);
        let round = previous["round"].as_u64().unwrap_or(0) + 1;
        if round > self.cfg.max_review_retries as u64 {
            return Err(error(format!(
                "Exhausted {} review retries ({sig}): {detail}",
                self.cfg.max_review_retries
            )));
        }
        let mut feedback = ctx
            .get("repair_feedback")
            .and_then(|v| v.get(self.approval().to_string_lossy().as_ref()).cloned())
            .ok_or_else(|| error("Missing rejection feedback".into()))?;
        feedback["round"] = json!(round);
        let mut fixes = previous.get("fixes").cloned().unwrap_or_else(|| json!({}));
        for id in feedback["rejected_bugs"]
            .as_array()
            .ok_or_else(|| error("Missing rejected bugs".into()))?
        {
            let id = id
                .as_str()
                .ok_or_else(|| error("Invalid rejected bug ID".into()))?;
            let mut repair = feedback.clone();
            repair["fix_revision"] =
                json!(revision(&self.fix_dir(id).join("fix_report.json")).map_err(error)? + 1);
            repair["review_attempts"] = json!(
                fixes[id]["review_attempts"]
                    .as_u64()
                    .unwrap_or(if fixes[id].is_null() { 0 } else { 1 })
                    + 1
            );
            repair["manual_qa"] = feedback["manual_qa"][id].clone();
            fixes[id] = repair;
        }
        feedback["fixes"] = fixes;
        write_json(&self.feedback, &feedback, true).map_err(error)?;
        Ok(vec!["fix_bugs".into()])
    }
}

#[derive(Clone, Copy)]
enum ReleaseAction {
    Upload,
    Commit,
}
impl ReleaseAction {
    fn names(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Upload => (
                "upload_version_history",
                "upload",
                "approve_upload_zip",
                "Not approved for upload",
            ),
            Self::Commit => (
                "commit_to_main",
                "commit",
                "approve_commit_main",
                "Not approved for commit to main",
            ),
        }
    }
    fn packet(self, b: &Batch) -> Result<Value, String> {
        let review = read(&b.summaries[0])?;
        let pkg = &review["package"];
        Ok(match self {
            Self::Upload => {
                json!({"stage":self.names().0,"version_history_url":b.cfg.version_history_url,
                "release_id":pkg["release_id"],"zip_path":pkg["zip_path"],"zip_sha256":pkg["zip_sha256"],
                "instructions":["Preview this exact ZIP using the authorized Admin account.",
                "Confirm a real signed-in display name. Refresh sign-in if it shows User.","Publish and verify the persisted release and uploader attribution.",
                "On resume, check for an existing release first.","Write upload_receipt.json with previewed, published, release_id, and uploader_name."]})
            }
            Self::Commit => {
                json!({"stage":self.names().0,"touched_repos":review["touched_repos"],"branch":"main",
                "instructions":["Run git diff --check and land only the reviewed fixes through each repository's authorized main branch workflow.",
                    "On resume, confirm the existing main commit before creating another.","Push only with explicit approval for pushing.",
                    "Write commit_receipt.json with committed true and a commits list containing repo, branch, and sha."]})
            }
        })
    }
    fn verify(self, b: &Batch, path: &Path) -> Result<VerifyOutcome, String> {
        let review = read(&b.summaries[0])?;
        let sig = match self {
            Self::Upload => "upload_incomplete",
            Self::Commit => "commit_incomplete",
        };
        let doc = match read(path) {
            Ok(d) => d,
            Err(e) => return Ok(failure(sig, e)),
        };
        Ok(match self {
            Self::Upload => {
                if doc["previewed"] != true
                    || doc["published"] != true
                    || doc["release_id"] != review["package"]["release_id"]
                {
                    failure(
                        sig,
                        "Confirm the preview and publication for this release ID.",
                    )
                } else if let Some(name) = text(&doc, "uploader_name")
                    .ok()
                    .map(str::trim)
                    .filter(|name| !name.eq_ignore_ascii_case("user"))
                {
                    VerifyOutcome::pass(format!("Published Version History as {name}"))
                } else {
                    failure(
                        "invalid_uploader_attribution",
                        "Confirm the real Admin uploader name.",
                    )
                }
            }
            Self::Commit => {
                let commits = doc["commits"].as_array();
                let touched: BTreeSet<_> = review["touched_repos"]
                    .as_object()
                    .ok_or("Missing touched_repos")?
                    .keys()
                    .map(String::as_str)
                    .collect();
                if let Some(commits) = commits.filter(|c| {
                    doc["committed"] == true
                        && !c.is_empty()
                        && c.iter().all(|c| {
                            c["branch"] == "main"
                                && text(c, "sha").is_ok()
                                && text(c, "repo").is_ok()
                        })
                        && c.iter()
                            .filter_map(|c| c["repo"].as_str())
                            .collect::<BTreeSet<_>>()
                            == touched
                }) {
                    VerifyOutcome::pass(format!(
                        "Recorded {} reviewed commits to main",
                        commits.len()
                    ))
                } else {
                    failure(
                        sig,
                        "Provide main commit SHAs for the reviewed repositories.",
                    )
                }
            }
        })
    }
    fn approved(self, b: &Batch) -> Result<bool, String> {
        let d = b.decision(false)?;
        if !d.verdict.ok {
            return Err(d.verdict.detail);
        }
        Ok(d.doc[self.names().2].as_bool().unwrap_or(true))
    }
    fn node(self, b: &Arc<Batch>) -> Node {
        let (id, name, _, reason) = self.names();
        let commit = matches!(self, Self::Commit);
        let receipt = b.review.join(format!("{name}_receipt.json"));
        let hash = b.clone();
        let packet = b.clone();
        let verify = b.clone();
        let path = receipt.clone();
        let mut n = packet_worker(PacketWorkerOptions {
            node_id: id.into(),
            packet_path: b.review.join(format!("{name}_packet.json")),
            artifacts: outputs(vec![receipt.clone()]),
            input_hash: Arc::new(move || Ok(files_hash(&hash.summaries))),
            packet: Arc::new(move |_| self.packet(&packet)),
            deps: if commit {
                vec!["upload_version_history".into()]
            } else {
                vec![]
            },
            resource: commit.then(|| "repo".into()),
            enabled: None,
            repairs: None,
            verify: Some(Arc::new(move |_, _| {
                self.verify(&verify, &path)
                    .unwrap_or_else(|e| failure("invalid_receipt", e))
            })),
        });
        let run = n.run_fn;
        let batch = b.clone();
        let out = receipt.clone();
        n.run_fn = Arc::new(move |ctx, attempt| {
            if self.approved(&batch).map_err(TaskRunError::other)? {
                run(ctx, attempt)
            } else {
                write_json(&out, &json!({"skipped":true,"reason":reason}), true)
                    .map_err(TaskRunError::other)?;
                Ok(json!(reason))
            }
        });
        let hash = n.input_hash_fn;
        let batch = b.clone();
        n.input_hash_fn =
            Arc::new(move |ctx| Ok(fingerprint(json!([hash(ctx)?, self.approved(&batch)?]))));
        let out = n.outputs_fn;
        let batch = b.clone();
        n.outputs_fn = Arc::new(move |ctx| {
            if self.approved(&batch)? {
                out(ctx)
            } else {
                Ok(vec![receipt.clone()])
            }
        });
        Node::Task(n)
    }
}

#[allow(clippy::result_large_err)] // LoopRepairFn returns GraphFailure by value.
pub fn create_histoury_bug_fix_graph(mut cfg: HistouryBugFixConfig) -> Result<Vec<Node>, String> {
    require_release_id(&require_bug_id(&cfg.batch_id)?)?;
    let platforms: &[&str] = match cfg.product.as_str() {
        "desktop" => &["WebGL", "Windows", "macOS"],
        "app" => &["iOS", "Android"],
        _ => return Err("product must be desktop or app".into()),
    };
    if cfg.repos.is_empty() || cfg.repos.keys().any(|k| k.trim().is_empty()) {
        return Err("repos must be a non-empty mapping from repository names to paths".into());
    }
    for path in cfg.repos.values_mut() {
        *path = resolve_path(path)?
    }
    cfg.work_dir = resolve_path(&cfg.work_dir)?;
    cfg.tester = configured_tester(cfg.tester.as_deref())?;
    let reviewer = cfg
        .reviewer
        .as_deref()
        .or(cfg.tester.as_deref())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(default_tester);
    if reviewer.is_empty() {
        return Err("reviewer must be non-empty text".into());
    }
    let work = &cfg.work_dir;
    let review = work.join("review");
    let batch = Arc::new(Batch {
        platforms,
        reviewer,
        batch: work.join("bug_batch.json"),
        evidence: work.join("evidence"),
        media: work.join("media"),
        manifest: work.join("version_history/package_manifest.json"),
        summaries: [
            review.join("review_summary.json"),
            review.join("review_summary.md"),
        ],
        feedback: review.join("review_feedback.json"),
        review,
        config_hash: fingerprint(json!([
            cfg.batch_id,
            cfg.product,
            cfg.repos,
            cfg.bug_reports_url
        ])),
        cfg,
    });
    let pkg = create_version_history_subgraph(VersionHistoryConfig {
        work_dir: batch.cfg.work_dir.clone(),
        product: batch.cfg.product.clone(),
        release_id: Some(batch.cfg.batch_id.clone()),
        changes_manifest_path: Some(batch.batch.clone()),
        media_dir: Some(batch.media.clone()),
        builder_script: batch.cfg.builder_script.clone(),
        max_retries: batch.cfg.max_package_retries,
        deps: vec!["capture_after".into()],
        source_paths: vec![batch.feedback.clone()],
        ..Default::default()
    })?;
    let verify = batch.clone();
    let repair = batch.clone();
    let mut review_loop = LoopSubgraphNode::new(
        "fix_and_review_loop",
        vec![
            batch.fix_splitter(),
            Capture::graph(&batch, "after", "fix_bugs"),
            Node::LoopSubgraph(pkg),
            batch.prepare_node(),
            batch.gate(),
        ],
        Arc::new(move |ctx| verify.review_outcome(ctx)),
    );
    review_loop.max_retries = batch.cfg.max_review_retries;
    review_loop.deps = vec!["capture_before".into()];
    review_loop.qa_checklist_path = Some(batch.review.join("manual_qa.json"));
    review_loop.repair_fn = Some(Arc::new(move |ctx, _, sig, detail| {
        repair.repair_review(ctx, sig, detail)
    }));
    let mut release = SubgraphNode::new(
        "release",
        vec![
            ReleaseAction::Upload.node(&batch),
            ReleaseAction::Commit.node(&batch),
        ],
    );
    release.deps = vec!["fix_and_review_loop".into()];
    Ok(vec![
        batch.select(),
        Capture::graph(&batch, "before", "select_bugs"),
        Node::LoopSubgraph(with_retry_route(
            review_loop,
            "user_verify_gate",
            "fix_bugs",
            "if rejected → re-fix",
        )),
        Node::Subgraph(release),
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::engine::{GraphRunner, RunnerConfig};
    use tempfile::TempDir;

    fn bug(id: &str, ui: bool) -> Map<String, Value> {
        json!({"id":id,"title":format!("Restore {id} settings"),"description":"Saved value lost","platform":"macOS","expected":"Restores saved value",
            "repro_steps":["Save settings","Reopen"],"requires_ui_capture":ui}).as_object().unwrap().clone()
    }
    fn png(after: bool) -> Vec<u8> {
        let idat = if after {
            "6338615e0d000345017b7dd3f70e"
        } else {
            "634831af060002190117e9869b71"
        };
        let hex=format!("89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de0000000c49444154789c{idat}0000000049454e44ae426082");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    struct Fixture {
        root: TempDir,
        work: PathBuf,
        repo: PathBuf,
        cfg: HistouryBugFixConfig,
    }
    impl Fixture {
        fn new(bugs: Option<Vec<Map<String, Value>>>) -> Self {
            let root = TempDir::new().unwrap();
            let dir = root.path().canonicalize().unwrap();
            let repo = dir.join("repo");
            let work = dir.join("work");
            fs::create_dir(&repo).unwrap();
            let builder = dir.join("test_builder.py");
            // Use a controlled ZIP builder even when the user's Admin skill is installed.
            fs::write(&builder,"import sys, zipfile\nfrom pathlib import Path\nif sys.argv[1]=='build':\n    root=Path(sys.argv[2]).parent\n    with zipfile.ZipFile(sys.argv[-1], 'w') as archive:\n        for name in ['release.json','media/before.png','media/after.png']:\n            archive.write(root/name,name)\n").unwrap();
            let verify_repo = repo.clone();
            let verify: CheckFn = Arc::new(move |_| {
                Ok(
                    if fs::read_to_string(verify_repo.join("settings"))
                        .as_deref()
                        .ok()
                        == Some("correct")
                    {
                        VerifyOutcome::pass("settings verified")
                    } else {
                        VerifyOutcome::fail("settings_reset", "saved value was lost")
                    },
                )
            });
            let cfg = HistouryBugFixConfig {
                batch_id: "desktop-42".into(),
                work_dir: work.clone(),
                repos: BTreeMap::from([("client".into(), repo.clone())]),
                bugs,
                watch_globs: vec!["settings".into()],
                verify_fns: BTreeMap::from([("client".into(), verify)]),
                builder_script: Some(builder),
                tester: Some("Sam".into()),
                reviewer: Some("Alex".into()),
                ..Default::default()
            };
            Self {
                root,
                work,
                repo,
                cfg,
            }
        }
        fn run(&self) -> Result<Context, Box<GraphFailure>> {
            let mut cfg = RunnerConfig::new(self.root.path().join("state.json"));
            cfg.logger = Some(Arc::new(|_| {}));
            GraphRunner::new(cfg)
                .unwrap()
                .run(
                    &create_histoury_bug_fix_graph(self.cfg.clone()).unwrap(),
                    &Context::new(),
                    "",
                )
                .map_err(Box::new)
        }
        fn wait(&self, stage: &str) -> (Value, String) {
            let err = self.run().unwrap_err();
            assert!(err.awaiting_subagents, "{err}");
            let packet = read(Path::new(err.packet_path.as_deref().unwrap())).unwrap();
            assert_eq!(packet["stage"], stage, "{err}");
            (packet, err.reason)
        }
        fn doc(&self, relative: &str) -> Value {
            read(&self.work.join(relative)).unwrap()
        }
        fn save(&self, relative: &str, doc: Value) {
            write_json(&self.work.join(relative), &doc, true).unwrap()
        }
        fn fix(&self, packet: &Value, settings: &str) {
            fs::write(self.repo.join("settings"), settings).unwrap();
            write_json(Path::new(packet["outputs"][0].as_str().unwrap()),&json!({"revision":packet["revision"],"summary":"Load saved settings","root_cause":"Defaults overwrote the save","changed_files":["settings"]}),true).unwrap();
        }
        fn capture(&self, packet: &Value, stage: &str, after: bool) {
            let path = self.work.join(format!("media/a-{stage}.png"));
            fs::write(&path, png(after)).unwrap();
            let key = if stage == "before" {
                "source_revision"
            } else {
                "verified_revision"
            };
            self.save(&format!("evidence/{stage}_provenance.json"),json!({"revision":packet["revision"],"captures":{"a":{"path":path,(key):"head","platform":"macOS","captured_at":"2026-11-24","screen_state":"Settings reopened"}}}));
        }
        fn author(&self, packet: &Value, summary: &str) {
            let dir = Path::new(packet["pkg_dir"].as_str().unwrap());
            fs::create_dir_all(dir.join("media")).unwrap();
            for (name, after) in [("before", false), ("after", true)] {
                fs::write(dir.join(format!("media/{name}.png")), png(after)).unwrap()
            }
            let changes:Vec<_>=self.doc("bug_batch.json")["bugs"].as_array().unwrap().iter().map(|bug|json!({"id":bug["id"],"title":bug["title"],"type":"fix",
                "description":"Restored settings","verification":"Verified saved value","evidence":[{"kind":"image","stage":"before","url":"media/before.png","label":"Before"},{"kind":"image","stage":"after","url":"media/after.png","label":"After"}]})).collect();
            write_json(&dir.join("release.json"),&json!({"schema_version":2,"product":"desktop","release_id":"desktop-42","version":"1.0.42","title":"Desktop maintenance",
                "stage":"source","date":"2026-11-24","summary":summary,"changes":changes}),true).unwrap();
            write_json(
                &dir.join("author_meta.json"),
                &json!({"revision":packet["revision"],"release_id":"desktop-42"}),
                true,
            )
            .unwrap();
        }
        fn assert_unreleased(&self) {
            for name in [
                "upload_packet",
                "commit_packet",
                "upload_receipt",
                "commit_receipt",
            ] {
                assert!(!self.work.join(format!("review/{name}.json")).exists())
            }
        }
    }

    #[test]
    fn full_flow_repairs_evidence_and_fix_then_validates_approved_release_receipts() {
        let f = Fixture::new(None);
        let (p, _) = f.wait("fetch_bug_reports");
        assert_eq!(p["platforms"], json!(["WebGL", "Windows", "macOS"]));
        f.save(
            "bug_batch.json",
            json!({"batch_id":"desktop-42","product":"desktop","bugs":[bug("a",true)]}),
        );
        let (p, _) = f.wait("capture_baseline");
        assert!(!f.work.join("bugs/a/fix_packet.json").exists());
        f.capture(&p, "before", false);
        let (p, _) = f.wait("apply_fix");
        f.fix(&p, "broken");
        let (p, _) = f.wait("apply_fix");
        assert_eq!(p["repair_feedback"][0]["error_signature"], "settings_reset");
        f.fix(&p, "correct");
        let (p, _) = f.wait("capture_verified_after");
        f.capture(&p, "after", false);
        let (p, _) = f.wait("capture_verified_after");
        assert_eq!(
            p["repair_feedback"][0]["error_signature"],
            "after_matches_before"
        );
        f.capture(&p, "after", true);
        let (p, _) = f.wait("author_release");
        f.author(&p, "Restored settings");
        let (gate, _) = f.wait("user_verify_gate");
        f.assert_unreleased();
        assert_eq!(gate["approval_example"]["approved_by"], "Alex");
        assert!(gate["instructions"][0].as_str().unwrap().contains("Alex"));
        assert_eq!(f.doc("review/manual_qa.json")["tester"], "Sam");
        let review = f.doc("review/review_summary.json");
        assert_eq!(review["bugs"][0]["fix"]["attempts"], 2);
        for stage in ["before", "after"] {
            assert_eq!(
                review["bugs"][0]["screenshots"][stage]["sha256"],
                sha256_file(&f.work.join(format!("media/a-{stage}.png"))).unwrap()
            )
        }
        f.save(
            "review/approval.json",
            json!({"approved":true,"approved_by":"Alex","approve_commit_main":"yes"}),
        );
        assert!(f.wait("user_verify_gate").1.contains("invalid_approval"));
        f.assert_unreleased();
        f.save("review/approval.json", gate["approval_example"].clone());
        let (upload, _) = f.wait("upload_version_history");
        assert_eq!(upload["zip_sha256"], review["package"]["zip_sha256"]);
        let mut receipt =
            json!({"previewed":true,"published":true,"release_id":"wrong","uploader_name":"Daws"});
        f.save("review/upload_receipt.json", receipt.clone());
        assert!(f
            .wait("upload_version_history")
            .1
            .contains("upload_incomplete"));
        receipt["release_id"] = json!("desktop-42");
        receipt["uploader_name"] = json!(" USER ");
        f.save("review/upload_receipt.json", receipt.clone());
        assert!(f
            .wait("upload_version_history")
            .1
            .contains("invalid_uploader_attribution"));
        receipt["uploader_name"] = json!("Daws");
        f.save("review/upload_receipt.json", receipt);
        let (commit, _) = f.wait("commit_to_main");
        assert_eq!(commit["touched_repos"], json!({"client":f.repo}));
        for (repo, branch) in [("client", "feature"), ("other", "main")] {
            f.save(
                "review/commit_receipt.json",
                json!({"committed":true,"commits":[{"repo":repo,"branch":branch,"sha":"123abc"}]}),
            );
            assert!(f.wait("commit_to_main").1.contains("commit_incomplete"))
        }
        f.save(
            "review/commit_receipt.json",
            json!({"committed":true,"commits":[{"repo":"client","branch":"main","sha":"123abc"}]}),
        );
        f.run().unwrap();
        f.run().unwrap();
    }

    #[test]
    fn selective_review_retry_unions_manual_failures_preserves_other_fix_and_invalidates_old_approval(
    ) {
        let f = Fixture::new(Some(["a", "b", "c"].map(|id| bug(id, false)).to_vec()));
        for id in ["a", "b", "c"] {
            let (p, _) = f.wait("apply_fix");
            assert_eq!(p["bug_id"], id);
            f.fix(&p, "correct");
        }
        for stage in ["before", "after"] {
            // The after capture runs immediately before package authoring.
            if stage == "after" {
                let (p, _) = f.wait("author_release");
                f.author(&p, "First pass");
            }
            assert_eq!(
                f.doc(&format!("evidence/{stage}_provenance.json")),
                json!({"revision":0,"captures":{}})
            );
            assert!(!f
                .work
                .join(format!("evidence/{stage}_packet.json"))
                .exists());
        }
        let (gate, _) = f.wait("user_verify_gate");
        let stable = fs::read(f.work.join("bugs/c/fix_report.json")).unwrap();
        f.assert_unreleased();
        let mut rejection = json!({"approved":false,"rejected_by":"Alex","review_sha256":"old","rejected_bugs":["a"],"manual_qa":{"b":{"result":"fail","gap_type":"missing_verifier","notes":"Reset"}}});
        f.save("review/approval.json", rejection.clone());
        assert!(f.wait("user_verify_gate").1.contains("stale_approval"));
        assert!(!f.work.join("review/review_feedback.json").exists());
        rejection["review_sha256"] = gate["review_sha256"].clone();
        f.save("review/approval.json", rejection);
        for id in ["a", "b"] {
            let (p, _) = f.wait("apply_fix");
            assert_eq!(p["bug_id"], id);
            assert_eq!(p["review_feedback"]["fix_revision"], 1);
            f.fix(&p, "correct");
        }
        let feedback = f.doc("review/review_feedback.json");
        assert_eq!(feedback["rejected_bugs"], json!(["a", "b"]));
        assert_eq!(feedback["fixes"]["b"]["manual_qa"]["notes"], "Reset");
        assert!(feedback["fixes"].get("c").is_none());
        assert_eq!(
            fs::read(f.work.join("bugs/c/fix_report.json")).unwrap(),
            stable
        );
        let (p, _) = f.wait("author_release");
        f.author(&p, "Repaired selected fixes");
        let (fresh, reason) = f.wait("user_verify_gate");
        assert!(reason.contains("stale_approval"));
        assert_ne!(fresh["review_sha256"], gate["review_sha256"]);
        assert_eq!(
            fresh["stale_approval_sha256"],
            feedback["rejected_approval_sha256"]
        );
        f.assert_unreleased();
        f.save("review/approval.json",json!({"approved":true,"approved_by":"Alex","approve_upload_zip":false,"approve_commit_main":false}));
        assert!(f.wait("user_verify_gate").1.contains("stale_approval"));
        f.assert_unreleased();
        f.save("review/approval.json",json!({"approved":true,"approved_by":"Alex","review_sha256":fresh["review_sha256"],"approve_upload_zip":false,"approve_commit_main":false,
            "manual_qa":[{"case_id":"b","result":"pass"}]}));
        f.run().unwrap();
        assert_eq!(
            f.doc("review/upload_receipt.json"),
            json!({"skipped":true,"reason":"Not approved for upload"})
        );
        assert_eq!(
            f.doc("review/commit_receipt.json"),
            json!({"skipped":true,"reason":"Not approved for commit to main"})
        );
        assert_eq!(f.doc("review/qa_scorecard.json")["counts"]["passed"], 1);
    }

    #[test]
    fn changed_watched_source_requires_a_fresh_approval_even_without_a_prior_rejection() {
        let mut f = Fixture::new(Some(vec![bug("a", false)]));
        f.cfg.watch_globs.push("theme".into());
        let (p, _) = f.wait("apply_fix");
        f.fix(&p, "correct");
        let (p, _) = f.wait("author_release");
        f.author(&p, "Restored settings");
        let (original, _) = f.wait("user_verify_gate");
        let mut approval = original["approval_example"].clone();
        approval["approve_upload_zip"] = json!(false);
        approval["approve_commit_main"] = json!(false);
        f.save("review/approval.json", approval.clone());
        fs::write(f.repo.join("theme"), "updated").unwrap();
        let (fresh, reason) = f.wait("user_verify_gate");
        assert!(reason.contains("stale_approval"));
        assert_ne!(fresh["review_sha256"], original["review_sha256"]);
        assert!(f.wait("user_verify_gate").1.contains("stale_approval"));
        f.assert_unreleased();
        approval["review_sha256"] = fresh["review_sha256"].clone();
        f.save("review/approval.json", approval);
        f.run().unwrap();
    }

    #[test]
    fn baseline_delivery_requires_matching_platform_path_and_decodable_png() {
        let f = Fixture::new(Some(vec![bug("a", true)]));
        let (mut packet, _) = f.wait("capture_baseline");
        for (field, bad) in [("platform", "Android"), ("path", "../wrong.png")] {
            f.capture(&packet, "before", false);
            let mut provenance = f.doc("evidence/before_provenance.json");
            provenance["captures"]["a"][field] = json!(bad);
            f.save("evidence/before_provenance.json", provenance);
            // Resolve each failure before testing the next one on a fresh graph.
            let (retry, _) = f.wait("capture_baseline");
            assert_eq!(
                retry["repair_feedback"].as_array().unwrap().last().unwrap()["error_signature"],
                "invalid_provenance"
            );
            packet = retry;
            if field == "platform" {
                // Start a separate baseline revision to avoid the repeated-signature breaker.
                f.capture(&packet, "before", false);
                f.wait("apply_fix");
            }
        }
        f.capture(&packet, "before", false);
        fs::write(f.work.join("media/a-before.png"), b"invalid").unwrap();
        let (retry, _) = f.wait("capture_baseline");
        assert_eq!(
            retry["repair_feedback"].as_array().unwrap().last().unwrap()["error_signature"],
            "invalid_before_png"
        );
        f.capture(&retry, "before", false);
        f.wait("apply_fix");
    }

    #[test]
    fn intake_validates_platform_repo_steps_commands_and_unique_ids() {
        let mut f = Fixture::new(None);
        f.cfg.product = "app".into();
        f.cfg.repos.insert("engine".into(), f.repo.clone());
        let (p, _) = f.wait("fetch_bug_reports");
        assert_eq!(p["platforms"], json!(["iOS", "Android"]));
        let mut good = Value::Object(bug("a", true));
        good["platform"] = json!("iOS");
        good["repo"] = json!("client");
        for (field, bad) in [
            ("repo", Value::Null),
            ("platform", json!("macOS")),
            ("repro_steps", json!([""])),
            ("expected", json!(" ")),
            ("requires_ui_capture", json!(0)),
            ("repro_cmd", json!([1])),
        ] {
            let mut wrong = good.clone();
            wrong[field] = bad;
            f.save(
                "bug_batch.json",
                json!({"batch_id":"desktop-42","product":"app","bugs":[wrong]}),
            );
            assert!(
                f.run().unwrap_err().reason.contains("invalid_bug_batch"),
                "{field}"
            )
        }
        let mut duplicate = good.clone();
        duplicate["id"] = json!("A");
        f.save(
            "bug_batch.json",
            json!({"batch_id":"desktop-42","product":"app","bugs":[good,duplicate]}),
        );
        assert!(f.run().unwrap_err().reason.contains("Duplicate bug ID"));
    }
}
