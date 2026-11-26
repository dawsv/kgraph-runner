//! Run a shell script or an argument vector with live logging and a time limit.

use crate::types::{
    Context, InputHashFn, OutputsFn, RepairFn, TaskKind, TaskNode, TaskRunError, VerifyFn,
    VerifyOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub enum CommandSpec {
    Shell(String),
    Args(Vec<String>),
}

pub type CommandFn = Arc<dyn Fn(&Context) -> Result<CommandSpec, String> + Send + Sync>;

#[derive(Clone)]
pub enum ShellCommand {
    Static(CommandSpec),
    Dynamic(CommandFn),
}

impl From<CommandSpec> for ShellCommand {
    fn from(spec: CommandSpec) -> Self {
        Self::Static(spec)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct ShellTaskOptions {
    pub id: String,
    pub command: ShellCommand,
    pub input_hash_fn: InputHashFn,
    pub outputs_fn: OutputsFn,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub kind: TaskKind,
    pub deps: Vec<String>,
    pub timeout_s: Option<f64>,
    pub max_retries: usize,
    pub resource: Option<String>,
    pub cacheable: bool,
    pub log_path: Option<PathBuf>,
    pub verify_fn: Option<VerifyFn>,
    pub repair_fn: Option<RepairFn>,
}

impl ShellTaskOptions {
    pub fn new(
        id: impl Into<String>,
        command: impl Into<ShellCommand>,
        input_hash_fn: InputHashFn,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            input_hash_fn,
            outputs_fn: Arc::new(|_| Ok(Vec::new())),
            cwd: None,
            env: BTreeMap::new(),
            kind: TaskKind::Code,
            deps: Vec::new(),
            timeout_s: None,
            max_retries: 0,
            resource: None,
            cacheable: true,
            log_path: None,
            verify_fn: None,
            repair_fn: None,
        }
    }
}

struct CommandProcess {
    child: Child,
    armed: bool,
}

impl CommandProcess {
    fn spawn(mut command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self { child, armed: true })
    }

    fn terminate(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            // SAFETY: process_group(0) makes the child's PID its dedicated group ID.
            if unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        #[cfg(not(unix))]
        if self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        self.child.wait()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for CommandProcess {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.terminate();
        }
    }
}

// Keep incomplete UTF-8 and CRLF sequences across reads, including at EOF.
#[derive(Default)]
struct StreamDecoder {
    bytes: Vec<u8>,
    pending_cr: bool,
}

impl StreamDecoder {
    fn decode(&mut self, chunk: &[u8], eof: bool) -> String {
        self.bytes.extend_from_slice(chunk);
        let mut remaining = self.bytes.as_slice();
        let mut decoded = String::new();
        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    remaining = &[];
                    break;
                }
                Err(error) => {
                    let (valid, rest) = remaining.split_at(error.valid_up_to());
                    decoded.push_str(std::str::from_utf8(valid).expect("validated prefix"));
                    remaining = rest;
                    match error.error_len() {
                        Some(len) => {
                            decoded.push('\u{fffd}');
                            remaining = &remaining[len..];
                        }
                        None => {
                            if eof {
                                decoded.push('\u{fffd}');
                                remaining = &[];
                            }
                            break;
                        }
                    }
                }
            }
        }
        let consumed = self.bytes.len() - remaining.len();
        self.bytes.drain(..consumed);
        if decoded.is_empty() && !eof {
            return decoded;
        }
        if self.pending_cr {
            decoded.insert(0, '\r');
        }
        self.pending_cr = !eof && decoded.ends_with('\r');
        if self.pending_cr {
            decoded.pop();
        }
        decoded.replace("\r\n", "\n").replace('\r', "\n")
    }
}

#[derive(Default)]
struct Capture {
    log: Option<File>,
    stdout: String,
    stderr: String,
    closed: bool,
}

impl Capture {
    fn finish(&mut self, returncode: i32) -> io::Result<CommandOutput> {
        // A descendant outside the group may still hold a pipe open. Detached
        // readers must never append to the final log after the drain deadline.
        self.closed = true;
        if let Some(log) = &mut self.log {
            log.rewind()?;
            for stream in [&self.stdout, &self.stderr] {
                if !stream.is_empty() {
                    writeln!(log, "{}", stream.trim_end_matches('\n'))?;
                }
            }
            let len = log.stream_position()?;
            log.set_len(len)?;
            log.flush()?;
        }
        // Release the file even if a reader is still blocked in read().
        self.log.take();
        Ok(CommandOutput {
            returncode,
            stdout: std::mem::take(&mut self.stdout),
            stderr: std::mem::take(&mut self.stderr),
        })
    }
}

fn spawn_reader(
    mut pipe: impl Read + Send + 'static,
    capture: Arc<Mutex<Capture>>,
    stderr: bool,
) -> io::Result<thread::JoinHandle<io::Result<()>>> {
    thread::Builder::new().spawn(move || {
        let mut decoder = StreamDecoder::default();
        let mut buffer = [0; 65536];
        loop {
            let n = match pipe.read(&mut buffer) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            let chunk = decoder.decode(&buffer[..n], n == 0);
            let mut state = capture.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Ok(());
            }
            if stderr {
                state.stderr.push_str(&chunk);
            } else {
                state.stdout.push_str(&chunk);
            }
            if let Some(log) = &mut state.log {
                log.write_all(chunk.as_bytes())?;
                log.flush()?;
            }
            if n == 0 {
                return Ok(());
            }
        }
    })
}

fn decode_all(bytes: Vec<u8>) -> String {
    let text = String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if !text.contains('\r') {
        return text;
    }
    let mut normalized = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some((line, tail)) = rest.split_once('\r') {
        normalized.push_str(line);
        normalized.push('\n');
        rest = tail.strip_prefix('\n').unwrap_or(tail);
    }
    normalized.push_str(rest);
    normalized
}

fn read_pipe(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn capture_output(process: &mut CommandProcess) -> Result<CommandOutput, TaskRunError> {
    // Drain both pipes concurrently while retaining the process-group guard on every error.
    let stderr = process.child.stderr.take().expect("piped stderr");
    let reader = thread::Builder::new()
        .spawn(move || read_pipe(stderr))
        .map_err(command_io)?;
    let stdout =
        read_pipe(process.child.stdout.take().expect("piped stdout")).map_err(command_io)?;
    let stderr = reader
        .join()
        .map_err(|_| TaskRunError::other("Command reader panicked"))?
        .map_err(command_io)?;
    let status = process.child.wait().map_err(command_io)?;
    process.armed = false;
    Ok(CommandOutput {
        returncode: returncode(status),
        stdout: decode_all(stdout),
        stderr: decode_all(stderr),
    })
}

fn returncode(status: ExitStatus) -> i32 {
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

fn command_io(error: io::Error) -> TaskRunError {
    TaskRunError::other(error.to_string())
}

pub fn run_command_streaming(
    cmd: &CommandSpec,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    timeout_s: Option<f64>,
    log_path: Option<&Path>,
) -> Result<CommandOutput, TaskRunError> {
    let timeout = timeout_s
        .map(Duration::try_from_secs_f64)
        .transpose()
        .map_err(|e| TaskRunError::other(format!("Invalid command timeout: {e}")))?;
    let mut command = match cmd {
        CommandSpec::Shell(script) => {
            #[cfg(windows)]
            let (shell, flag) = ("cmd", "/C");
            #[cfg(not(windows))]
            let (shell, flag) = ("sh", "-c");
            let mut command = Command::new(shell);
            command.arg(flag).arg(script);
            command
        }
        CommandSpec::Args(args) => {
            let (program, args) = args
                .split_first()
                .ok_or_else(|| TaskRunError::other("Command argument list is empty"))?;
            let mut command = Command::new(program);
            command.args(args);
            command
        }
    };
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.envs(env);
    if let Some(parent) = log_path.and_then(Path::parent) {
        fs::create_dir_all(parent).map_err(command_io)?;
    }
    let mut process = CommandProcess::spawn(command).map_err(command_io)?;
    if timeout.is_none() && log_path.is_none() {
        return capture_output(&mut process);
    }
    let log = log_path.map(File::create).transpose().map_err(command_io)?;
    let capture = Arc::new(Mutex::new(Capture {
        log,
        ..Capture::default()
    }));
    let start = Instant::now();
    let stdout = process.child.stdout.take().expect("piped stdout");
    let stderr = process.child.stderr.take().expect("piped stderr");
    let mut readers = vec![
        spawn_reader(stdout, capture.clone(), false).map_err(command_io)?,
        spawn_reader(stderr, capture.clone(), true).map_err(command_io)?,
    ];

    let mut poll_delays = [200, 500, 1000, 2000, 5000].into_iter();
    let max_interval = Duration::from_millis(10);
    let status = loop {
        // Surface reader failures even while a producer is still running.
        let mut i = 0;
        while i < readers.len() {
            if readers[i].is_finished() {
                readers
                    .swap_remove(i)
                    .join()
                    .map_err(|_| TaskRunError::other("Command reader panicked"))?
                    .map_err(command_io)?;
            } else {
                i += 1;
            }
        }
        if let Some(status) = process.child.try_wait().map_err(command_io)? {
            if readers.is_empty() {
                break Some(status);
            }
        }
        let poll_interval = poll_delays
            .next()
            .map(Duration::from_micros)
            .unwrap_or(max_interval);
        let delay = timeout
            .map(|limit| limit.saturating_sub(start.elapsed()))
            .unwrap_or(poll_interval);
        if delay.is_zero() {
            process.terminate().map_err(command_io)?;
            let drain_start = Instant::now();
            while readers.iter().any(|r| !r.is_finished())
                && drain_start.elapsed() < Duration::from_secs(1)
            {
                thread::sleep(max_interval);
            }
            for reader in readers {
                if reader.is_finished() {
                    let _ = reader.join();
                }
            }
            break None;
        }
        thread::sleep(delay.min(poll_interval));
    };
    process.armed = false;
    let output = capture
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .finish(status.map(returncode).unwrap_or(-1))
        .map_err(command_io)?;
    if status.is_none() {
        return Err(TaskRunError::Timeout(format!(
            "Command timed out after {}s",
            timeout_s.expect("elapsed timeout")
        )));
    }
    Ok(output)
}

pub fn shell_task(opts: ShellTaskOptions) -> TaskNode {
    let run = Arc::new(move |ctx: &Context, _| {
        let dynamic;
        let cmd = match &opts.command {
            ShellCommand::Static(spec) => spec,
            ShellCommand::Dynamic(build) => {
                dynamic = build(ctx).map_err(TaskRunError::other)?;
                &dynamic
            }
        };
        let output = run_command_streaming(
            cmd,
            opts.cwd.as_deref(),
            &opts.env,
            opts.timeout_s,
            opts.log_path.as_deref(),
        )?;
        serde_json::to_value(output).map_err(|e| TaskRunError::other(e.to_string()))
    });
    let mut node = TaskNode::new(opts.id, opts.kind, run, opts.input_hash_fn);
    node.outputs_fn = opts.outputs_fn;
    node.verify_fn = Some(Arc::new(move |ctx, result| {
        let Some(code) = result["returncode"].as_i64() else {
            return VerifyOutcome::fail("invalid_command_output", "Missing command return code");
        };
        if code != 0 {
            let detail = result["stderr"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| result["stdout"].as_str())
                .unwrap_or_default()
                .trim();
            let mut message = format!("Command exited with code {code}");
            if !detail.is_empty() {
                message.push_str(": ");
                message.push_str(detail);
            }
            return VerifyOutcome::fail(format!("exit_{code}"), message);
        }
        match &opts.verify_fn {
            Some(verify) => verify(ctx, result),
            None => VerifyOutcome::pass("Command passed"),
        }
    }));
    node.repair_fn = opts.repair_fn;
    node.deps = opts.deps;
    node.max_retries = opts.max_retries;
    node.resource = opts.resource;
    node.cacheable = opts.cacheable;
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use serde_json::json;
    #[cfg(unix)]
    use tempfile::tempdir;

    fn options(cmd: impl Into<ShellCommand>) -> ShellTaskOptions {
        ShellTaskOptions::new("check", cmd, Arc::new(|_| Ok("input".into())))
    }

    #[test]
    fn whole_output_matches_streaming_decoding() {
        let input = b"first\r\nsecond\rthird\xe2\x82\xac\xff\xf0\x9f";
        let expected = StreamDecoder::default().decode(input, true);
        assert_eq!(decode_all(input.to_vec()), expected);
    }

    #[test]
    fn decoder_preserves_split_multibyte_and_crlf_and_replaces_invalid_utf8() {
        let mut decoder = StreamDecoder::default();
        let chunks: &[&[u8]] = &[
            b"a\r",
            b"\n\xe2",
            b"\x82",
            b"\xac\xc2",
            b"b\r\r",
            b"\xff\xe2\x82",
        ];
        let mut text = String::new();
        for chunk in chunks {
            text.push_str(&decoder.decode(chunk, false));
        }
        text.push_str(&decoder.decode(&[], true));
        assert_eq!(text, "a\n€�b\n\n��");
    }

    #[test]
    fn dynamic_command_error_propagates_before_spawning() {
        let cmd = ShellCommand::Dynamic(Arc::new(|_| Err("missing executable setting".into())));
        let result = (shell_task(options(cmd)).run_fn)(&Context::new(), 0);
        assert!(
            matches!(result, Err(TaskRunError::Other { detail, .. }) if detail == "missing executable setting")
        );
    }

    #[cfg(unix)]
    #[test]
    fn static_shell_honors_working_directory_and_environment() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("input"), b"local file").unwrap();
        let log = dir.path().join("logs/output");
        let mut opts = options(CommandSpec::Shell(
            r#"cat input; printf '%s' "$MODE" >&2"#.into(),
        ));
        opts.cwd = Some(dir.path().to_owned());
        opts.log_path = Some(log.clone());
        opts.env.insert("MODE".into(), "test".into());
        let task = shell_task(opts);
        let ctx = Context::new();
        let output = (task.run_fn)(&ctx, 0).unwrap();
        assert_eq!(
            output,
            json!({"returncode": 0, "stdout": "local file", "stderr": "test"})
        );
        assert_eq!(fs::read_to_string(log).unwrap(), "local file\ntest\n");
        assert_eq!(
            task.verify_fn.unwrap()(&ctx, &output),
            VerifyOutcome::pass("Command passed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dynamic_argv_preserves_literal_arguments_and_invokes_verifier() {
        let literal = "one argument; $HOME";
        let ctx = Context::from_value(json!({"text": literal})).unwrap();
        let mut opts = options(ShellCommand::Dynamic(Arc::new(|ctx| {
            let text = ctx.get("text").unwrap();
            Ok(CommandSpec::Args(vec![
                "printf".into(),
                "%s".into(),
                text.as_str().unwrap().into(),
            ]))
        })));
        opts.verify_fn = Some(Arc::new(|ctx, output| {
            if Some(output["stdout"].clone()) == ctx.get("text") {
                VerifyOutcome::pass("Output verified")
            } else {
                VerifyOutcome::fail("unexpected_output", "Output did not match")
            }
        }));
        let task = shell_task(opts);
        let output = (task.run_fn)(&ctx, 0).unwrap();
        let verify = task.verify_fn.unwrap();
        assert_eq!(output["stdout"], literal);
        assert_eq!(
            verify(&ctx, &output),
            VerifyOutcome::pass("Output verified")
        );
        ctx.insert("text", json!("changed"));
        assert_eq!(verify(&ctx, &output).error_signature, "unexpected_output");
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_prefers_stderr_and_skips_custom_verifier() {
        let mut opts = options(CommandSpec::Shell(
            "echo out; echo ' check failed ' >&2; exit 7".into(),
        ));
        opts.verify_fn = Some(Arc::new(|_, _| panic!("must skip verifier")));
        let task = shell_task(opts);
        let ctx = Context::new();
        let output = (task.run_fn)(&ctx, 0).unwrap();
        assert_eq!(
            task.verify_fn.unwrap()(&ctx, &output),
            VerifyOutcome::fail("exit_7", "Command exited with code 7: check failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn both_streams_reach_live_log_before_exit() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("live.log");
        let mut opts = options(CommandSpec::Shell(
            "printf 'error\\r\\n' >&2; printf 'first\\r\\n'; while [ ! -e release ]; do sleep 0.02; done; printf 'last\\377\\r'".into()
        ));
        opts.cwd = Some(dir.path().to_owned());
        opts.log_path = Some(log.clone());
        opts.timeout_s = Some(5.0);
        let running = thread::spawn(move || (shell_task(opts).run_fn)(&Context::new(), 0));
        let start = Instant::now();
        let streamed = loop {
            let partial = fs::read_to_string(&log).unwrap_or_default();
            if partial.contains("first\n") && partial.contains("error\n") {
                break !running.is_finished();
            }
            if start.elapsed() > Duration::from_secs(3) || running.is_finished() {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        };
        fs::write(dir.path().join("release"), "").unwrap();
        let output = running.join().unwrap().unwrap();
        assert!(streamed, "both pipes should stream before release");
        assert_eq!(output["stdout"], "first\nlast�\n");
        assert_eq!(output["stderr"], "error\n");
        assert_eq!(fs::read_to_string(log).unwrap(), "first\nlast�\nerror\n");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_child_subshell_even_after_leader_exits() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("timeout.log");
        let mut opts = options(CommandSpec::Shell(
            "(echo 'partial error' >&2; echo started; sleep 0.8; touch survived) & exit 0".into(),
        ));
        opts.log_path = Some(log.clone());
        opts.cwd = Some(dir.path().to_owned());
        opts.timeout_s = Some(0.3);
        assert!(
            matches!((shell_task(opts).run_fn)(&Context::new(), 0), Err(TaskRunError::Timeout(s)) if s == "Command timed out after 0.3s")
        );
        assert_eq!(fs::read_to_string(log).unwrap(), "started\npartial error\n");
        thread::sleep(Duration::from_millis(600));
        assert!(
            !dir.path().join("survived").exists(),
            "descendant survived timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_process_guard_terminates_descendants_without_timeout() {
        use std::io::{BufRead, BufReader};
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "(echo ready; sleep 2; echo survived) & wait"]);
        let mut guard = CommandProcess::spawn(cmd).unwrap();
        let mut pipe = BufReader::new(guard.child.stdout.take().unwrap());
        let mut output = String::new();
        pipe.read_line(&mut output).unwrap();
        assert_eq!(output, "ready\n");
        drop(guard);
        pipe.read_to_string(&mut output).unwrap();
        assert_eq!(output, "ready\n");
    }
}
