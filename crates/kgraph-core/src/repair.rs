//! Bounded evidence for a fresh worker after a failed verification attempt.

use crate::{contracts::open_regular, Context};
use chrono::Utc;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

const LOG_LIMIT: usize = 6 * 1024;
const HISTORY_LIMIT: usize = 8;
const DETAIL_LIMIT: usize = 2048;
const FILE_LIMIT: usize = 64;
const CONTEXT_KEY: &str = "__kgraph_repair";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSummary {
    pub attempt: usize,
    pub error_signature: String,
    pub detail: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairContext {
    pub node_path: String,
    pub attempt: usize,
    pub next_attempt: usize,
    pub max_retries: usize,
    pub remaining_attempts: usize,
    pub error_signature: String,
    pub detail: String,
    /// Failures preceding next_attempt, including the latest failed attempt.
    pub prior_attempts: Vec<AttemptSummary>,
    pub log_tail: Option<String>,
    pub git_diff_stat: Option<String>,
    pub changed_files: Vec<String>,
}

static CREDENTIAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
    r"(?i)\b(?:bearer|basic)[ \t]+[\w.+/=~-]+|",
    r#"["']?\b[\w-]*(?:token|password|passwd|secret|api[_-]?key|credential)[\w-]*["']?\s*[:=]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|(?:bearer|basic)[ \t]+[\w.+/=~-]+|[^\s,;&]+)|"#,
    r"\b(?:sk-|gh[pousr]_|github_pat_|xox[baprs]-|AIza)[\w-]+"
)).expect("credential regex")
});

static ERROR_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:error\[|\berror:|failed|panic|traceback|assertion|exception|rejected)")
        .expect("error line regex")
});

pub fn redact_secrets(text: &str) -> String {
    CREDENTIAL.replace_all(text, "[redacted]").into_owned()
}

/// Compact retry feedback to fit `max_bytes` while preserving high-signal error lines
/// and the trailing context (mitigates context rot on multi-attempt loops).
pub fn compact_feedback(text: &str, max_bytes: usize) -> String {
    let clean = redact_secrets(text);
    if max_bytes == 0 {
        return String::new();
    }
    if clean.len() <= max_bytes {
        return clean;
    }
    let marker = "\n...[compacted for retry context budget]...\n";
    if max_bytes <= marker.len() + 32 {
        return prefix(&clean, max_bytes).to_owned();
    }
    let budget = max_bytes - marker.len();
    let head_budget = (budget * 35) / 100;
    let mut head = String::new();
    for line in clean.lines() {
        if ERROR_LINE.is_match(line) {
            let needed = line.len() + usize::from(!head.is_empty());
            if head.len() + needed <= head_budget {
                if !head.is_empty() {
                    head.push('\n');
                }
                head.push_str(line);
            }
        }
    }
    if head.is_empty() {
        head.push_str(prefix(&clean, head_budget));
    }
    let tail_budget = budget.saturating_sub(head.len());
    let mut start = clean.len().saturating_sub(tail_budget);
    while start < clean.len() && !clean.is_char_boundary(start) {
        start += 1;
    }
    format!("{head}{marker}{}", &clean[start..])
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn evidence(text: &str, limit: usize) -> String {
    prefix(&redact_secrets(text), limit).to_owned()
}

fn log_tail(path: &Path) -> Option<String> {
    let mut file = open_regular(path).ok()?;
    let length = file.metadata().ok()?.len();
    let window = (LOG_LIMIT * 2) as u64;
    let start = length.saturating_sub(window);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(window).read_to_end(&mut bytes).ok()?;
    // The partial leading line may begin inside a credential of arbitrary length.
    let bytes = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(&[][..], |end| &bytes[end + 1..])
    } else {
        &bytes
    };
    let clean = redact_secrets(&String::from_utf8_lossy(bytes));
    let mut cutoff = clean.len().saturating_sub(LOG_LIMIT);
    while !clean.is_char_boundary(cutoff) {
        cutoff += 1;
    }
    Some(clean[cutoff..].to_owned())
}

// A large checkout can emit megabytes of status. Stop the producer once enough
// evidence is available and keep only complete path records in the bundle.
fn git_prefix(dir: &Path, args: &[&str], limit: usize) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().ok()?;
    read.ok()?;
    if !truncated && !status.success() {
        return None;
    }
    bytes.truncate(limit);
    Some(bytes)
}

/// attempt is one-based. max_retries excludes the initial execution.
pub fn build_repair_context(
    node_path: &str,
    attempt: usize,
    max_retries: usize,
    error_signature: &str,
    detail: &str,
    log_path: Option<&Path>,
    repo_dir: Option<&Path>,
) -> RepairContext {
    let attempt = attempt.max(1);
    let error_signature = evidence(error_signature, 512);
    let detail = evidence(detail, DETAIL_LIMIT);
    let summary = AttemptSummary {
        attempt,
        error_signature: error_signature.clone(),
        detail: detail.clone(),
        timestamp: Utc::now().to_rfc3339(),
    };
    let git_diff_stat = repo_dir
        .and_then(|dir| {
            git_prefix(
                dir,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--stat",
                    "HEAD",
                    "--",
                ],
                8192,
            )
        })
        .map(|b| evidence(String::from_utf8_lossy(&b).trim_end(), 4096))
        .filter(|s| !s.is_empty());
    let mut changed_files = Vec::new();
    if let Some(status) = repo_dir.and_then(|dir| {
        git_prefix(
            dir,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--no-renames",
                "--untracked-files=all",
            ],
            64 * 1024,
        )
    }) {
        for record in status.split_inclusive(|b| *b == 0).take(FILE_LIMIT) {
            if let Some(path) = record.strip_suffix(&[0]).and_then(|r| r.get(3..)) {
                changed_files.push(evidence(&String::from_utf8_lossy(path), 512));
            }
        }
    }
    RepairContext {
        node_path: node_path.into(),
        attempt,
        next_attempt: attempt.saturating_add(1),
        max_retries,
        remaining_attempts: max_retries.saturating_sub(attempt - 1),
        error_signature,
        detail,
        prior_attempts: vec![summary],
        log_tail: log_path.and_then(log_tail),
        git_diff_stat,
        changed_files,
    }
}

// A loop repair callback can attach evidence to itself or to a relative child.
fn scoped_repair_path(node: &str, active: &str) -> String {
    if node == active || active.rsplit('/').next() == Some(node) {
        active.to_owned()
    } else if node.starts_with(&format!("{active}/")) {
        node.to_owned()
    } else {
        format!("{active}/{node}")
    }
}

/// Merge the node's history while holding the context write lock. Reattaching
/// the same failure retains its original timestamp.
pub fn attach_repair_context(ctx: &Context, repair: &RepairContext) {
    let mut state = ctx.0.write().unwrap_or_else(|e| e.into_inner());
    let slot = state
        .entry(CONTEXT_KEY)
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    let bundles = slot.as_object_mut().unwrap();
    let mut bundle = repair.clone();
    if let Some(active) = crate::engine::active_node_path() {
        bundle.node_path = scoped_repair_path(&repair.node_path, &active);
    }
    let previous = bundles
        .get(&bundle.node_path)
        .and_then(|v| serde_json::from_value::<RepairContext>(v.clone()).ok());
    let mut history = BTreeMap::new();
    for entry in previous
        .iter()
        .flat_map(|p| &p.prior_attempts)
        .chain(&repair.prior_attempts)
    {
        if entry.attempt <= repair.attempt {
            history
                .entry(entry.attempt)
                .and_modify(|old: &mut AttemptSummary| {
                    if old.detail != entry.detail || old.error_signature != entry.error_signature {
                        *old = entry.clone();
                    }
                })
                .or_insert_with(|| entry.clone());
        }
    }
    let skip = history.len().saturating_sub(HISTORY_LIMIT);
    bundle.prior_attempts = history.into_values().skip(skip).collect();
    bundles.insert(
        bundle.node_path.clone(),
        serde_json::to_value(bundle).expect("serializable repair context"),
    );
}

pub fn get_repair_context(ctx: &Context, node_path: &str) -> Option<RepairContext> {
    let state = ctx.0.read().unwrap_or_else(|e| e.into_inner());
    let bundles = state.get(CONTEXT_KEY)?.as_object()?;
    let value = if let Some(active) = crate::engine::active_node_path() {
        bundles.get(&scoped_repair_path(node_path, &active))?
    } else if let Some(value) = bundles.get(node_path) {
        value
    } else {
        // Historical callers inspect a child by local ID after execution.
        // A local ID is sufficient only when exactly one scoped bundle matches.
        let mut matches = bundles
            .iter()
            .filter(|(path, _)| path.rsplit('/').next() == Some(node_path));
        let (_, value) = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        value
    };
    serde_json::from_value(value.clone()).ok()
}

pub fn format_repair_prompt(repair: &RepairContext) -> String {
    let mut prompt = format!(
        "## Repair {}\nAttempt {} failed. Next attempt {} ({} remaining).\n\nFailure: {}\n{}\n",
        repair.node_path,
        repair.attempt,
        repair.next_attempt,
        repair.remaining_attempts,
        repair.error_signature,
        repair.detail
    );
    let previous: Vec<_> = repair
        .prior_attempts
        .iter()
        .filter(|a| a.attempt < repair.attempt)
        .collect();
    if !previous.is_empty() {
        prompt.push_str("\nEarlier failures:\n");
        for a in previous {
            let _ = writeln!(
                prompt,
                "- Attempt {}: {} — {}",
                a.attempt, a.error_signature, a.detail
            );
        }
    }
    if !repair.changed_files.is_empty() {
        let _ = writeln!(
            prompt,
            "\nChanged files: {}",
            repair.changed_files.join(", ")
        );
    }
    if let Some(stat) = &repair.git_diff_stat {
        let _ = writeln!(prompt, "\nDiff stat:\n{stat}");
    }
    if let Some(tail) = &repair.log_tail {
        let _ = writeln!(prompt, "\nLog tail:\n{tail}");
    }
    prompt.push_str("\nStart with this failure. Inspect the affected output, choose a fix that addresses the evidence, and rerun the verifier. Avoid repeating earlier failed approaches.");
    redact_secrets(&prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn redacts_credentials_and_preserves_surrounding_evidence() {
        for secret in [
            "Bearer a.b/c==",
            "sk-proj-wxyz",
            "ghp_abcd",
            "xoxb-123-A",
            "AIzaABcd",
            "password=abc",
            "token=Bearer a.b.c",
            "TOKEN='a b'",
            "\"access_token\": \"abc\"",
        ] {
            assert_eq!(
                redact_secrets(&format!("before {secret} after")),
                "before [redacted] after"
            );
        }
    }

    #[test]
    fn log_reads_have_a_byte_bound_and_redact_before_cutting() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("worker.log");
        let text = format!(
            "Bearer {}\n{}password=hidden\n末尾",
            "Q".repeat(40_000),
            "é\n".repeat(3000)
        );
        fs::write(&path, text).unwrap();
        let repair = build_repair_context(
            "leaf",
            1,
            2,
            "bad",
            "Bearer hidden",
            Some(&path),
            Some(dir.path()),
        );
        let tail = repair.log_tail.unwrap();
        assert!(tail.len() <= LOG_LIMIT && tail.ends_with("[redacted]\n末尾"));
        assert!(!tail.contains('Q') && !tail.contains("hidden"));
        assert_eq!(repair.detail, "[redacted]");
        assert!(repair.git_diff_stat.is_none() && repair.changed_files.is_empty());
        let path = dir.path().join("absent");
        assert!(build_repair_context("n", 1, 1, "s", "d", Some(&path), None)
            .log_tail
            .is_none());
    }

    #[test]
    fn stores_chronological_bounded_history_and_separate_nodes() {
        let ctx = Context::new();
        assert!(get_repair_context(&ctx, "node/x").is_none());
        let other = build_repair_context("node/y", 1, 0, "else", "why", None, None);
        attach_repair_context(&ctx, &other);
        for i in 1..=12 {
            let r = build_repair_context("node/x", i, 12, "same", &format!("step {i}"), None, None);
            attach_repair_context(&ctx, &r);
            attach_repair_context(&ctx, &r);
        }
        let got = get_repair_context(&ctx, "node/x").unwrap();
        assert_eq!(
            (got.attempt, got.next_attempt, got.remaining_attempts),
            (12, 13, 1)
        );
        assert_eq!(
            got.prior_attempts
                .iter()
                .map(|s| s.attempt)
                .collect::<Vec<_>>(),
            (5..=12).collect::<Vec<_>>()
        );
        assert!(got
            .prior_attempts
            .iter()
            .all(|s| chrono::DateTime::parse_from_rfc3339(&s.timestamp).is_ok()));
        assert_eq!(get_repair_context(&ctx, "node/y").unwrap(), other);
        assert_eq!(other.remaining_attempts, 0);
    }

    fn fixture_git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn git_stat_and_nul_delimited_status_in_throwaway_repo() {
        let dir = tempdir().unwrap();
        fixture_git(dir.path(), &["init", "--quiet"]);
        fs::write(dir.path().join("source"), "before\n").unwrap();
        fixture_git(dir.path(), &["add", "source"]);
        fixture_git(dir.path(), &["commit", "--quiet", "-m", "fixture"]);
        fs::write(dir.path().join("source"), "after\n").unwrap();
        fs::write(dir.path().join("new file"), "extra").unwrap();
        let bundle = build_repair_context(
            "root/job",
            1,
            1,
            "bad",
            "wrong value",
            None,
            Some(dir.path()),
        );
        assert!(bundle.git_diff_stat.unwrap().contains("source"));
        assert_eq!(bundle.changed_files, ["source", "new file"]);
    }

    #[test]
    fn fresh_prompt_names_failure_and_earlier_evidence() {
        let ctx = Context::new();
        for (i, detail) in [(1, "expected integer"), (2, "missing score")] {
            let mut r = build_repair_context(
                "root/check",
                i,
                2,
                "schema_violation:/score",
                detail,
                None,
                None,
            );
            r.log_tail = Some("last log line".into());
            r.changed_files.push("result.json".into());
            r.git_diff_stat = Some("1 file changed".into());
            attach_repair_context(&ctx, &r);
        }
        let prompt = format_repair_prompt(&get_repair_context(&ctx, "root/check").unwrap());
        for snippet in [
            "root/check",
            "Next attempt 3 (1 remaining)",
            "schema_violation:/score",
            "missing score",
            "Attempt 1",
            "expected integer",
            "result.json",
            "1 file changed",
            "last log line",
            "rerun the verifier",
            "Avoid repeating",
        ] {
            assert!(prompt.contains(snippet), "{prompt}");
        }
        assert_eq!(prompt.matches("missing score").count(), 1);
    }

    #[test]
    fn compact_feedback_preserves_error_lines_and_tail_within_budget() {
        let mut text = String::from("error[E0308]: mismatched types\n");
        for i in 0..200 {
            text.push_str(&format!("verbose build line {i} with padding padding padding\n"));
        }
        text.push_str("FINAL_TAIL_MARKER: bearer supersecret123\n");
        let compacted = compact_feedback(&text, 400);
        assert!(compacted.len() <= 400, "len={}", compacted.len());
        assert!(compacted.contains("error[E0308]: mismatched types"), "{compacted}");
        assert!(compacted.contains("FINAL_TAIL_MARKER"), "{compacted}");
        assert!(!compacted.contains("supersecret123"), "{compacted}");
    }
}
