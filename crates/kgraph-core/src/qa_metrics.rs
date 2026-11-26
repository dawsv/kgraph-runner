//! Manual re-test checklists, scorecards, and batch history.

use crate::files::{
    file_lock, read_object, read_optional_object, resolve_path, write_json, write_text,
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const MANUAL_QA_VALUES: &[&str] = &["pass", "fail", "blocked", "not_tested", "skipped"];
pub const GAP_TYPES: &[&str] = &[
    "missing_verifier",
    "logic_bug",
    "ui_regression",
    "spec_changed",
    "env_blocker",
    "other",
];
pub const AUTOMATED_STATUSES: &[&str] = &["pass", "fail", "pending", "blocked"];
const FALLBACK_TESTER: &str = "Daniel";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaCase {
    pub id: String,
    pub title: String,
    pub platform: String,
    pub kind: String,
    pub automated_status: String,
    pub ai_attempts: u64,
    pub manual_qa: String,
    pub resolved_on_attempt: Option<u64>,
    pub gap_type: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaChecklist {
    pub version: u32,
    pub batch_id: String,
    pub workflow_type: String,
    pub title: Option<String>,
    pub tester: String,
    pub build_ref: Option<String>,
    pub updated_at: Option<String>,
    pub cases: Vec<QaCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaResultInput {
    pub case_id: String,
    pub manual_qa: String,
    pub resolved_on_attempt: Option<u64>,
    pub gap_type: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaCounts {
    pub total: u64,
    pub tested: u64,
    pub passed: u64,
    pub failed: u64,
    pub blocked: u64,
    pub not_tested: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaRates {
    pub manual_pass_pct: Option<f64>,
    pub one_shot_passed: u64,
    pub one_shot_pct: Option<f64>,
    pub second_attempt_passed: u64,
    pub within_two_attempts_passed: u64,
    pub within_two_attempts_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaPlatformCounts {
    pub total: u64,
    pub tested: u64,
    pub passed: u64,
    pub failed: u64,
    pub blocked: u64,
    pub not_tested: u64,
    pub one_shot_passed: u64,
    pub within_two_attempts_passed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaGapItem {
    pub id: String,
    pub title: String,
    pub platform: String,
    pub gap_type: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaScorecard {
    pub batch_id: String,
    pub workflow_type: String,
    pub title: Option<String>,
    pub tester: String,
    pub build_ref: Option<String>,
    pub updated_at: Option<String>,
    pub cases: Vec<QaCase>,
    pub counts: QaCounts,
    pub rates: QaRates,
    pub auto_pass_manual_fail: Vec<QaGapItem>,
    pub gap_breakdown: BTreeMap<String, u64>,
    pub by_platform: BTreeMap<String, QaPlatformCounts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QaRecentBatch {
    pub batch_id: String,
    pub workflow_type: String,
    pub title: Option<String>,
    pub tester: String,
    pub build_ref: Option<String>,
    pub updated_at: Option<String>,
    pub counts: QaCounts,
    pub rates: QaRates,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QaHistorySummary {
    pub batches: usize,
    pub total_cases: u64,
    pub tested: u64,
    pub passed: u64,
    pub failed: u64,
    pub blocked: u64,
    pub manual_pass_pct: Option<f64>,
    pub one_shot_passed: u64,
    pub one_shot_pct: Option<f64>,
    pub within_two_attempts_passed: u64,
    pub within_two_attempts_pct: Option<f64>,
    pub auto_pass_manual_fail_count: u64,
    pub gap_breakdown: BTreeMap<String, u64>,
    pub recent_batches: Vec<QaRecentBatch>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)
}

// Python str.strip and str.split also treat the ASCII information separators as whitespace.
fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn environment_tester() -> Option<String> {
    env::var("KGRAPH_TESTER")
        .ok()
        .map(|s| s.trim_matches(python_whitespace).to_owned())
        .filter(|s| !s.is_empty())
}

fn configured_tester(explicit: Option<&str>) -> Result<Option<String>, String> {
    match explicit {
        Some(name) => required(Some(name), "tester").map(Some),
        None => Ok(environment_tester()),
    }
}

pub fn default_tester() -> String {
    environment_tester().unwrap_or_else(|| FALLBACK_TESTER.into())
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home(path: &Path) -> PathBuf {
    path.strip_prefix("~")
        .map(|suffix| home_dir().join(suffix))
        .unwrap_or_else(|_| path.to_owned())
}

pub fn default_history_path() -> PathBuf {
    let home = env::var_os("KGRAPH_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".kgraph"));
    let home = expand_home(&home);
    resolve_path(&home).unwrap_or(home).join("qa_history.jsonl")
}

pub fn run_qa_checklist_path(state_path: &Path) -> PathBuf {
    let state = resolve_path(state_path).unwrap_or_else(|_| state_path.to_owned());
    let mut dir = state.file_stem().unwrap_or_default().to_owned();
    dir.push("_qa");
    state.with_file_name(dir).join("manual_qa.json")
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn required(value: Option<&str>, name: &str) -> Result<String, String> {
    value
        .map(|s| s.trim_matches(python_whitespace))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{name} must be non-empty text"))
}

fn optional(value: &Value, name: &str) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|s| Some(s.into()))
        .ok_or_else(|| format!("{name} must be text or null"))
}

fn choice(input: Option<&str>, allowed: &[&str], field: &str) -> Result<String, String> {
    input
        .filter(|s| allowed.contains(s))
        .map(str::to_owned)
        .ok_or_else(|| format!("Invalid {field} {input:?}. Expected one of {allowed:?}"))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(xs) => !xs.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn string_field<'a>(object: &'a Value, key: &str, default: &'a str) -> Option<&'a str> {
    object.get(key).map_or(Some(default), Value::as_str)
}

fn resolved_attempt(manual: &str, attempts: u64, explicit: &Value) -> Result<Option<u64>, String> {
    if manual != "pass" {
        return Ok(None);
    }
    if explicit.is_null() {
        return Ok(Some(attempts.max(1)));
    }
    explicit
        .as_u64()
        .filter(|&n| n > 0)
        .map(Some)
        .ok_or_else(|| "resolved_on_attempt must be an integer of at least 1".into())
}

fn normalize_case(case: &Value) -> Result<QaCase, String> {
    if !case.is_object() {
        return Err("Each QA case must be a JSON object".into());
    }
    let id = required(case["id"].as_str(), "case id")?;
    let text = |key: &str, default: &str| {
        let input = if truthy(&case[key]) {
            case[key].as_str()
        } else {
            Some(default)
        };
        required(input, &format!("case {key}"))
    };
    let automated_status = choice(
        string_field(case, "automated_status", "pending"),
        AUTOMATED_STATUSES,
        "automated_status",
    )?;
    let ai_attempts = match case.get("ai_attempts") {
        None => 0,
        Some(n) => n
            .as_u64()
            .ok_or("ai_attempts must be an integer of at least 0")?,
    };
    let manual_qa = choice(
        string_field(case, "manual_qa", "not_tested"),
        MANUAL_QA_VALUES,
        "manual_qa",
    )?;
    let gap_type = if case["gap_type"].is_null() {
        None
    } else {
        Some(choice(case["gap_type"].as_str(), GAP_TYPES, "gap_type")?)
    };
    Ok(QaCase {
        title: text("title", &id)?,
        platform: text("platform", "unspecified")?,
        kind: text("kind", "custom")?,
        id,
        automated_status,
        ai_attempts,
        resolved_on_attempt: resolved_attempt(
            &manual_qa,
            ai_attempts,
            &case["resolved_on_attempt"],
        )?,
        manual_qa,
        gap_type,
        notes: optional(&case["notes"], "notes")?,
    })
}

fn normalize_cases(raw_cases: &[Value]) -> Result<Vec<QaCase>, String> {
    let mut ids = HashSet::new();
    let mut cases = Vec::with_capacity(raw_cases.len());
    for raw in raw_cases {
        let case = normalize_case(raw)?;
        if !ids.insert(case.id.clone()) {
            return Err(format!("Duplicate QA case ID {:?}", case.id));
        }
        cases.push(case);
    }
    Ok(cases)
}

fn normalize_checklist(data: &Value) -> Result<QaChecklist, String> {
    let valid_version = match data.get("version") {
        None => true,
        Some(v) => v.as_f64() == Some(1.0) || v.as_bool() == Some(true),
    };
    let raw_cases = data["cases"]
        .as_array()
        .filter(|_| valid_version)
        .ok_or("Expected a version 1 QA checklist with a cases list")?;
    Ok(QaChecklist {
        version: 1,
        batch_id: required(data["batch_id"].as_str(), "batch_id")?,
        workflow_type: required(
            string_field(data, "workflow_type", "custom"),
            "workflow_type",
        )?,
        tester: required(string_field(data, "tester", FALLBACK_TESTER), "tester")?,
        title: optional(&data["title"], "title")?,
        build_ref: optional(&data["build_ref"], "build_ref")?,
        updated_at: optional(&data["updated_at"], "updated_at")?,
        cases: normalize_cases(raw_cases)?,
    })
}

fn apply_state(cases: &mut [Value], path: &Path) -> Result<(), String> {
    let state = read_optional_object(path)?;
    let Some(nodes) = state.get("nodes") else {
        return Ok(());
    };
    let nodes = nodes
        .as_object()
        .ok_or("State nodes must be a JSON object")?;
    let mut leaves: HashMap<&str, Option<&Map<String, Value>>> = HashMap::new();
    for (path, value) in nodes {
        if let Some(record) = value.as_object() {
            let leaf = path.rsplit('/').next().unwrap_or(path);
            leaves
                .entry(leaf)
                .and_modify(|unique| *unique = None)
                .or_insert(Some(record));
        }
    }
    for case in cases {
        let reference = ["node_path", "node_id", "id"]
            .iter()
            .map(|key| &case[*key])
            .find(|v| truthy(v))
            .and_then(Value::as_str);
        let Some(reference) = reference else {
            continue;
        };
        let record = nodes
            .get(reference)
            .and_then(Value::as_object)
            .or_else(|| leaves.get(reference).copied().flatten());
        if let Some(record) = record {
            let status = match record.get("status").and_then(Value::as_str).unwrap_or("") {
                "passed" | "cached" => "pass",
                "failed" | "circuit_broken" => "fail",
                "awaiting_subagents" | "budget_exceeded" => "blocked",
                s if AUTOMATED_STATUSES.contains(&s) => s,
                _ => "pending",
            };
            case["automated_status"] = json!(status);
            if let Some(n) = record.get("attempts").and_then(Value::as_u64) {
                case["ai_attempts"] = json!(n);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn init_qa_checklist(
    output_path: &Path,
    batch_id: &str,
    cases: &[Value],
    workflow_type: &str,
    title: Option<&str>,
    tester: Option<&str>,
    build_ref: Option<&str>,
    state_path: Option<&Path>,
    preserve_existing: bool,
) -> Result<QaChecklist, String> {
    let configured = configured_tester(tester)?;
    let mut cases = std::borrow::Cow::Borrowed(cases);
    if let Some(path) = state_path {
        apply_state(cases.to_mut(), path)?;
    }
    let mut current = QaChecklist {
        version: 1,
        batch_id: required(Some(batch_id), "batch_id")?,
        workflow_type: required(Some(workflow_type), "workflow_type")?,
        title: title.map(str::to_owned),
        build_ref: build_ref.map(str::to_owned),
        tester: configured.as_deref().unwrap_or(FALLBACK_TESTER).to_owned(),
        updated_at: None,
        cases: normalize_cases(&cases)?,
    };

    let _lock = file_lock(&lock_path(output_path))?;
    if preserve_existing && output_path.is_file() {
        let previous = normalize_checklist(&Value::Object(read_object(output_path)?))?;
        if current.batch_id == previous.batch_id {
            if configured.is_none() {
                current.tester.clone_from(&previous.tester);
            }
            if current.title.is_none() {
                current.title.clone_from(&previous.title);
            }
            if current.build_ref.is_none() {
                current.build_ref.clone_from(&previous.build_ref);
            }
            if current.build_ref == previous.build_ref {
                let prior: HashMap<_, _> =
                    previous.cases.iter().map(|c| (c.id.as_str(), c)).collect();
                for case in &mut current.cases {
                    if let Some(old) = prior.get(case.id.as_str()) {
                        if case.title == old.title
                            && case.platform == old.platform
                            && case.kind == old.kind
                            && case.ai_attempts == old.ai_attempts
                        {
                            case.manual_qa.clone_from(&old.manual_qa);
                            case.resolved_on_attempt = old.resolved_on_attempt;
                            case.gap_type.clone_from(&old.gap_type);
                            case.notes.clone_from(&old.notes);
                            if tester.is_none()
                                && (case.manual_qa != "not_tested"
                                    || case.gap_type.is_some()
                                    || case.notes.is_some())
                            {
                                current.tester.clone_from(&previous.tester);
                            }
                        }
                    }
                }
            }
            current.updated_at.clone_from(&previous.updated_at);
            if current == previous {
                return Ok(previous);
            }
        }
    }
    current.updated_at = Some(now());
    write_json(output_path, &current, true)?;
    Ok(current)
}

pub fn record_qa_results(
    checklist_path: &Path,
    results: &[QaResultInput],
    tester: Option<&str>,
) -> Result<QaChecklist, String> {
    let tester = tester
        .map(|name| required(Some(name), "tester"))
        .transpose()?;
    let _lock = file_lock(&lock_path(checklist_path))?;
    let raw = Value::Object(read_object(checklist_path)?);
    let mut updated = normalize_checklist(&raw)?;
    let mut cases: HashMap<_, _> = updated
        .cases
        .iter_mut()
        .map(|c| (c.id.clone(), c))
        .collect();
    for result in results {
        let id = required(Some(&result.case_id), "case id")?;
        let case = cases
            .get_mut(&id)
            .ok_or_else(|| format!("Unknown QA case ID {id:?}"))?;
        let manual = choice(Some(&result.manual_qa), MANUAL_QA_VALUES, "manual_qa")?;
        let prior_attempt = if case.manual_qa == "pass" {
            case.resolved_on_attempt
        } else {
            None
        };
        case.resolved_on_attempt = resolved_attempt(
            &manual,
            case.ai_attempts,
            &json!(result.resolved_on_attempt.or(prior_attempt)),
        )?;
        case.manual_qa = manual;
        case.gap_type = result
            .gap_type
            .as_deref()
            .map(|s| choice(Some(s), GAP_TYPES, "gap_type"))
            .transpose()?;
        case.notes.clone_from(&result.notes);
    }
    if let Some(name) = tester {
        updated.tester = name;
    }
    if serde_json::to_value(&updated).map_err(|e| e.to_string())? != raw {
        updated.updated_at = Some(now());
        write_json(checklist_path, &updated, true)?;
    }
    Ok(updated)
}

pub fn record_qa_result(
    checklist_path: &Path,
    result: &QaResultInput,
    tester: Option<&str>,
) -> Result<QaChecklist, String> {
    let tester = configured_tester(tester)?;
    record_qa_results(
        checklist_path,
        std::slice::from_ref(result),
        tester.as_deref(),
    )
}

#[derive(Default)]
struct CaseTally {
    counts: QaCounts,
    first: u64,
    second: u64,
}

impl CaseTally {
    fn add(&mut self, case: &QaCase) {
        self.counts.total += 1;
        match case.manual_qa.as_str() {
            "pass" => {
                self.counts.passed += 1;
                self.counts.tested += 1;
                self.first += u64::from(case.resolved_on_attempt == Some(1));
                self.second += u64::from(case.resolved_on_attempt == Some(2));
            }
            "fail" => {
                self.counts.failed += 1;
                self.counts.tested += 1;
            }
            "blocked" => self.counts.blocked += 1,
            "skipped" => self.counts.skipped += 1,
            _ => self.counts.not_tested += 1,
        }
    }
}

fn pct(n: u64, tested: u64) -> Option<f64> {
    // Formatting rounds the actual binary float to decimal ties-to-even, like Python round(x, 1).
    (tested > 0).then(|| {
        format!("{:.1}", n as f64 / tested as f64 * 100.0)
            .parse()
            .expect("formatted finite percentage")
    })
}

pub fn compute_qa_scorecard(checklist: &QaChecklist) -> Result<QaScorecard, String> {
    compute_qa_scorecard_value(&serde_json::to_value(checklist).map_err(|e| e.to_string())?)
}

pub fn compute_qa_scorecard_value(value: &Value) -> Result<QaScorecard, String> {
    let data = normalize_checklist(value)?;
    let mut total = CaseTally::default();
    let mut platforms: BTreeMap<String, CaseTally> = BTreeMap::new();
    let mut auto_pass_manual_fail = Vec::new();
    let mut gap_breakdown = BTreeMap::new();
    for case in &data.cases {
        total.add(case);
        platforms
            .entry(case.platform.clone())
            .or_default()
            .add(case);
        if matches!(case.manual_qa.as_str(), "fail" | "blocked") {
            *gap_breakdown
                .entry(case.gap_type.as_deref().unwrap_or("unspecified").to_owned())
                .or_default() += 1;
        }
        if case.automated_status == "pass" && case.manual_qa == "fail" {
            auto_pass_manual_fail.push(QaGapItem {
                id: case.id.clone(),
                title: case.title.clone(),
                platform: case.platform.clone(),
                gap_type: case.gap_type.clone(),
                notes: case.notes.clone(),
            });
        }
    }
    let by_platform = platforms
        .into_iter()
        .map(|(platform, tally)| {
            let c = tally.counts;
            (
                platform,
                QaPlatformCounts {
                    total: c.total,
                    tested: c.tested,
                    passed: c.passed,
                    failed: c.failed,
                    blocked: c.blocked,
                    not_tested: c.not_tested,
                    one_shot_passed: tally.first,
                    within_two_attempts_passed: tally.first + tally.second,
                },
            )
        })
        .collect();
    let tested = total.counts.tested;
    let within_two = total.first + total.second;
    Ok(QaScorecard {
        batch_id: data.batch_id,
        workflow_type: data.workflow_type,
        title: data.title,
        tester: data.tester,
        build_ref: data.build_ref,
        updated_at: data.updated_at,
        cases: data.cases,
        rates: QaRates {
            manual_pass_pct: pct(total.counts.passed, tested),
            one_shot_passed: total.first,
            one_shot_pct: pct(total.first, tested),
            second_attempt_passed: total.second,
            within_two_attempts_passed: within_two,
            within_two_attempts_pct: pct(within_two, tested),
        },
        counts: total.counts,
        auto_pass_manual_fail,
        gap_breakdown,
        by_platform,
    })
}

fn cell(value: &str) -> String {
    if value.is_empty() {
        return "—".into();
    }
    let mut escaped = String::new();
    for (i, word) in value
        .split(python_whitespace)
        .filter(|word| !word.is_empty())
        .enumerate()
    {
        if i > 0 {
            escaped.push(' ');
        }
        for c in word.chars() {
            match c {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '\\' | '|' | '\u{60}' | '*' | '[' | ']' | '~' => {
                    escaped.push_str(&format!("&#{};", c as u32))
                }
                _ => escaped.push(c),
            }
        }
    }
    escaped
}

fn row(values: &[&str]) -> String {
    format!(
        "| {} |",
        values
            .iter()
            .map(|v| cell(v))
            .collect::<Vec<_>>()
            .join(" | ")
    )
}

fn header<const N: usize>(lines: &mut Vec<String>, labels: [&str; N]) {
    lines.push(row(&labels));
    lines.push(row(&["---"; N]));
}

pub fn render_qa_scorecard_markdown(scorecard: &QaScorecard) -> String {
    let c = &scorecard.counts;
    let r = &scorecard.rates;
    let mut lines = vec![
        format!(
            "# QA Scorecard — {}",
            cell(
                scorecard
                    .title
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&scorecard.batch_id)
            )
        ),
        String::new(),
        format!(
            "Batch **{}** · {} · Tester **{}**",
            cell(&scorecard.batch_id),
            cell(&scorecard.workflow_type),
            cell(&scorecard.tester)
        ),
    ];
    if let Some(build) = scorecard.build_ref.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("Build **{}**", cell(build)));
    }
    if let Some(updated) = scorecard.updated_at.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("Updated {}", cell(updated)));
    }
    lines.push(String::new());
    header(&mut lines, ["KPI", "Cases", "Rate"]);
    for (label, count, rate) in [
        ("Manual QA Pass %", c.passed, r.manual_pass_pct),
        ("One-Shot %", r.one_shot_passed, r.one_shot_pct),
        (
            "≤2-Shot %",
            r.within_two_attempts_passed,
            r.within_two_attempts_pct,
        ),
    ] {
        lines.push(row(&[
            label,
            &format!("{count} / {}", c.tested),
            &rate.map(|value| format!("{value:.1}%")).unwrap_or_default(),
        ]));
    }
    lines.extend([String::new(),
        format!("Tested **{}/{}** · Failed **{}** · Blocked **{}** · Awaiting re-test **{}** · Skipped **{}**",
            c.tested, c.total, c.failed, c.blocked, c.not_tested, c.skipped),
        String::new(), "Rates use manual passes and failures as the denominator.".into(),
        String::new(), "## Auto-Pass → Manual-Fail Graph Gaps".into(), String::new(),
    ]);
    if scorecard.auto_pass_manual_fail.is_empty() {
        lines.push("None recorded.".into());
    } else {
        header(&mut lines, ["ID", "Case", "Platform", "Gap", "Notes"]);
        for gap in &scorecard.auto_pass_manual_fail {
            lines.push(row(&[
                &gap.id,
                &gap.title,
                &gap.platform,
                gap.gap_type.as_deref().unwrap_or_default(),
                gap.notes.as_deref().unwrap_or_default(),
            ]));
        }
    }
    lines.extend([String::new(), "## By Platform".into(), String::new()]);
    header(
        &mut lines,
        [
            "Platform",
            "Total",
            "Tested",
            "Passed",
            "Failed",
            "Blocked",
            "Awaiting re-test",
            "One-shot",
            "Within two",
        ],
    );
    for (platform, p) in &scorecard.by_platform {
        lines.push(row(&[
            platform,
            &p.total.to_string(),
            &p.tested.to_string(),
            &p.passed.to_string(),
            &p.failed.to_string(),
            &p.blocked.to_string(),
            &p.not_tested.to_string(),
            &p.one_shot_passed.to_string(),
            &p.within_two_attempts_passed.to_string(),
        ]));
    }
    lines.extend([String::new(), "## Cases".into(), String::new()]);
    header(
        &mut lines,
        [
            "ID",
            "Case",
            "Platform",
            "AI attempts",
            "Automated",
            "Manual",
            "Passed on attempt",
            "Gap",
            "Notes",
        ],
    );
    for case in &scorecard.cases {
        lines.push(row(&[
            &case.id,
            &case.title,
            &case.platform,
            &case.ai_attempts.to_string(),
            &case.automated_status,
            &case.manual_qa,
            &case
                .resolved_on_attempt
                .map(|n| n.to_string())
                .unwrap_or_default(),
            case.gap_type.as_deref().unwrap_or_default(),
            case.notes.as_deref().unwrap_or_default(),
        ]));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn history_location(path: Option<&Path>) -> PathBuf {
    path.map(expand_home).unwrap_or_else(default_history_path)
}

// Preserve first insertion order on upsert. Equal timestamps prefer the batch inserted later.
#[derive(Default)]
struct HistoryRows {
    rows: Vec<Value>,
    positions: HashMap<String, usize>,
}

impl HistoryRows {
    fn upsert(&mut self, value: Value) -> Result<(), String> {
        let id = value["batch_id"]
            .as_str()
            .ok_or("History row needs a batch_id")?;
        let position = *self
            .positions
            .entry(id.to_owned())
            .or_insert(self.rows.len());
        if position == self.rows.len() {
            self.rows.push(value);
        } else {
            self.rows[position] = value;
        }
        Ok(())
    }

    fn read(path: &Path) -> Result<Self, String> {
        use std::io::{BufRead, BufReader};
        let mut history = Self::default();
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(history),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
            if !line.trim_matches(python_whitespace).is_empty() {
                let value = serde_json::from_str(&line)
                    .map_err(|e| format!("{} line {}: {e}", path.display(), index + 1))?;
                history.upsert(value)?;
            }
        }
        Ok(history)
    }
}

pub fn write_qa_scorecard(
    checklist_path: &Path,
    json_path: Option<&Path>,
    md_path: Option<&Path>,
    record_history: bool,
    history_path: Option<&Path>,
) -> Result<QaScorecard, String> {
    let json_path = json_path
        .map(Path::to_owned)
        .unwrap_or_else(|| checklist_path.with_file_name("qa_scorecard.json"));
    let md_path = md_path
        .map(Path::to_owned)
        .unwrap_or_else(|| checklist_path.with_file_name("qa_scorecard.md"));
    let history = record_history.then(|| history_location(history_path));
    let mut distinct = HashSet::new();
    for path in [checklist_path, &json_path, &md_path]
        .into_iter()
        .chain(history.as_deref())
    {
        if !distinct.insert(resolve_path(path)?) {
            return Err(
                "The QA checklist, scorecard outputs, and history must be separate files".into(),
            );
        }
    }
    let _lock = file_lock(&lock_path(checklist_path))?;
    let scorecard = compute_qa_scorecard_value(&Value::Object(read_object(checklist_path)?))?;
    write_json(&json_path, &scorecard, true)?;
    write_text(&md_path, &render_qa_scorecard_markdown(&scorecard))?;
    if let Some(path) = history {
        let _history_lock = file_lock(&lock_path(&path))?;
        let mut rows = HistoryRows::read(&path)?;
        rows.upsert(serde_json::to_value(&scorecard).map_err(|e| e.to_string())?)?;
        let mut contents = String::new();
        for row in rows.rows {
            contents.push_str(&serde_json::to_string(&row).map_err(|e| e.to_string())?);
            contents.push('\n');
        }
        write_text(&path, &contents)?;
    }
    Ok(scorecard)
}

pub fn aggregate_qa_history(history_path: Option<&Path>) -> Result<QaHistorySummary, String> {
    #[derive(Deserialize)]
    struct HistoryEntry {
        #[serde(flatten)]
        batch: QaRecentBatch,
        gap_breakdown: BTreeMap<String, u64>,
        auto_pass_manual_fail: Vec<serde::de::IgnoredAny>,
    }

    let history = HistoryRows::read(&history_location(history_path))?;
    let mut summary = QaHistorySummary {
        batches: history.rows.len(),
        ..Default::default()
    };
    for row in history.rows.into_iter().rev() {
        let entry: HistoryEntry =
            serde_json::from_value(row).map_err(|e| format!("Invalid history scorecard: {e}"))?;
        let counts = &entry.batch.counts;
        summary.total_cases += counts.total;
        summary.tested += counts.tested;
        summary.passed += counts.passed;
        summary.failed += counts.failed;
        summary.blocked += counts.blocked;
        summary.one_shot_passed += entry.batch.rates.one_shot_passed;
        summary.within_two_attempts_passed += entry.batch.rates.within_two_attempts_passed;
        summary.auto_pass_manual_fail_count += entry.auto_pass_manual_fail.len() as u64;
        for (gap, n) in entry.gap_breakdown {
            *summary.gap_breakdown.entry(gap).or_default() += n;
        }
        summary.recent_batches.push(entry.batch);
    }
    summary.manual_pass_pct = pct(summary.passed, summary.tested);
    summary.one_shot_pct = pct(summary.one_shot_passed, summary.tested);
    summary.within_two_attempts_pct = pct(summary.within_two_attempts_passed, summary.tested);
    summary.recent_batches.sort_by(|a, b| {
        b.updated_at
            .as_deref()
            .unwrap_or("")
            .cmp(a.updated_at.as_deref().unwrap_or(""))
    });
    summary.recent_batches.truncate(10);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init(path: &Path, batch: &str, cases: &[Value]) -> QaChecklist {
        init_qa_checklist(
            path,
            batch,
            cases,
            "custom",
            None,
            Some("Tester"),
            None,
            None,
            true,
        )
        .unwrap()
    }

    fn result(id: &str, manual: &str) -> QaResultInput {
        QaResultInput {
            case_id: id.into(),
            manual_qa: manual.into(),
            resolved_on_attempt: None,
            gap_type: None,
            notes: None,
        }
    }

    #[test]
    fn exact_state_paths_unique_leaves_and_missing_evidence() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("run.v2.json");
        let checklist = run_qa_checklist_path(&state);
        assert!(checklist.ends_with("run.v2_qa/manual_qa.json"));
        write_json(&state, &json!({"nodes": {
            "batch/a": {"status":"passed", "attempts":2},
            "batch/b": {"status":"circuit_broken", "attempts":3},
            "batch/capture": {"status":"awaiting_subagents", "attempts":0},
            "batch/direct": {"status":"failed", "attempts":4}, "direct":{"status":"cached","attempts":1},
            "other/ambiguous": {"status":"passed"}, "batch/ambiguous":{"status":"failed"},
            "bad": {"status":[], "attempts":true}, "budget":{"status":"budget_exceeded"},
            "empty":false, "other/empty":{"status":"fail"}
        }}), true).unwrap();
        let cases = vec![
            json!({"id":"a"}),
            json!({"id":"b1","node_path":"batch/b","node_id":"a"}),
            json!({"id":"screenshot","node_id":"capture"}),
            json!({"id":"direct"}),
            json!({"id":"ambiguous"}),
            json!({"id":"unknown","automated_status":"blocked","ai_attempts":4}),
            json!({"id":"bad", "ai_attempts":5}),
            json!({"id":"budget"}),
            json!({"id":"empty"}),
        ];
        let qa = init_qa_checklist(
            &checklist,
            "batch",
            &cases,
            "custom",
            None,
            None,
            None,
            Some(&state),
            true,
        )
        .unwrap();
        assert_eq!(qa.version, 1);
        assert!(qa.updated_at.unwrap().ends_with("+00:00"));
        assert_eq!(
            qa.cases
                .iter()
                .map(|c| (c.automated_status.as_str(), c.ai_attempts))
                .collect::<Vec<_>>(),
            [
                ("pass", 2),
                ("fail", 3),
                ("blocked", 0),
                ("pass", 1),
                ("pending", 0),
                ("blocked", 4),
                ("pending", 5),
                ("blocked", 0),
                ("fail", 0)
            ]
        );
        fs::remove_file(&state).unwrap();
        let plain = init_qa_checklist(
            &checklist,
            "batch",
            &[json!({"id":"plain"})],
            "custom",
            None,
            None,
            None,
            Some(&state),
            true,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&plain.cases[0]).unwrap(),
            json!({
                "id":"plain", "title":"plain", "platform":"unspecified", "kind":"custom", "automated_status":"pending",
                "ai_attempts":0, "manual_qa":"not_tested", "resolved_on_attempt":null, "gap_type":null, "notes":null
            })
        );
        write_json(&state, &json!({"nodes":[]}), false).unwrap();
        assert!(init_qa_checklist(
            &checklist,
            "batch",
            &[],
            "custom",
            None,
            None,
            None,
            Some(&state),
            true
        )
        .is_err());
    }

    #[test]
    fn refresh_preserves_only_current_manual_results_and_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        let a = json!({"id":"a","ai_attempts":1});
        let b = json!({"id":"b","ai_attempts":2});
        let cases = [a.clone(), b.clone()];
        let refresh = |batch, cases: &[Value], title, build_ref, preserve_existing| {
            init_qa_checklist(
                &path,
                batch,
                cases,
                "custom",
                title,
                None,
                build_ref,
                None,
                preserve_existing,
            )
            .unwrap()
        };
        refresh("batch", &cases, Some("Régression"), Some("build-a"), true);
        let mut update = result("a", "pass");
        update.notes = Some("Đã kiểm tra".into());
        record_qa_results(&path, &[update, result("b", "pass")], Some("Hà")).unwrap();
        let changed = [
            json!({"id":"a","ai_attempts":1,"automated_status":"pass"}),
            json!({"id":"b","ai_attempts":3}),
        ];
        let first = refresh("batch", &changed, None, None, true);
        assert_eq!(
            (
                &first.tester[..],
                first.title.as_deref(),
                first.build_ref.as_deref()
            ),
            ("Hà", Some("Régression"), Some("build-a"))
        );
        assert_eq!(
            (
                first.cases[0].automated_status.as_str(),
                first.cases[0].resolved_on_attempt,
                first.cases[0].notes.as_deref()
            ),
            ("pass", Some(1), Some("Đã kiểm tra"))
        );
        assert_eq!(first.cases[1].manual_qa, "not_tested");
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first, refresh("batch", &changed, None, None, true));
        assert_eq!(before, fs::metadata(&path).unwrap().modified().unwrap());
        let mut fail = result("a", "fail");
        fail.gap_type = Some("missing_verifier".into());
        fail.resolved_on_attempt = Some(9);
        let update = record_qa_result(&path, &fail, Some("Hà")).unwrap();
        assert_eq!(update.cases[0].resolved_on_attempt, None);
        assert_eq!(
            refresh("batch", &cases, None, None, true).cases[0].manual_qa,
            "fail"
        );
        assert_eq!(
            refresh("batch", &cases, None, None, false).cases[0].manual_qa,
            "not_tested"
        );
        record_qa_result(&path, &result("b", "pass"), Some("Tester")).unwrap();
        assert_eq!(
            refresh("batch", &cases, None, Some("build-b"), true).cases[1].manual_qa,
            "not_tested"
        );
        for field in ["title", "platform", "kind"] {
            refresh("batch", &cases, None, None, false);
            record_qa_results(&path, &[result("a", "pass")], None).unwrap();
            let mut different = a.clone();
            different[field] = json!("new");
            assert_eq!(
                refresh("batch", &[different], None, None, true).cases[0].manual_qa,
                "not_tested"
            );
        }
        refresh("original", &cases, None, None, false);
        record_qa_results(&path, &[result("a", "pass")], None).unwrap();
        assert_eq!(
            refresh("new", &cases, None, None, true).cases[0].manual_qa,
            "not_tested"
        );
    }

    #[test]
    fn pass_note_edits_keep_explicit_attempt_and_fresh_pass_defaults_to_attempts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        init(
            &path,
            "batch",
            &[json!({"id":"a","ai_attempts":4}), json!({"id":"zero"})],
        );
        let mut update = result("a", "pass");
        update.resolved_on_attempt = Some(2);
        record_qa_result(&path, &update, None).unwrap();
        update.resolved_on_attempt = None;
        update.notes = Some("Retest confirmed".into());
        let changed = record_qa_result(&path, &update, None).unwrap();
        assert_eq!(changed.cases[0].resolved_on_attempt, Some(2));
        assert_eq!(changed.cases[0].notes, update.notes);
        assert_eq!(changed, record_qa_result(&path, &update, None).unwrap());
        update.resolved_on_attempt = Some(3);
        assert_eq!(
            record_qa_result(&path, &update, None).unwrap().cases[0].resolved_on_attempt,
            Some(3)
        );
        let fresh = record_qa_results(
            &path,
            &[
                result("a", "fail"),
                result("a", "pass"),
                result("zero", "pass"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            (
                fresh.cases[0].resolved_on_attempt,
                fresh.cases[1].resolved_on_attempt
            ),
            (Some(4), Some(1))
        );
    }

    #[test]
    fn invalid_inputs_leave_the_original_bytes_intact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        init(&path, "batch", &[json!({"id":"stable"})]);
        let before = fs::read(&path).unwrap();
        for invalid in [
            json!([{"id":"a"},{"id":" a "}]),
            json!([{"id":"x","ai_attempts":-1}]),
            json!([{"id":"x","ai_attempts":true}]),
            json!([{"id":"x","automated_status":"passed"}]),
            json!([{"title":"missing ID"}]),
            json!(["not a case"]),
            json!([{"id":"x","manual_qa":"pass","resolved_on_attempt":true}]),
            json!([{"id":"x","notes":[]}]),
            json!([{"id":"x","kind":"  "}]),
        ] {
            assert!(init_qa_checklist(
                &path,
                "batch",
                invalid.as_array().unwrap(),
                "custom",
                None,
                None,
                None,
                None,
                true
            )
            .is_err());
        }
        for invalid in [
            result("bad", "pass"),
            result("stable", "done"),
            QaResultInput {
                gap_type: Some("wrong".into()),
                ..result("stable", "pass")
            },
            QaResultInput {
                resolved_on_attempt: Some(0),
                ..result("stable", "pass")
            },
        ] {
            assert!(
                record_qa_results(&path, &[result("stable", "blocked"), invalid], None).is_err()
            );
        }
        assert!(record_qa_results(&path, &[result("stable", "pass")], Some("  ")).is_err());
        for invalid in [
            json!({"case_id":"stable","manual_qa":"pass","notes":[]}),
            json!({"case_id":"stable","manual_qa":"pass","resolved_on_attempt":true}),
        ] {
            assert!(serde_json::from_value::<QaResultInput>(invalid).is_err());
        }
        for invalid in [
            json!([]),
            json!({"cases":{},"batch_id":"b"}),
            json!({"version":2,"batch_id":"b","cases":[]}),
        ] {
            assert!(compute_qa_scorecard_value(&invalid).is_err());
        }
        assert_eq!(before, fs::read(&path).unwrap());
    }

    #[test]
    fn scorecard_denominators_platforms_and_graph_gaps_match_python() {
        let card = compute_qa_scorecard_value(&json!({"batch_id":"b", "title":"Build QA", "cases":[
            {"id":"a","platform":"Windows","manual_qa":"pass","resolved_on_attempt":1},
            {"id":"b","platform":"Windows","manual_qa":"pass","resolved_on_attempt":2},
            {"id":"c","platform":"macOS","manual_qa":"pass","resolved_on_attempt":3},
            {"id":"d","platform":"macOS","automated_status":"pass","manual_qa":"fail","title":"Góc | view",
             "gap_type":"missing_verifier","notes":"<b>Broken</b>\nOpen menu"},
            {"id":"e","automated_status":"fail","manual_qa":"fail","resolved_on_attempt":1},
            {"id":"f","manual_qa":"blocked","gap_type":"env_blocker"}, {"id":"g"}, {"id":"h","manual_qa":"skipped"}
        ]})).unwrap();
        let v = serde_json::to_value(&card).unwrap();
        assert!(v.get("version").is_none());
        assert_eq!(
            v["counts"],
            json!({"total":8,"tested":5,"passed":3,"failed":2,"blocked":1,"not_tested":1,"skipped":1})
        );
        assert_eq!(
            v["rates"],
            json!({"manual_pass_pct":60.0,"one_shot_passed":1,"one_shot_pct":20.0,"second_attempt_passed":1,
            "within_two_attempts_passed":2,"within_two_attempts_pct":40.0})
        );
        assert!(v["cases"][4]["resolved_on_attempt"].is_null());
        assert_eq!(
            v["auto_pass_manual_fail"],
            json!([{"id":"d","title":"Góc | view","platform":"macOS","gap_type":"missing_verifier","notes":"<b>Broken</b>\nOpen menu"}])
        );
        assert_eq!(
            v["gap_breakdown"],
            json!({"unspecified":1,"env_blocker":1,"missing_verifier":1})
        );
        assert_eq!(
            v["by_platform"]["Windows"],
            json!({"total":2,"tested":2,"passed":2,"failed":0,"blocked":0,
            "not_tested":0,"one_shot_passed":1,"within_two_attempts_passed":2})
        );
        let md = render_qa_scorecard_markdown(&card);
        for fragment in [
            "60.0%",
            "20.0%",
            "40.0%",
            "Auto-Pass → Manual-Fail",
            "Góc &#124; view",
            "&lt;b&gt;Broken&lt;/b&gt; Open menu",
            "By Platform",
            "## Cases",
        ] {
            assert!(md.contains(fragment), "{fragment}");
        }
        assert_eq!(pct(1, 16), Some(6.2));
        assert_eq!(pct(3, 16), Some(18.8));
    }

    #[test]
    fn empty_scorecards_and_history_have_no_percentages() {
        for cases in [
            json!([]),
            json!([{"id":"b","manual_qa":"blocked"},{"id":"p"},{"id":"s","manual_qa":"skipped"}]),
        ] {
            let card = compute_qa_scorecard_value(&json!({"batch_id":"b","cases":cases})).unwrap();
            assert_eq!(
                (
                    card.rates.manual_pass_pct,
                    card.rates.one_shot_pct,
                    card.rates.within_two_attempts_pct
                ),
                (None, None, None)
            );
            let md = render_qa_scorecard_markdown(&card);
            assert!(md.contains("| Manual QA Pass % | 0 / 0 | — |"));
            assert!(md.contains("None recorded."));
        }
        let dir = tempdir().unwrap();
        assert_eq!(
            aggregate_qa_history(Some(&dir.path().join("missing.jsonl"))).unwrap(),
            QaHistorySummary::default()
        );
    }

    #[test]
    fn full_markdown_matches_python_reference() {
        let input: Value = serde_json::from_str(r##"{"batch_id": "B|2", "workflow_type": "QA", "tester": "Hà", "build_ref": "v*4", "updated_at": "2026-11-26", "title": "Escapes < & >", "cases": [{"id": "x", "title": "\\|`*[]~", "platform": "Windows", "manual_qa": "fail", "automated_status": "pass", "notes": "Đã\n  kiểm\ttra <b> &"}, {"id": "a", "platform": "Android", "manual_qa": "pass"}]}"##).unwrap();
        let reference = r##"# QA Scorecard — Escapes &lt; &amp; &gt;

Batch **B&#124;2** · QA · Tester **Hà**
Build **v&#42;4**
Updated 2026-11-26

| KPI | Cases | Rate |
| --- | --- | --- |
| Manual QA Pass % | 1 / 2 | 50.0% |
| One-Shot % | 1 / 2 | 50.0% |
| ≤2-Shot % | 1 / 2 | 50.0% |

Tested **2/2** · Failed **1** · Blocked **0** · Awaiting re-test **0** · Skipped **0**

Rates use manual passes and failures as the denominator.

## Auto-Pass → Manual-Fail Graph Gaps

| ID | Case | Platform | Gap | Notes |
| --- | --- | --- | --- | --- |
| x | &#92;&#124;&#96;&#42;&#91;&#93;&#126; | Windows | — | Đã kiểm tra &lt;b&gt; &amp; |

## By Platform

| Platform | Total | Tested | Passed | Failed | Blocked | Awaiting re-test | One-shot | Within two |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Android | 1 | 1 | 1 | 0 | 0 | 0 | 1 | 1 |
| Windows | 1 | 1 | 0 | 1 | 0 | 0 | 0 | 0 |

## Cases

| ID | Case | Platform | AI attempts | Automated | Manual | Passed on attempt | Gap | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| x | &#92;&#124;&#96;&#42;&#91;&#93;&#126; | Windows | 0 | pass | fail | — | — | Đã kiểm tra &lt;b&gt; &amp; |
| a | a | Android | 0 | pending | pass | 1 | — | — |
"##;
        assert_eq!(
            render_qa_scorecard_markdown(&compute_qa_scorecard_value(&input).unwrap()),
            reference
        );
    }

    #[test]
    fn history_upserts_weighted_rates_and_publishes_custom_outputs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        let history = dir.path().join("history.jsonl");
        init(
            &path,
            "desktop-42",
            &[json!({"id":"a","automated_status":"pass"})],
        );
        let mut update = result("a", "fail");
        update.gap_type = Some("logic_bug".into());
        record_qa_result(&path, &update, None).unwrap();
        write_qa_scorecard(&path, None, None, true, Some(&history)).unwrap();
        assert_eq!(
            aggregate_qa_history(Some(&history))
                .unwrap()
                .auto_pass_manual_fail_count,
            1
        );
        update.manual_qa = "pass".into();
        update.resolved_on_attempt = Some(2);
        record_qa_result(&path, &update, None).unwrap();
        let published = write_qa_scorecard(&path, None, None, true, Some(&history)).unwrap();
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            Value::Object(read_object(&dir.path().join("qa_scorecard.json")).unwrap())
        );
        assert_eq!(
            render_qa_scorecard_markdown(&published),
            fs::read_to_string(dir.path().join("qa_scorecard.md")).unwrap()
        );
        let second = dir.path().join("second.json");
        init(
            &second,
            "other",
            &[
                json!({"id":"x","manual_qa":"pass"}),
                json!({"id":"y","manual_qa":"fail","automated_status":"pass","gap_type":"ui_regression"}),
                json!({"id":"z","manual_qa":"fail"}),
                json!({"id":"blocked","manual_qa":"blocked","gap_type":"env_blocker"}),
            ],
        );
        let custom_json = dir.path().join("publish/result.json");
        let custom_md = dir.path().join("publish/result.md");
        let other = write_qa_scorecard(
            &second,
            Some(&custom_json),
            Some(&custom_md),
            true,
            Some(&history),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&other).unwrap(),
            Value::Object(read_object(&custom_json).unwrap())
        );
        assert_eq!(
            render_qa_scorecard_markdown(&other),
            fs::read_to_string(&custom_md).unwrap()
        );
        assert_eq!(fs::read_to_string(&history).unwrap().lines().count(), 2);
        let summary = aggregate_qa_history(Some(&history)).unwrap();
        assert_eq!(
            (
                summary.batches,
                summary.total_cases,
                summary.tested,
                summary.blocked
            ),
            (2, 5, 4, 1)
        );
        assert_eq!(
            (
                summary.manual_pass_pct,
                summary.one_shot_pct,
                summary.within_two_attempts_pct
            ),
            (Some(50.0), Some(25.0), Some(50.0))
        );
        assert_eq!(summary.auto_pass_manual_fail_count, 1);
        assert_eq!(
            serde_json::to_value(&summary.gap_breakdown).unwrap(),
            json!({"ui_regression":1,"unspecified":1,"env_blocker":1})
        );
        assert_eq!(
            summary
                .recent_batches
                .iter()
                .map(|b| b.batch_id.as_str())
                .collect::<Vec<_>>(),
            ["other", "desktop-42"]
        );
        let recent = serde_json::to_value(&summary.recent_batches[0]).unwrap();
        assert_eq!(recent.as_object().unwrap().len(), 8);
        for field in [
            "batch_id",
            "workflow_type",
            "title",
            "tester",
            "build_ref",
            "updated_at",
            "counts",
            "rates",
        ] {
            assert_eq!(recent[field], serde_json::to_value(&other).unwrap()[field]);
        }
    }

    #[test]
    fn history_deduplicates_without_changing_tie_order_and_limits_recent_batches() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa_history.jsonl");
        let mut rows = vec![];
        for n in 0..12 {
            let updated_at = if n == 0 { "2026-11-27" } else { "2026-11-26" };
            let card = compute_qa_scorecard_value(
                &json!({"batch_id":n.to_string(), "updated_at":updated_at, "cases":[]}),
            )
            .unwrap();
            rows.push(serde_json::to_string(&card).unwrap());
        }
        rows.push(rows[4].clone());
        write_text(&path, &rows.join("\n\n")).unwrap();
        let summary = aggregate_qa_history(Some(&path)).unwrap();
        assert_eq!(summary.batches, 12);
        assert_eq!(
            summary
                .recent_batches
                .iter()
                .map(|b| b.batch_id.as_str())
                .collect::<Vec<_>>(),
            ["0", "11", "10", "9", "8", "7", "6", "5", "4", "3"]
        );
        write_text(&path, "{bad JSON}\n").unwrap();
        assert!(aggregate_qa_history(Some(&path)).is_err());
    }

    #[test]
    fn output_aliases_are_rejected_before_writing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        init(&path, "batch", &[]);
        let before = fs::read(&path).unwrap();
        let equivalent = dir.path().join("future/../qa.json");
        let future = dir.path().join("report");
        for (json_path, md_path, history) in [
            (Some(equivalent.as_path()), None, None),
            (None, Some(path.as_path()), None),
            (Some(future.as_path()), Some(future.as_path()), None),
            (Some(future.as_path()), None, Some(future.as_path())),
        ] {
            assert!(
                write_qa_scorecard(&path, json_path, md_path, history.is_some(), history)
                    .unwrap_err()
                    .contains("separate")
            );
        }
        #[cfg(unix)]
        {
            let link = dir.path().join("alias");
            std::os::unix::fs::symlink("report", &link).unwrap();
            assert!(
                write_qa_scorecard(&path, Some(&future), Some(&link), false, None)
                    .unwrap_err()
                    .contains("separate")
            );
        }
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!future.exists());
    }

    #[test]
    fn parallel_record_and_history_writes_keep_all_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("qa.json");
        let history = dir.path().join("history.jsonl");
        let ids: Vec<_> = (0..8).map(|n| format!("case-{n}")).collect();
        let cases: Vec<_> = ids.iter().map(|id| json!({"id":id})).collect();
        init(&path, "batch", &cases);
        let barrier = std::sync::Barrier::new(ids.len());
        std::thread::scope(|scope| {
            let (barrier, root, path, history) =
                (&barrier, dir.path(), path.as_path(), history.as_path());
            for id in &ids {
                scope.spawn(move || {
                    barrier.wait();
                    record_qa_result(path, &result(id, "pass"), None).unwrap();
                    let other = root.join(id).join("manual_qa.json");
                    init(&other, id, &[json!({"id":id})]);
                    write_qa_scorecard(&other, None, None, true, Some(history)).unwrap();
                });
            }
        });
        let card = compute_qa_scorecard_value(&Value::Object(read_object(&path).unwrap())).unwrap();
        assert_eq!(card.counts.passed, 8);
        assert_eq!(aggregate_qa_history(Some(&history)).unwrap().batches, 8);
    }

    #[test]
    fn environment_defaults_explicit_override_and_preserved_attribution() {
        // Each child has a private environment. The parallel test runner never changes its own.
        const MODE: &str = "KGRAPH_QA_TEST_PHASE";
        const TEST: &str =
            "qa_metrics::tests::environment_defaults_explicit_override_and_preserved_attribution";
        if let Ok(phase) = env::var(MODE) {
            let home = PathBuf::from(env::var_os("KGRAPH_HOME").unwrap());
            let path = home.join("manual_qa.json");
            let cases = [json!({"id":"a"})];
            let refresh = |cases: &[Value], tester| {
                init_qa_checklist(
                    &path, "batch", cases, "custom", None, tester, None, None, true,
                )
                .unwrap()
            };
            match phase.as_str() {
                "fallback" => {
                    assert_eq!(default_tester(), "Daniel");
                    assert_eq!(refresh(&cases, None).tester, "Daniel");
                }
                "configured" => {
                    assert_eq!(default_tester(), "Hà");
                    assert_eq!(refresh(&cases, None).tester, "Hà");
                    assert_eq!(refresh(&cases, Some("Daniel")).tester, "Daniel");
                    assert_eq!(
                        record_qa_result(&path, &result("a", "pass"), None)
                            .unwrap()
                            .tester,
                        "Hà"
                    );
                    let mut edit = result("a", "pass");
                    edit.notes = Some("Đã kiểm tra".into());
                    assert_eq!(
                        record_qa_result(&path, &edit, Some(" Alex "))
                            .unwrap()
                            .tester,
                        "Alex"
                    );
                }
                "preserved" => {
                    let qa = refresh(&cases, None);
                    assert_eq!(qa.tester, "Alex");
                    assert_eq!(qa.cases[0].notes.as_deref(), Some("Đã kiểm tra"));
                    let published = write_qa_scorecard(&path, None, None, true, None).unwrap();
                    assert!(render_qa_scorecard_markdown(&published).contains("Đã kiểm tra"));
                    assert_eq!(
                        default_history_path(),
                        resolve_path(&home).unwrap().join("qa_history.jsonl")
                    );
                    assert_eq!(
                        aggregate_qa_history(None).unwrap().within_two_attempts_pct,
                        Some(100.0)
                    );
                    assert_eq!(refresh(&cases, Some("Explicit")).tester, "Explicit");
                    let changed = refresh(&[json!({"id":"a","ai_attempts":1})], None);
                    assert_eq!(
                        (changed.tester.as_str(), changed.cases[0].manual_qa.as_str()),
                        ("New default", "not_tested")
                    );
                }
                _ => panic!("unknown test phase {phase}"),
            }
            return;
        }
        let dir = tempdir().unwrap();
        for (phase, tester) in [
            ("fallback", None),
            ("configured", Some("  Hà  ")),
            ("preserved", Some("New default")),
        ] {
            let mut child = std::process::Command::new(env::current_exe().unwrap());
            child
                .args(["--exact", TEST])
                .env_remove("KGRAPH_TESTER")
                .env(MODE, phase)
                .env("KGRAPH_HOME", dir.path());
            if let Some(tester) = tester {
                child.env("KGRAPH_TESTER", tester);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{phase}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
}
