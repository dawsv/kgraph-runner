//! Resumable desktop bug fixes with command verification and persistent repair feedback.

use crate::common::{
    files_hash, fingerprint, packet_worker, record_verification, require_bug_id, verification,
    with_retry_route, CheckFn, PacketWorkerOptions, Repairs, StringFn,
};
use kgraph_core::files::{read_object, require_id, write_json, write_text};
use kgraph_core::hash::hash_globs;
use kgraph_core::qa_metrics::{init_qa_checklist, write_qa_scorecard};
use kgraph_core::shell::CommandSpec;
use kgraph_core::types::*;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

const STAGES: [&str; 5] = [
    "triage_and_plan",
    "apply_fix",
    "build_check",
    "run_tests",
    "verify_fix",
];
const COMMANDS: [&str; 3] = ["build", "repro", "test"];

#[derive(Clone)]
pub struct DesktopBugFixConfig {
    pub bug_spec: Map<String, Value>,
    pub work_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub repro_cmd: Option<CommandSpec>,
    pub build_cmd: Option<CommandSpec>,
    pub test_cmd: Option<CommandSpec>,
    pub watch_globs: Vec<String>,
    pub verify_fn: Option<CheckFn>,
    pub resource: Option<String>,
    pub timeout_s: Option<f64>,
    pub max_retries: usize,
    pub deps: Vec<String>,
    pub node_id: Option<String>,
    pub review_feedback: Option<Map<String, Value>>,
    pub tester: Option<String>,
}

impl Default for DesktopBugFixConfig {
    fn default() -> Self {
        Self {
            bug_spec: Map::new(),
            work_dir: PathBuf::new(),
            repo_dir: ".".into(),
            repro_cmd: None,
            build_cmd: None,
            test_cmd: None,
            watch_globs: vec![],
            verify_fn: None,
            resource: Some("repo".into()),
            timeout_s: Some(300.0),
            max_retries: 4,
            deps: vec![],
            node_id: None,
            review_feedback: None,
            tester: None,
        }
    }
}

#[derive(Clone)]
pub struct BugBatchConfig {
    pub bugs: Vec<Map<String, Value>>,
    pub work_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub repro_cmd: Option<CommandSpec>,
    pub build_cmd: Option<CommandSpec>,
    pub test_cmd: Option<CommandSpec>,
    pub watch_globs: Vec<String>,
    pub verify_fn: Option<CheckFn>,
    pub resource: Option<String>,
    pub timeout_s: Option<f64>,
    pub max_retries: usize,
    pub batch_id: Option<String>,
    pub tester: Option<String>,
}

impl Default for BugBatchConfig {
    fn default() -> Self {
        let defaults = DesktopBugFixConfig::default();
        Self {
            bugs: vec![],
            work_dir: defaults.work_dir,
            repo_dir: defaults.repo_dir,
            repro_cmd: None,
            build_cmd: None,
            test_cmd: None,
            watch_globs: vec![],
            verify_fn: None,
            resource: defaults.resource,
            timeout_s: defaults.timeout_s,
            max_retries: defaults.max_retries,
            batch_id: None,
            tester: None,
        }
    }
}

// Like pathlib.resolve(strict=False), resolve existing symlinks even when a later component is missing.
pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    fn append(base: &mut PathBuf, path: &Path, depth: usize) -> io::Result<()> {
        if depth > 40 {
            return Err(io::Error::other("Too many symbolic links"));
        }
        for part in path.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    base.pop();
                }
                Component::Normal(name) => {
                    base.push(name);
                    match fs::symlink_metadata(&*base) {
                        Ok(meta) if meta.is_symlink() => {
                            let target = fs::read_link(&*base)?;
                            base.pop();
                            append(base, &target, depth + 1)?;
                        }
                        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                        _ => {}
                    }
                }
                _ => base.push(part.as_os_str()),
            }
        }
        Ok(())
    }
    let mut base = std::env::current_dir().map_err(|e| e.to_string())?;
    append(&mut base, path, 0).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(base)
}

fn whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

pub(crate) fn configured_tester(explicit: Option<&str>) -> Result<Option<String>, String> {
    match explicit.map(|s| s.trim_matches(whitespace)) {
        Some("") => Err("tester must be non-empty text".into()),
        Some(name) => Ok(Some(name.into())),
        None => Ok(std::env::var("KGRAPH_TESTER")
            .ok()
            .map(|s| s.trim_matches(whitespace).to_owned())
            .filter(|s| !s.is_empty())),
    }
}

pub(crate) fn qa_hash(mut values: Vec<Value>, tester: Option<&str>) -> String {
    if let Some(tester) = tester {
        values.push(json!(tester));
    }
    fingerprint(Value::Array(values))
}

pub(crate) fn command_json(cmd: Option<&CommandSpec>, name: &str) -> Result<Value, String> {
    match cmd {
        None => Ok(Value::Null),
        Some(CommandSpec::Shell(s)) if !s.trim_matches(whitespace).is_empty() => Ok(json!(s)),
        Some(CommandSpec::Args(args))
            if args
                .first()
                .is_some_and(|s| !s.trim_matches(whitespace).is_empty()) =>
        {
            Ok(json!(args))
        }
        _ => Err(format!(
            "{name} must be a non-empty command string or argument sequence"
        )),
    }
}

pub(crate) fn outputs(paths: Vec<PathBuf>) -> OutputsFn {
    Arc::new(move |_| Ok(paths.clone()))
}

pub(crate) fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

static FAILURE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:fail(?:ed|ure)?|traceback|\w*(?:error|exception))\b").unwrap()
});
static FRAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"^File ".*", line \d+(?:, in .*)?$"#).unwrap());
static VOLATILE: Lazy<Vec<(Regex, &str)>> = Lazy::new(|| {
    [
        (r"\x1b\[[0-?]*[ -/]*[@-~]", ""),
        (
            r"\b(?:\d{4}-\d{2}-\d{2}[ T])?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b",
            "<time>",
        ),
        (r"\b\d+(?:\.\d+)?\s*(?:ms|s)\b", "<duration>"),
        (r"\b0x[0-9a-fA-F]+\b", "<address>"),
        (r"\bline \d+\b", "<line>"),
    ]
    .into_iter()
    .map(|(p, r)| (Regex::new(p).unwrap(), r))
    .collect()
});
static LOCATION: Lazy<Regex> = Lazy::new(|| Regex::new(r":\d+(?::\d+)?").unwrap());

fn normalize(line: &str, repo: &Path) -> String {
    let mut line = line.replace(repo.to_string_lossy().as_ref(), "<repo>");
    for (pattern, replacement) in VOLATILE.iter() {
        line = pattern.replace_all(&line, *replacement).into_owned();
    }
    // Implement Python's lookahead without consuming the delimiter of the next match.
    let mut normalized = String::new();
    let mut copied = 0;
    for location in LOCATION.find_iter(&line) {
        let mut end = location.end();
        let valid_end = |i| {
            line[i..]
                .chars()
                .next()
                .is_none_or(|c| c == ':' || c == ')' || whitespace(c))
        };
        if !valid_end(end) {
            if let Some(second) = location.as_str()[1..].find(':') {
                end = location.start() + 1 + second;
            } else {
                continue;
            }
        }
        normalized.push_str(&line[copied..location.start()]);
        normalized.push_str("<line>");
        copied = end;
    }
    normalized.push_str(&line[copied..]);
    normalized
        .split(whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn failure_output(log: &Path, repo: &Path) -> io::Result<(String, String)> {
    let mut tail = VecDeque::with_capacity(20);
    let mut selected = VecDeque::with_capacity(12);
    let mut previous_frame = false;
    for bytes in BufReader::new(File::open(log)?).split(b'\n') {
        let bytes = bytes?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text
            .split('\r')
            .map(|s| s.trim_matches(whitespace))
            .filter(|s| !s.is_empty())
        {
            if tail.len() == 20 {
                tail.pop_front();
            }
            tail.push_back(line.to_owned());
            let frame = FRAME.is_match(line);
            if frame || previous_frame || FAILURE.is_match(line) {
                if selected.len() == 12 {
                    selected.pop_front();
                }
                selected.push_back(normalize(line, repo));
            }
            previous_frame = frame;
        }
    }
    if selected.is_empty() {
        selected.extend(
            tail.iter()
                .skip(tail.len().saturating_sub(5))
                .map(|s| normalize(s, repo)),
        );
    }
    let hash = fingerprint(json!([selected]));
    let text = tail.into_iter().collect::<Vec<_>>().join("\n");
    let start = text.char_indices().rev().nth(1199).map_or(0, |(i, _)| i);
    Ok((text[start..].to_owned(), hash[..8].to_owned()))
}

// Keep the child's stdout and stderr on the same file descriptor target. The
// stage failure fingerprint depends on the byte order of the merged build log.
struct BuildProcess {
    child: Child,
    armed: bool,
}

impl BuildProcess {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // setpgid(0, 0) in the child before exec. Descendants inherit this group.
            command.process_group(0);
        }
        Ok(Self {
            child: command.spawn()?,
            armed: true,
        })
    }

    fn terminate(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            // SAFETY: this unreaped child's PID identifies its own process group.
            if unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        #[cfg(windows)]
        {
            use std::process::Stdio;
            let result = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &self.child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if !result.as_ref().is_ok_and(|s| s.success()) && self.child.try_wait()?.is_none() {
                self.child.kill()?;
                self.child.wait()?;
                self.armed = false;
                return Err(result.err().unwrap_or_else(|| {
                    io::Error::other("taskkill could not terminate build process tree")
                }));
            }
        }
        #[cfg(not(any(unix, windows)))]
        self.child.kill()?;
        self.child.wait()?;
        self.armed = false;
        Ok(())
    }

    fn wait(mut self, timeout: Option<f64>) -> io::Result<Option<ExitStatus>> {
        let Some(seconds) = timeout else {
            let status = self.child.wait()?;
            self.armed = false;
            return Ok(Some(status));
        };
        let start = Instant::now();
        let limit = Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX);
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.armed = false;
                return Ok(Some(status));
            }
            let remaining = limit.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                self.terminate()?;
                return Ok(None);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
}

impl Drop for BuildProcess {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.terminate();
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

fn run_command(
    stage: &str,
    spec: &CommandSpec,
    log: &Path,
    repo: &Path,
    timeout: Option<f64>,
) -> Result<VerifyOutcome, String> {
    let mut command = match spec {
        CommandSpec::Args(args) => {
            let mut cmd = Command::new(&args[0]);
            cmd.args(&args[1..]);
            cmd
        }
        CommandSpec::Shell(script) => {
            #[cfg(windows)]
            let (shell, flag) = ("cmd", "/C");
            #[cfg(not(windows))]
            let (shell, flag) = ("/bin/sh", "-c");
            let mut cmd = Command::new(shell);
            cmd.args([flag, script]);
            cmd
        }
    };
    let mut file = File::create(log).map_err(|e| e.to_string())?;
    command.stdout(file.try_clone().map_err(|e| e.to_string())?);
    command.stderr(file.try_clone().map_err(|e| e.to_string())?);
    let (sig, message, with_hash) = match BuildProcess::spawn(command.current_dir(repo))
        .and_then(|process| process.wait(timeout))
    {
        Ok(Some(status)) if status.success() => {
            return Ok(VerifyOutcome::pass(format!("{stage} command passed")))
        }
        Ok(Some(status)) => {
            let code = exit_code(status);
            (
                format!("{stage}_exit_{code}"),
                format!("{stage} command exited with code {code}"),
                true,
            )
        }
        Ok(None) => (
            format!("{stage}_timeout"),
            format!("{stage} timed out after {:?}s", timeout.unwrap()),
            false,
        ),
        Err(e) => {
            let subject = if !repo.is_dir() {
                repo.as_os_str()
            } else {
                command.get_program()
            };
            let detail = match e.raw_os_error() {
                Some(errno) => format!(
                    "[Errno {errno}] {}: '{}'",
                    e.to_string().split(" (os error ").next().unwrap_or(""),
                    subject.to_string_lossy()
                ),
                None => e.to_string(),
            };
            writeln!(file, "{detail}").map_err(|e| e.to_string())?;
            (
                format!("{stage}_launch_error"),
                format!("Could not start {stage} command"),
                false,
            )
        }
    };
    let (tail, hash) = failure_output(log, repo).map_err(|e| e.to_string())?;
    let signature = if with_hash {
        format!("{sig}:{hash}")
    } else {
        sig
    };
    let detail = if tail.is_empty() {
        message
    } else {
        format!("{message}\n{tail}")
    };
    Ok(VerifyOutcome::fail(signature, detail))
}

pub fn bug_qa_case(spec: &Map<String, Value>, fix: &Map<String, Value>) -> Value {
    json!({"id": fix["bug_id"], "title": fix["title"], "platform": spec.get("platform").and_then(Value::as_str),
           "kind": "bug", "automated_status": "pass", "ai_attempts": fix["attempts"]})
}

struct Fix {
    cfg: DesktopBugFixConfig,
    id: String,
    dir: PathBuf,
    settings: Map<String, Value>,
    config_hash: String,
    fix_hash: StringFn,
    repairs: Repairs,
}

impl Fix {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn logs(&self, existing_only: bool) -> Map<String, Value> {
        COMMANDS
            .iter()
            .filter_map(|stage| {
                let path = self.path(&format!("{stage}.log"));
                (!existing_only || path.is_file()).then(|| (stage.to_string(), json!(path)))
            })
            .collect()
    }

    fn packet(&self) -> Value {
        let mut packet = self.settings.clone();
        let mut instructions = vec!["Investigate the bug and apply the fix in repo_dir."];
        if let Some(review) = &self.cfg.review_feedback {
            if !review.is_empty() {
                packet.insert("review_feedback".into(), json!(review));
                instructions.push("Address the reviewer notes in review_feedback before delivering the revised fix.");
            }
        }
        instructions.extend(["Use the commands, logs, and repair_feedback to address verification failures.",
            "Deliver fix_report.json with the requested integer revision or newer, a root_cause, a summary, and a list of changed_files."]);
        packet.extend(json!({"stage":"apply_fix", "bug_id":self.id, "bug_spec":self.cfg.bug_spec, "spec_path":self.path("bug_spec.json"),
            "log_paths":self.logs(true), "instructions":instructions}).as_object().unwrap().clone());
        Value::Object(packet)
    }

    fn verification_hash(&self, names: &[&str]) -> Result<String, String> {
        let paths = names.iter().map(|n| self.path(n)).collect::<Vec<_>>();
        Ok(fingerprint(json!([
            self.config_hash,
            (self.fix_hash)()?,
            files_hash(&paths)
        ])))
    }

    fn command(&self, stage: &str, spec: &CommandSpec) -> Result<VerifyOutcome, String> {
        run_command(
            stage,
            spec,
            &self.path(&format!("{stage}.log")),
            &self.cfg.repo_dir,
            self.cfg.timeout_s,
        )
    }

    fn check_build(&self) -> Result<VerifyOutcome, String> {
        match &self.cfg.build_cmd {
            Some(cmd) => self.command("build", cmd),
            None => {
                write_text(&self.path("build.log"), "No build command configured.\n")?;
                Ok(VerifyOutcome::pass("No build command configured"))
            }
        }
    }

    fn test_commands(&self) -> [(&str, Option<&CommandSpec>); 2] {
        [
            ("repro", self.cfg.repro_cmd.as_ref()),
            ("test", self.cfg.test_cmd.as_ref()),
        ]
    }

    fn check_tests(&self, ctx: &Context) -> Result<VerifyOutcome, String> {
        let build = verification(&self.path("build_result.json"))?;
        for (stage, command) in self.test_commands() {
            if !build.ok || command.is_none() {
                remove_file(&self.path(&format!("{stage}.log")))?;
            }
        }
        if !build.ok {
            return Ok(build);
        }
        let mut results = Vec::new();
        for (stage, command) in self.test_commands() {
            if let Some(cmd) = command {
                results.push(self.command(stage, cmd)?);
            }
        }
        if let Some(verify) = &self.cfg.verify_fn {
            results.push(verify(ctx)?);
        }
        let passed = if results.is_empty() {
            "No tests configured"
        } else {
            "Reproduction and regression checks passed"
        };
        Ok(results
            .into_iter()
            .find(|v| !v.ok)
            .unwrap_or_else(|| VerifyOutcome::pass(passed)))
    }

    fn check_fix(&self) -> Result<VerifyOutcome, String> {
        let final_summary = self.path("final_summary.json");
        for name in ["build_result.json", "test_result.json"] {
            let result = verification(&self.path(name))?;
            if !result.ok {
                remove_file(&final_summary)?;
                return Ok(result);
            }
        }
        let report = read_object(&self.path("fix_report.json")).unwrap_or_default();
        let instructions = self.repairs.instructions()?;
        let nonempty = |v: &Value| {
            v.as_str()
                .is_some_and(|s| !s.trim_matches(whitespace).is_empty())
        };
        let valid = report
            .get("revision")
            .and_then(Value::as_u64)
            .is_some_and(|r| r >= instructions["revision"].as_u64().unwrap())
            && ["root_cause", "summary"]
                .iter()
                .all(|k| report.get(*k).is_some_and(nonempty))
            && report
                .get("changed_files")
                .and_then(Value::as_array)
                .is_some_and(|paths| paths.iter().all(nonempty));
        if !valid {
            remove_file(&final_summary)?;
            return Ok(VerifyOutcome::fail("invalid_fix_report", "Provide the required revision, a non-empty summary and root_cause, and a changed_files list of paths in fix_report.json."));
        }
        let review_attempts = self
            .cfg
            .review_feedback
            .as_ref()
            .filter(|r| !r.is_empty())
            .map_or(0, |r| {
                r.get("review_attempts")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
            });
        let attempts = (instructions["repair_feedback"].as_array().unwrap().len() as u64 + 1)
            .checked_add(review_attempts)
            .ok_or("Too many repair attempts")?;
        let title = self
            .cfg
            .bug_spec
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.id);
        let fix = json!({"bug_id":self.id, "title":title, "root_cause": report["root_cause"], "summary":report["summary"],
            "changed_files":report["changed_files"], "revision":report["revision"], "attempts":attempts, "log_paths":self.logs(true)});
        init_qa_checklist(
            &self.path("manual_qa.json"),
            &self.id,
            &[bug_qa_case(&self.cfg.bug_spec, fix.as_object().unwrap())],
            "desktop-bug-fix",
            Some(title),
            self.cfg.tester.as_deref(),
            None,
            None,
            true,
        )?;
        write_json(&final_summary, &fix, true)?;
        Ok(VerifyOutcome::pass(format!("Verified fix for {}", self.id)))
    }

    fn triage(self: &Arc<Self>) -> TaskNode {
        let run = self.clone();
        let hash = self.clone();
        let mut node = TaskNode::new(
            "triage_and_plan",
            TaskKind::Code,
            Arc::new(move |_, _| {
                let mut spec = run.cfg.bug_spec.clone();
                spec.extend(run.settings.clone());
                spec.insert("log_paths".into(), json!(run.logs(false)));
                spec.extend(run.repairs.instructions().map_err(TaskRunError::other)?);
                write_json(&run.path("bug_spec.json"), &spec, true).map_err(TaskRunError::other)?;
                Ok(Value::Null)
            }),
            Arc::new(move |_| {
                Ok(fingerprint(json!([
                    hash.config_hash,
                    hash.repairs.instructions()?
                ])))
            }),
        );
        node.outputs_fn = outputs(vec![self.path("bug_spec.json")]);
        node
    }

    fn build(self: &Arc<Self>) -> TaskNode {
        let hash = self.clone();
        let check = self.clone();
        let mut task = record_verification(
            "build_check",
            self.path("build_result.json"),
            Arc::new(move || hash.verification_hash(&[])),
            Arc::new(move |_| check.check_build()),
            vec!["apply_fix".into()],
            self.cfg.resource.clone(),
            true,
        );
        task.outputs_fn = outputs(vec![self.path("build_result.json"), self.path("build.log")]);
        task
    }

    fn tests(self: &Arc<Self>) -> TaskNode {
        let fix = self.clone();
        let hash: StringFn = Arc::new(move || fix.verification_hash(&["build_result.json"]));
        let check = self.clone();
        let mut task = record_verification(
            "run_tests",
            self.path("test_result.json"),
            hash.clone(),
            Arc::new(move |ctx| check.check_tests(ctx)),
            vec!["build_check".into()],
            self.cfg.resource.clone(),
            true,
        );
        if self.cfg.verify_fn.is_some() {
            task.input_hash_fn = Arc::new(move |ctx| {
                let mut inputs = ctx.snapshot_map();
                inputs.remove("repair_feedback");
                inputs.remove("verification_results");
                Ok(fingerprint(json!([hash()?, inputs])))
            });
        }
        let fix = self.clone();
        task.outputs_fn = Arc::new(move |_| {
            let mut paths = vec![fix.path("test_result.json")];
            if verification(&fix.path("build_result.json")).is_ok_and(|r| r.ok) {
                paths.extend(
                    fix.test_commands()
                        .into_iter()
                        .filter(|(_, cmd)| cmd.is_some())
                        .map(|(stage, _)| fix.path(&format!("{stage}.log"))),
                );
            }
            Ok(paths)
        });
        task
    }

    fn verify(self: &Arc<Self>) -> TaskNode {
        let hash = self.clone();
        let check = self.clone();
        let mut task = record_verification(
            "verify_fix",
            self.path("verification.json"),
            Arc::new(move || {
                Ok(qa_hash(
                    vec![
                        json!(hash.verification_hash(&["build_result.json", "test_result.json"])?),
                        json!(hash.repairs.instructions()?),
                        json!("qa-v1"),
                        json!(hash.cfg.review_feedback),
                    ],
                    hash.cfg.tester.as_deref(),
                ))
            }),
            Arc::new(move |_| check.check_fix()),
            vec!["run_tests".into()],
            None,
            true,
        );
        let fix = self.clone();
        task.outputs_fn = Arc::new(move |_| {
            let result = fix.path("verification.json");
            let mut paths = vec![result.clone()];
            if verification(&result).is_ok_and(|r| r.ok) {
                paths.push(fix.path("final_summary.json"));
            }
            Ok(paths)
        });
        task
    }
}

// LoopRepairFn returns the engine's structured GraphFailure by value.
#[allow(clippy::result_large_err)]
pub fn create_desktop_bug_fix_subgraph(
    mut cfg: DesktopBugFixConfig,
) -> Result<LoopSubgraphNode, String> {
    let id = require_bug_id(cfg.bug_spec.get("id").and_then(Value::as_str).unwrap_or(""))?;
    let node_id = require_id(cfg.node_id.as_deref().unwrap_or(&id))?;
    cfg.tester = configured_tester(cfg.tester.as_deref())?;
    let commands = json!({"build":command_json(cfg.build_cmd.as_ref(), "build_cmd")?,
        "repro":command_json(cfg.repro_cmd.as_ref(),"repro_cmd")?, "test":command_json(cfg.test_cmd.as_ref(),"test_cmd")?});
    if cfg.verify_fn.is_none() && commands.as_object().unwrap().values().all(Value::is_null) {
        return Err(
            "Configure at least one of repro_cmd, build_cmd, test_cmd, or verify_fn".into(),
        );
    }
    cfg.repo_dir = resolve_path(&cfg.repo_dir)?;
    let dir = resolve_path(&cfg.work_dir)?.join("bugs").join(&id);
    let settings = json!({"repo_dir":cfg.repo_dir, "commands":commands, "timeout_s":cfg.timeout_s, "watch_globs":cfg.watch_globs});
    let config_hash = fingerprint(json!([
        cfg.bug_spec,
        settings,
        cfg.verify_fn.as_ref().map(|_| "verify")
    ]));
    let fix_report = dir.join("fix_report.json");
    let artifact = fix_report.clone();
    let globs = cfg.watch_globs.clone();
    let repo = cfg.repo_dir.clone();
    let fix_hash: StringFn = Arc::new(move || {
        Ok(fingerprint(json!([
            files_hash(std::slice::from_ref(&artifact)),
            hash_globs(&globs, Some(&repo), None, &[], true)?
        ])))
    });
    let hash = config_hash.clone();
    let constant_hash: StringFn = Arc::new(move || Ok(hash.clone()));
    let minimum = match cfg.review_feedback.as_ref().filter(|r| !r.is_empty()) {
        Some(review) => review
            .get("fix_revision")
            .and_then(Value::as_u64)
            .ok_or("review_feedback requires an integer fix_revision")?,
        None => 0,
    };
    let repairs = Repairs::new(
        dir.join("repair_state.json"),
        constant_hash.clone(),
        vec![fix_report.clone()],
        Some(fix_report.clone()),
        Some(fix_hash.clone()),
        minimum,
    );
    let fix = Arc::new(Fix {
        cfg,
        id,
        dir,
        settings: settings.as_object().unwrap().clone(),
        config_hash,
        fix_hash,
        repairs: repairs.clone(),
    });
    let packet = fix.clone();
    let worker = packet_worker(PacketWorkerOptions {
        node_id: "apply_fix".into(),
        packet_path: fix.path("fix_packet.json"),
        artifacts: outputs(vec![fix_report]),
        input_hash: constant_hash,
        packet: Arc::new(move |_| Ok(packet.packet())),
        deps: vec!["triage_and_plan".into()],
        repairs: Some(repairs.clone()),
        enabled: None,
        verify: None,
        resource: None,
    });
    let tasks = [fix.triage(), worker, fix.build(), fix.tests(), fix.verify()];
    let verify = fix.clone();
    let mut node = LoopSubgraphNode::new(
        node_id,
        tasks.into_iter().map(Node::Task).collect(),
        Arc::new(move |_| {
            let result = verification(&verify.path("verification.json"))?;
            if result.ok {
                verify.repairs.resolved()?;
            }
            Ok(result)
        }),
    );
    let limit = fix.cfg.max_retries;
    node.max_retries = limit;
    node.repair_fn = Some(Arc::new(move |ctx, _, sig, detail| {
        repairs.repair(ctx, sig, detail, &STAGES, limit)
    }));
    node.deps = fix.cfg.deps.clone();
    node.resource = fix.cfg.resource.clone();
    node.qa_checklist_path = Some(fix.path("manual_qa.json"));
    Ok(with_retry_route(
        node,
        "verify_fix",
        "apply_fix",
        "if fails → repair",
    ))
}

fn override_command(
    spec: &mut Map<String, Value>,
    key: &str,
    default: &Option<CommandSpec>,
) -> Result<Option<CommandSpec>, String> {
    match spec.remove(key) {
        None => Ok(default.clone()),
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(CommandSpec::Shell(s))),
        Some(v) => serde_json::from_value(v)
            .map(CommandSpec::Args)
            .map(Some)
            .map_err(|_| format!("{key} must be a non-empty command string or argument sequence")),
    }
}

fn batch_reports(
    dir: &Path,
    id: &str,
    tester: Option<&str>,
    specs: &[Map<String, Value>],
    summaries: &[PathBuf],
) -> Result<(), String> {
    let fixes: Vec<_> = summaries
        .iter()
        .map(|p| read_object(p))
        .collect::<Result<_, _>>()?;
    let cases: Vec<_> = specs
        .iter()
        .zip(&fixes)
        .map(|(s, f)| bug_qa_case(s, f))
        .collect();
    let qa = dir.join("manual_qa.json");
    init_qa_checklist(
        &qa,
        id,
        &cases,
        "bug-batch",
        Some("Bug fix batch"),
        tester,
        None,
        None,
        true,
    )?;
    write_qa_scorecard(&qa, None, None, false, None)?;
    write_json(
        &dir.join("batch_report.json"),
        &json!({"fixed_count":fixes.len(), "bugs":fixes}),
        true,
    )?;
    let noun = if fixes.len() == 1 { "fix" } else { "fixes" };
    let mut markdown = format!(
        "# Bug fix batch report\n\nVerified {} {noun}.\n",
        fixes.len()
    );
    for fix in fixes {
        let text = |k: &str| {
            fix.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Missing string {k} in final summary"))
        };
        markdown.push_str(&format!(
            "\n## {} — {}\n\n{}\n\n### Root cause\n\n{}\n\nVerified on attempt {} (revision {}).\n",
            text("bug_id")?,
            text("title")?,
            text("summary")?,
            text("root_cause")?,
            fix["attempts"],
            fix["revision"]
        ));
        if let Some(files) = fix
            .get("changed_files")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            markdown.push_str("\n### Changed files\n\n");
            for path in files {
                markdown.push_str(&format!(
                    "- {}\n",
                    path.as_str().ok_or("Invalid changed file")?
                ));
            }
        }
    }
    write_text(&dir.join("batch_report.md"), &markdown)
}

pub fn create_bug_batch_graph(cfg: BugBatchConfig) -> Result<Vec<Node>, String> {
    let dir = resolve_path(&cfg.work_dir)?;
    let id = cfg.batch_id.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("bug-batch")
            .into()
    });
    let tester = configured_tester(cfg.tester.as_deref())?;
    let checklist = dir.join("manual_qa.json");
    let mut used = HashSet::from(["consolidated_report".to_owned()]);
    let mut nodes: Vec<Node> = Vec::new();
    let mut specs = Vec::new();
    let mut summaries = Vec::new();
    for mut spec in cfg.bugs {
        let bug_id = require_bug_id(spec.get("id").and_then(Value::as_str).unwrap_or(""))?;
        if !used.insert(bug_id.to_ascii_lowercase()) {
            return Err(format!("Duplicate or reserved batch bug ID {bug_id:?}"));
        }
        let mut fix = create_desktop_bug_fix_subgraph(DesktopBugFixConfig {
            build_cmd: override_command(&mut spec, "build_cmd", &cfg.build_cmd)?,
            test_cmd: override_command(&mut spec, "test_cmd", &cfg.test_cmd)?,
            repro_cmd: override_command(&mut spec, "repro_cmd", &cfg.repro_cmd)?,
            watch_globs: match spec.remove("watch_globs") {
                Some(v) => serde_json::from_value(v).map_err(|e| e.to_string())?,
                None => cfg.watch_globs.clone(),
            },
            max_retries: match spec.remove("max_retries") {
                Some(v) => serde_json::from_value(v)
                    .map_err(|_| "max_retries must be a nonnegative integer")?,
                None => cfg.max_retries,
            },
            deps: nodes
                .last()
                .filter(|_| cfg.resource.is_some())
                .map(|p| vec![p.id().to_owned()])
                .unwrap_or_default(),
            bug_spec: spec.clone(),
            work_dir: dir.clone(),
            repo_dir: cfg.repo_dir.clone(),
            verify_fn: cfg.verify_fn.clone(),
            resource: cfg.resource.clone(),
            timeout_s: cfg.timeout_s,
            tester: tester.clone(),
            ..Default::default()
        })?;
        fix.qa_checklist_path = Some(checklist.clone());
        summaries.push(dir.join("bugs").join(bug_id).join("final_summary.json"));
        specs.push(spec);
        nodes.push(Node::LoopSubgraph(fix));
    }
    let hash_id = id.clone();
    let hash_specs = specs.clone();
    let hash_paths = summaries.clone();
    let hash_tester = tester.clone();
    let report_paths = vec![dir.join("batch_report.json"), dir.join("batch_report.md")];
    let mut report = TaskNode::new(
        "consolidated_report",
        TaskKind::Code,
        Arc::new(move |_, _| {
            batch_reports(&dir, &id, tester.as_deref(), &specs, &summaries)
                .map_err(TaskRunError::other)?;
            Ok(Value::Null)
        }),
        Arc::new(move |_| {
            Ok(qa_hash(
                vec![
                    json!("qa-v1"),
                    json!(hash_id),
                    json!(hash_specs),
                    json!(files_hash(&hash_paths)),
                ],
                hash_tester.as_deref(),
            ))
        }),
    );
    report.deps = nodes.iter().map(|n| n.id().into()).collect();
    report.outputs_fn = outputs(report_paths);
    report.qa_checklist_path = Some(checklist);
    nodes.push(Node::Task(report));
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraph_core::engine::{GraphRunner, RunnerConfig};
    use kgraph_core::qa_metrics::{record_qa_result, QaResultInput};
    use tempfile::TempDir;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }
    fn shell(s: &str) -> Option<CommandSpec> {
        Some(CommandSpec::Shell(s.into()))
    }

    struct Fixture {
        _temp: TempDir,
        cfg: DesktopBugFixConfig,
        state: PathBuf,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let repo = root.join("app repo");
            fs::create_dir(&repo).unwrap();
            fs::write(repo.join("value"), "wrong").unwrap();
            let work = root.join("fixes");
            let cfg = DesktopBugFixConfig {
                bug_spec:object(json!({"id":"app-42", "title":"Restore the \"settings\"", "platform":"macOS", "description":"First line\nSecond line"})),
                work_dir:work.clone(), repo_dir:repo, watch_globs:vec!["value".into()],
                test_cmd:shell("echo checking; if [ \"$(cat value)\" != good ]; then echo \"Error $(cat value)\" >&2; exit 1; fi; echo accepted"),
                ..Default::default()
            };
            Self {
                _temp: temp,
                cfg,
                state: root.join("state.json"),
                dir: work.join("bugs/app-42"),
            }
        }
        fn runner(&self, budget: Option<usize>) -> GraphRunner {
            let mut cfg = RunnerConfig::new(&self.state);
            cfg.max_worker_calls = budget;
            cfg.logger = Some(Arc::new(|_| {}));
            GraphRunner::new(cfg).unwrap()
        }
        fn graph(&self) -> Vec<Node> {
            vec![Node::LoopSubgraph(
                create_desktop_bug_fix_subgraph(self.cfg.clone()).unwrap(),
            )]
        }
        fn run(&self, expected: &str, ctx: &Context, budget: Option<usize>) -> Vec<String> {
            let runner = self.runner(budget);
            let result = runner.run(&self.graph(), ctx, "");
            assert_eq!(
                result
                    .as_ref()
                    .err()
                    .map_or("passed", GraphFailure::status_str),
                expected,
                "{result:?}"
            );
            runner
                .trace()
                .into_iter()
                .filter(|e| e["status"] == "cached")
                .map(|e| e["node"].as_str().unwrap().to_owned())
                .collect()
        }
        fn read(&self, name: &str) -> Map<String, Value> {
            read_object(&self.dir.join(name)).unwrap()
        }
        fn deliver(&self, rev: u64) {
            write_json(
                &self.dir.join("fix_report.json"),
                &json!({"revision":rev, "summary":"Restored settings.",
                "root_cause":"Bad defaults.", "changed_files":["value"]}),
                true,
            )
            .unwrap();
        }
        fn value(&self, s: &str) {
            fs::write(self.cfg.repo_dir.join("value"), s).unwrap();
        }
    }

    #[test]
    fn validates_bug_ids_commands_batch_overrides_and_topology() {
        let mut f = Fixture::new();
        for bad in [
            json!(null),
            json!(1),
            json!(""),
            json!("path/to/bug"),
            json!("nul"),
        ] {
            let mut cfg = f.cfg.clone();
            cfg.bug_spec.insert("id".into(), bad);
            assert!(create_desktop_bug_fix_subgraph(cfg).is_err());
        }
        for cmd in [
            None,
            shell(" "),
            Some(CommandSpec::Args(vec![])),
            Some(CommandSpec::Args(vec!["".into()])),
        ] {
            let mut cfg = f.cfg.clone();
            cfg.test_cmd = cmd;
            assert!(create_desktop_bug_fix_subgraph(cfg).is_err());
        }
        for ids in [vec!["A", "a"], vec!["CONSOLIDATED_REPORT"]] {
            let cfg = BugBatchConfig {
                bugs: ids.iter().map(|id| object(json!({"id":id}))).collect(),
                work_dir: f.cfg.work_dir.clone(),
                test_cmd: shell("echo test"),
                ..Default::default()
            };
            assert!(create_bug_batch_graph(cfg)
                .err()
                .unwrap()
                .contains("Duplicate or reserved"));
        }
        f.cfg.node_id = Some("fix_settings".into());
        f.cfg.deps = vec!["ready".into()];
        f.cfg.resource = Some("desktop".into());
        let node = create_desktop_bug_fix_subgraph(f.cfg).unwrap();
        assert_eq!(node.id, "fix_settings");
        assert_eq!(node.deps, ["ready"]);
        assert_eq!(node.resource.as_deref(), Some("desktop"));
        assert_eq!(node.nodes.iter().map(Node::id).collect::<Vec<_>>(), STAGES);
        assert_eq!(node.retry_route.unwrap().label, "if fails → repair (max 4)");
        assert_eq!(
            node.qa_checklist_path.unwrap(),
            f.dir.join("manual_qa.json")
        );
    }

    #[test]
    fn normalization_matches_python_hash_and_character_tail() {
        let f = Fixture::new();
        let log = f.cfg.repo_dir.join("test.log");
        let trace = "noisy info\nTraceback (most recent call last):\n File \"/app/code\", line 47, in trial\n   assert a == b\n\x1b[31mERROR\x1b[0m 2026-01-02T12:23:59.123Z after 5ms at 0xabc /app/code:9:4)\n";
        fs::write(&log, trace).unwrap();
        let (tail, hash) = failure_output(&log, Path::new("/app")).unwrap();
        assert!(tail.starts_with("noisy info\nTraceback"));
        assert_eq!(hash, "87082322");
        fs::write(&log, trace.replace("assert a == b", "assert a == c")).unwrap();
        assert_ne!(failure_output(&log, Path::new("/app")).unwrap().1, hash);
        fs::write(&log, "é".repeat(1300)).unwrap();
        assert_eq!(
            failure_output(&log, Path::new("/app")).unwrap().0,
            "é".repeat(1200)
        );
        assert_eq!(
            normalize("Error a:12:3x b:5  c:8:", Path::new("/app")),
            "Error a<line>:3x b<line> c<line>:"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fresh_runners_repair_source_changes_and_preserve_manual_qa() {
        let mut f = Fixture::new();
        f.cfg.repro_cmd = shell("echo reproduction");
        f.cfg.build_cmd = shell("echo built");
        let ctx = Context::new();
        f.run("awaiting_subagents", &ctx, None);
        let packet = f.read("fix_packet.json");
        assert_eq!(packet["revision"], 0);
        assert_eq!(packet["bug_spec"], json!(f.cfg.bug_spec));
        assert_eq!(packet["repo_dir"], json!(f.cfg.repo_dir));
        assert_eq!(packet["outputs"], json!([f.dir.join("fix_report.json")]));
        assert_eq!(f.read("bug_spec.json")["watch_globs"], json!(["value"]));
        f.deliver(0);
        f.run("awaiting_subagents", &ctx, None);
        let first = f.read("fix_packet.json");
        let failure = &first["repair_feedback"][0];
        assert_eq!(first["revision"], 1);
        assert!(failure["detail"].as_str().unwrap().contains("Error wrong"));
        assert_eq!(
            failure["error_signature"],
            f.read("test_result.json")["error_signature"]
        );
        assert_eq!(first["log_paths"].as_object().unwrap().len(), 3);
        f.value("other");
        f.deliver(1);
        f.run("awaiting_subagents", &ctx, None);
        let next = f.read("fix_packet.json");
        assert_eq!(next["revision"], 2);
        assert_eq!(next["repair_feedback"][0], *failure);
        assert_ne!(
            next["repair_feedback"][1]["error_signature"],
            failure["error_signature"]
        );
        f.value("good");
        f.deliver(2);
        f.run("passed", &ctx, None);
        let summary = f.read("final_summary.json");
        assert_eq!(summary["attempts"], 3);
        assert_eq!(summary["revision"], 2);
        assert_eq!(f.read("repair_state.json")["consecutive_repairs"], 0);
        let qa = f.dir.join("manual_qa.json");
        let case = &f.read("manual_qa.json")["cases"][0];
        assert_eq!(case["platform"], "macOS");
        assert_eq!(case["ai_attempts"], 3);
        assert_eq!(case["manual_qa"], "not_tested");
        record_qa_result(
            &qa,
            &QaResultInput {
                case_id: "app-42".into(),
                manual_qa: "pass".into(),
                gap_type: None,
                notes: None,
                resolved_on_attempt: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(f.run("passed", &ctx, Some(0)).len(), 5);
        f.value("good\n");
        assert_eq!(
            f.run("passed", &ctx, Some(0)),
            ["app-42/triage_and_plan", "app-42/apply_fix"]
        );
        fs::remove_file(f.dir.join("test.log")).unwrap();
        fs::remove_file(f.dir.join("final_summary.json")).unwrap();
        f.run("passed", &ctx, Some(0));
        assert_eq!(f.read("final_summary.json"), summary);
        assert_eq!(
            f.read("manual_qa.json")["cases"][0]["resolved_on_attempt"],
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn repeated_volatile_failure_trips_native_breaker_on_second_delivery() {
        let mut f = Fixture::new();
        f.cfg.test_cmd = shell("n=$(cat value); printf 'ERROR 2026-01-02T03:04:%sZ after 0.%ss at 0xabc%s line %s\n' \"$n\" \"$n\" \"$n\" \"$n\"; exit 7");
        let ctx = Context::new();
        f.run("awaiting_subagents", &ctx, None);
        f.value("04");
        f.deliver(0);
        f.run("awaiting_subagents", &ctx, None);
        let first = f.read("test_result.json");
        f.value("05");
        f.deliver(1);
        f.run("circuit_broken", &ctx, None);
        let second = f.read("test_result.json");
        assert_eq!(first["error_signature"], second["error_signature"]);
        assert!(first["error_signature"]
            .as_str()
            .unwrap()
            .starts_with("test_exit_7:"));
        assert_ne!(first["detail"], second["detail"]);
    }

    #[cfg(unix)]
    #[test]
    fn failed_build_cleans_logs_and_requires_requested_revision_before_retesting() {
        let mut f = Fixture::new();
        f.cfg.build_cmd = shell("test \"$(cat value)\" = good || { echo SyntaxError; exit 1; }");
        f.cfg.repro_cmd = shell("touch repro_ran");
        f.cfg.test_cmd = shell("touch test_ran");
        let ctx = Context::new();
        f.run("awaiting_subagents", &ctx, None);
        for name in ["repro.log", "test.log"] {
            fs::write(f.dir.join(name), "stale").unwrap();
        }
        f.deliver(0);
        f.run("awaiting_subagents", &ctx, None);
        let packet = f.read("fix_packet.json");
        assert_eq!(
            packet["log_paths"],
            json!({"build":f.dir.join("build.log")})
        );
        assert_eq!(f.read("build_result.json"), f.read("test_result.json"));
        assert!(f.read("build_result.json")["detail"]
            .as_str()
            .unwrap()
            .contains("SyntaxError"));
        assert!(!f.cfg.repo_dir.join("repro_ran").exists());
        assert!(!f.cfg.repo_dir.join("test_ran").exists());
        f.value("good");
        f.deliver(0);
        let failure = f.runner(None).run(&f.graph(), &ctx, "").unwrap_err();
        assert!(failure.reason.contains("revision 1"));
        assert_eq!(f.read("fix_packet.json"), packet);
        f.deliver(1);
        f.run("passed", &ctx, None);
        assert!(f.cfg.repo_dir.join("repro_ran").is_file());
        assert!(f.cfg.repo_dir.join("test_ran").is_file());
    }

    #[test]
    fn build_timeout_terminates_grandchild_and_preserves_raw_merged_log() {
        let f = Fixture::new();
        let repo = &f.cfg.repo_dir;
        let log = repo.join("build.log");
        let listener = "import socket, time; from pathlib import Path; s=socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(); Path('port').write_text(str(s.getsockname()[1])); time.sleep(10)";
        let cmd = CommandSpec::Args(vec![
            "python3".into(),
            "-c".into(),
            r#"import os, sys, subprocess
os.write(2, b'err:')
os.write(1, b'out\xff\n\n')
subprocess.run([sys.executable, '-c', sys.argv[1]])
"#
            .into(),
            listener.into(),
        ]);
        let result = run_command("build", &cmd, &log, repo, Some(1.5)).unwrap();
        assert_eq!(result.error_signature, "build_timeout");
        assert_eq!(fs::read(&log).unwrap(), b"err:out\xff\n\n");
        let port: u16 = fs::read_to_string(repo.join("port"))
            .unwrap()
            .parse()
            .unwrap();
        // The port was listening before the timeout. A surviving grandchild still owns it.
        let deadline = Instant::now() + Duration::from_secs(1);
        while std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "grandchild survived build timeout"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_launch_error_preserve_stage_feedback() {
        let f = Fixture::new();
        let log = f.cfg.repo_dir.join("sample.log");
        let cmd = CommandSpec::Args(vec![
            "sh".into(),
            "-c".into(),
            "echo Starting; exec sleep 2".into(),
        ]);
        let result = run_command("repro", &cmd, &log, &f.cfg.repo_dir, Some(0.15)).unwrap();
        assert_eq!(result.error_signature, "repro_timeout");
        assert_eq!(result.detail, "repro timed out after 0.15s\nStarting");
        let cmd = CommandSpec::Args(vec!["nonexistent-kgraph-command".into()]);
        let result = run_command("test", &cmd, &log, &f.cfg.repo_dir, None).unwrap();
        assert_eq!(result.error_signature, "test_launch_error");
        assert!(result.detail.contains("nonexistent-kgraph-command"));
        assert!(!result.detail.contains("Starting"));
    }

    #[test]
    fn report_validation_and_custom_verifier_track_context_without_telemetry() {
        let mut f = Fixture::new();
        f.cfg.test_cmd = None;
        f.cfg.verify_fn = Some(Arc::new(|ctx| {
            Ok(if ctx.get("approve") == Some(json!(false)) {
                VerifyOutcome::fail("custom", "rejected")
            } else {
                VerifyOutcome::pass("accepted")
            })
        }));
        f.cfg.review_feedback = Some(object(json!({"fix_revision":2,"review_attempts":2})));
        let ctx = Context::new();
        f.run("awaiting_subagents", &ctx, None);
        assert_eq!(f.read("fix_packet.json")["revision"], 2);
        write_json(
            &f.dir.join("fix_report.json"),
            &json!({"revision":2,"root_cause":" ","summary":"fix","changed_files":"value"}),
            true,
        )
        .unwrap();
        f.run("awaiting_subagents", &ctx, None);
        let packet = f.read("fix_packet.json");
        assert_eq!(packet["revision"], 3);
        assert_eq!(
            packet["repair_feedback"][0]["error_signature"],
            "invalid_fix_report"
        );
        f.deliver(3);
        f.run("passed", &ctx, None);
        assert_eq!(f.read("final_summary.json")["attempts"], 4);
        assert_eq!(f.run("passed", &ctx, Some(0)).len(), 5);
        ctx.insert("approve", json!(false));
        f.run("awaiting_subagents", &ctx, None);
        assert_eq!(f.read("test_result.json")["error_signature"], "custom");
        assert!(!f.dir.join("final_summary.json").exists());
    }

    #[test]
    fn verifies_each_report_field_and_accepts_an_empty_changed_files_list() {
        let mut f = Fixture::new();
        f.cfg.test_cmd = None;
        f.cfg.verify_fn = Some(Arc::new(|_| Ok(VerifyOutcome::pass("ok"))));
        let graph = create_desktop_bug_fix_subgraph(f.cfg.clone()).unwrap();
        let Node::Task(final_stage) = &graph.nodes[4] else {
            panic!("missing final verifier")
        };
        for name in ["build_result.json", "test_result.json"] {
            write_json(
                &f.dir.join(name),
                &json!({"ok":true,"error_signature":"","detail":"ok"}),
                true,
            )
            .unwrap();
        }
        let valid = json!({"revision":0, "summary":"fixed", "root_cause":"bad setting", "changed_files":[]});
        for (key, bad) in [
            ("revision", json!(-1)),
            ("revision", json!(0.5)),
            ("revision", json!(true)),
            ("summary", json!("  ")),
            ("root_cause", Value::Null),
            ("changed_files", json!("file")),
            ("changed_files", json!(["file", " "])),
        ] {
            let mut report = valid.clone();
            report[key] = bad;
            write_json(&f.dir.join("fix_report.json"), &report, true).unwrap();
            (final_stage.run_fn)(&Context::new(), 1).unwrap();
            assert_eq!(
                f.read("verification.json")["error_signature"],
                "invalid_fix_report"
            );
            assert!(!f.dir.join("final_summary.json").exists());
        }
        write_json(&f.dir.join("fix_report.json"), &valid, true).unwrap();
        (final_stage.run_fn)(&Context::new(), 1).unwrap();
        assert_eq!(f.read("verification.json")["ok"], true);
        assert_eq!(f.read("final_summary.json")["changed_files"], json!([]));
    }

    #[cfg(unix)]
    #[test]
    fn batch_resumes_in_order_applies_overrides_and_writes_consolidated_reports() {
        let f = Fixture::new();
        let mut cfg = BugBatchConfig {
            bugs: vec![
                object(json!({"id":"alpha","title":"Alpha"})),
                object(
                    json!({"id":"beta", "title":"Beta", "test_cmd":["echo","beta"],"watch_globs":["value"], "max_retries":2}),
                ),
            ],
            repo_dir: f.cfg.repo_dir.clone(),
            work_dir: f.cfg.work_dir.clone(),
            test_cmd: shell("echo alpha"),
            tester: Some("Tester".into()),
            ..Default::default()
        };
        let run = |cfg: &BugBatchConfig| {
            f.runner(None)
                .run(
                    &create_bug_batch_graph(cfg.clone()).unwrap(),
                    &Context::new(),
                    "",
                )
                .map_err(Box::new)
        };
        let nodes = create_bug_batch_graph(cfg.clone()).unwrap();
        assert_eq!(nodes[1].deps(), ["alpha"]);
        assert_eq!(nodes[2].deps(), ["alpha", "beta"]);
        for n in nodes {
            assert_eq!(
                n.qa_checklist_path(),
                Some(&cfg.work_dir.join("manual_qa.json"))
            );
        }
        for id in ["alpha", "beta"] {
            let error = run(&cfg).unwrap_err();
            let dir = cfg.work_dir.join("bugs").join(id);
            let packet = read_object(&dir.join("fix_packet.json")).unwrap();
            assert_eq!(
                error.packet_path.as_deref(),
                dir.join("fix_packet.json").to_str()
            );
            assert_eq!(packet["bug_id"], id);
            if id == "alpha" {
                assert!(!cfg.work_dir.join("bugs/beta/fix_packet.json").exists());
            } else {
                assert_eq!(packet["commands"]["test"], json!(["echo", "beta"]));
                assert_eq!(packet["watch_globs"], json!(["value"]));
                assert!(packet["bug_spec"].get("max_retries").is_none());
            }
            assert!(!cfg.work_dir.join("batch_report.md").exists());
            write_json(&dir.join("fix_report.json"),&json!({"revision":0,"root_cause":"defaults", "summary":format!("Fixed {id}"),"changed_files":["value"]}),true).unwrap();
        }
        run(&cfg).unwrap();
        let report = read_object(&cfg.work_dir.join("batch_report.json")).unwrap();
        assert_eq!(report["fixed_count"], 2);
        assert_eq!(report["bugs"][0]["bug_id"], "alpha");
        assert_eq!(report["bugs"][1]["bug_id"], "beta");
        assert_eq!(
            read_object(&cfg.work_dir.join("qa_scorecard.json")).unwrap()["counts"]["not_tested"],
            2
        );
        let text = fs::read_to_string(cfg.work_dir.join("batch_report.md")).unwrap();
        assert!(text.contains("## alpha — Alpha\n\nFixed alpha"));
        assert!(text.contains("## beta — Beta\n\nFixed beta"));
        assert!(fs::read_to_string(cfg.work_dir.join("qa_scorecard.md"))
            .unwrap()
            .contains("QA Scorecard"));
        cfg.resource = None;
        assert!(create_bug_batch_graph(cfg).unwrap()[1].deps().is_empty());
    }
}
