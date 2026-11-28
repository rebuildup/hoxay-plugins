//! GitHub forge TUI (`git tui forge [main|open] [--cwd DIR]`).
//!
//! Rust port of `plugins/git/src/tui-forge.ts` +
//! `tui-forge-support.ts` + `tui-forge-views.ts`
//! (contract herdr-plugins.tui/v1, issues #239/#292).
//!
//! Design rules honored (same as the TS surface):
//! - the TUI consumes the plugin's OWN public CLI (`git <args>` — this
//!   same binary) as a subprocess for every read and mutation, so
//!   CLI/MCP/GUI behavior cannot drift; the child env drops
//!   `HERDR_PANE_ID`/`HERDR_PLUGIN_CONTEXT_JSON` so ambient identity
//!   can never leak (tui-v1 §3);
//! - target identity is pinned at launch (origin pane/cwd + repo); the
//!   pinned ORIGIN pane id is passed explicitly to every CLI call —
//!   never `HERDR_PANE_ID` (the UI pane's own id);
//! - non-TTY stdin degrades to a plain-text PR/issue list — never a
//!   fake UI;
//! - mutation gating via `capability`/`mutation_commands` fails closed;
//! - PR mutations re-read `pr reviews` (head.sha) + `pr view`
//!   (updatedAt) immediately before writing and pass the fresh sha as
//!   `--expect-head`; issue mutations re-read `issue view` and compare
//!   `updated_at`; drift asks before applying;
//! - every mutation is confirm-gated with exact target + consequence;
//! - `gh_missing`/`stale_head`/`merge_blocked`/`release_boundary`
//!   surface their real `kind: message`, never fabricated success.
//!
//! Async model: the TS surface uses `ui.prompt/confirm/select` modal
//! dialogs inside async flows. Here each mutation is a `Flow` state
//! machine; compute functions run under a scoped `&mut Flow` borrow and
//! return `FlowAction`s which the app then applies (push modal / spawn
//! CLI task / set status) — keeping borrows simple and the event loop
//! single-threaded.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::{json, Map, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use hoxay_herdr_client::socket;
use hoxay_herdr_client::target::{resolve_target, ResolveTargetOptions};
use hoxay_tui::safety::sanitize_display;

// ===========================================================================
// Constants + small value helpers (port of tui-forge-support.ts accessors)
// ===========================================================================

const PLUGIN_ID: &str = "rebuildup.git";
const PLUGIN_VERSION: &str = "0.7.0";
const CLI_TIMEOUT_MS: u64 = 60_000;
const OP_TIMEOUT_MS: u64 = 90_000;
const JOB_LOG_TIMEOUT_MS: u64 = 120_000;
const CAPABILITY_TIMEOUT_MS: u64 = 30_000;

fn as_records(v: Option<&Value>) -> Vec<&Map<String, Value>> {
    v.and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_object()).collect())
        .unwrap_or_default()
}

fn jstr(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// `fmt()` — "unknown" for null/empty, "yes"/"no" for bools.
fn fmt(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "unknown".to_string(),
        Some(Value::String(s)) if s.is_empty() => "unknown".to_string(),
        Some(Value::Bool(b)) => {
            if *b {
                "yes".to_string()
            } else {
                "no".to_string()
            }
        }
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64()).filter(|n| n.is_finite())
}

fn bool_at(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn bool_map(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn author_of(raw: Option<&Value>) -> String {
    match raw {
        Some(Value::String(s)) => s.clone(),
        Some(v) => fmt(v.get("login").or_else(|| v.get("name"))),
        None => "unknown".to_string(),
    }
}

fn short_sha12(sha: Option<&str>) -> String {
    match sha {
        Some(s) if !s.is_empty() => s.chars().take(12).collect(),
        _ => "unknown".to_string(),
    }
}

fn label_names(raw: Option<&Value>) -> Vec<String> {
    match raw {
        Some(Value::Array(items)) => items
            .iter()
            .map(|l| match l {
                Value::String(s) => s.clone(),
                other => jstr(other.get("name")).unwrap_or_else(|| fmt(Some(other))),
            })
            .filter(|s| !s.is_empty() && s != "unknown")
            .collect(),
        _ => Vec::new(),
    }
}

fn str_list(raw: Option<&Value>) -> Vec<String> {
    raw.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    other => fmt(Some(other)),
                })
                .filter(|s| !s.is_empty() && s != "unknown")
                .collect()
        })
        .unwrap_or_default()
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ===========================================================================
// Payload formatting (port of tui-forge-support.ts formatters)
// ===========================================================================

/// `head.sha` echoed by every PR read surface that fetches refs.
fn head_sha_of(payload: &Value) -> Option<String> {
    jstr(payload.get("head").and_then(|h| h.get("sha")))
}

/// Select-list row for one PR from `pr list` → (label, detail).
fn pr_select_item(pr: &Value) -> (String, String) {
    let number = fmt(pr.get("number"));
    let title = fmt(pr.get("title"));
    let state = fmt(pr.get("state")).to_lowercase();
    let draft = bool_at(pr, "isDraft") || bool_at(pr, "is_draft");
    let mut labels: Vec<String> = Vec::new();
    if state != "unknown" {
        labels.push(state.clone());
    }
    if draft {
        labels.push("draft".to_string());
    }
    let suffix = if labels.is_empty() {
        String::new()
    } else {
        format!(" ({})", labels.join(","))
    };
    let detail = [
        format!("number: {number}"),
        format!("title: {title}"),
        format!("state: {}{}", state, if draft { " (draft)" } else { "" }),
        format!("author: {}", author_of(pr.get("author"))),
        format!(
            "head: {} → base: {}",
            fmt(pr.get("headRefName").or_else(|| pr.get("head_ref_name"))),
            fmt(pr.get("baseRefName").or_else(|| pr.get("base_ref_name")))
        ),
        format!(
            "updated: {}",
            fmt(pr.get("updatedAt").or_else(|| pr.get("updated_at")))
        ),
        format!("url: {}", fmt(pr.get("url"))),
    ]
    .join("\n");
    (format!("#{number} {title}{suffix}"), detail)
}

/// Select-list row for one issue from `issue list` → (label, detail).
fn issue_select_item(issue: &Value) -> (String, String) {
    let number = fmt(issue.get("number"));
    let title = fmt(issue.get("title"));
    let state = fmt(issue.get("state")).to_lowercase();
    let mut labels = label_names(issue.get("labels"));
    labels.truncate(4);
    let label_suffix = if labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", labels.join(", "))
    };
    let state_reason = jstr(
        issue
            .get("state_reason")
            .or_else(|| issue.get("stateReason")),
    );
    let assignees = str_list(issue.get("assignees"));
    let assignee_text = if issue.get("assignees").and_then(|v| v.as_array()).is_some() {
        if assignees.is_empty() {
            "none".to_string()
        } else {
            assignees.join(", ")
        }
    } else {
        "unknown".to_string()
    };
    let detail = [
        format!("number: {number}"),
        format!("title: {title}"),
        format!(
            "state: {}{}",
            state,
            state_reason.map(|r| format!(" ({r})")).unwrap_or_default()
        ),
        format!("author: {}", author_of(issue.get("author"))),
        format!(
            "labels: {}",
            if labels.is_empty() {
                "none".to_string()
            } else {
                labels.join(", ")
            }
        ),
        format!("assignees: {assignee_text}"),
        format!(
            "updated: {}",
            fmt(issue.get("updatedAt").or_else(|| issue.get("updated_at")))
        ),
        format!("url: {}", fmt(issue.get("url"))),
    ]
    .join("\n");
    (format!("#{number} {title} ({state}){label_suffix}"), detail)
}

fn pr_ref(pr: &Value, camel: &str, snake: &str) -> String {
    jstr(pr.get(camel))
        .or_else(|| jstr(pr.get(snake)))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Header + body lines for the PR detail view (`pr view` payload).
fn format_pr_view(pr: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "#{} {}",
        fmt(pr.get("number")),
        fmt(pr.get("title"))
    ));
    let draft = bool_at(pr, "isDraft") || bool_at(pr, "is_draft");
    lines.push(
        [
            format!(
                "state: {}{}",
                fmt(pr.get("state")).to_lowercase(),
                if draft { " (draft)" } else { "" }
            ),
            format!("author: {}", author_of(pr.get("author"))),
            format!(
                "head: {} → base: {}",
                pr_ref(pr, "headRefName", "head_ref_name"),
                pr_ref(pr, "baseRefName", "base_ref_name")
            ),
            format!("mergeable: {}", fmt(pr.get("mergeable")).to_lowercase()),
            format!(
                "mergeState: {}",
                fmt(pr
                    .get("mergeStateStatus")
                    .or_else(|| pr.get("merge_state_status")))
            ),
            format!(
                "reviewDecision: {}",
                fmt(pr
                    .get("reviewDecision")
                    .or_else(|| pr.get("review_decision")))
            ),
            format!(
                "diff: +{}/-{} in {} files",
                fmt(pr.get("additions")),
                fmt(pr.get("deletions")),
                fmt(pr.get("changedFiles").or_else(|| pr.get("changed_files")))
            ),
            format!(
                "created: {} · updated: {}",
                pr_ref(pr, "createdAt", "created_at"),
                pr_ref(pr, "updatedAt", "updated_at")
            ),
            format!("url: {}", fmt(pr.get("url"))),
        ]
        .join("\n"),
    );
    if let Some(checks) = pr
        .get("statusCheckRollup")
        .or_else(|| pr.get("status_check_rollup"))
    {
        if checks.is_array() {
            let list = as_records(Some(checks));
            let failing = list
                .iter()
                .filter(|c| {
                    let bucket = fmt(c
                        .get("bucket")
                        .or_else(|| c.get("conclusion"))
                        .or_else(|| c.get("state")))
                    .to_lowercase();
                    ["fail", "failure", "error", "cancel"]
                        .iter()
                        .any(|k| bucket.contains(k))
                })
                .count();
            lines.push(format!(
                "checks: {} reported{}{}",
                list.len(),
                if failing > 0 {
                    format!(", {failing} failing")
                } else {
                    String::new()
                },
                if bool_at(pr, "checks_truncated") {
                    " (truncated)"
                } else {
                    ""
                }
            ));
        }
    }
    let body = jstr(pr.get("body")).unwrap_or_default();
    if !body.is_empty() {
        lines.push("--- body ---".to_string());
        lines.extend(body.split('\n').map(|s| s.to_string()));
        if bool_at(pr, "body_truncated") {
            lines.push("… (body truncated by contract)".to_string());
        }
    } else {
        lines.push("(no body)".to_string());
    }
    lines
}

/// Format `pr reviews` payload (reviews + review requests).
fn format_pr_reviews_payload(payload: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("pr #{} — reviews", fmt(payload.get("number"))));
    if let Some(head) = payload.get("head") {
        lines.push(format!(
            "head: {} @ {}",
            fmt(head.get("ref")),
            short_sha12(jstr(head.get("sha")).as_deref())
        ));
    }
    if let Some(rr) = payload.get("review_requests") {
        let users = str_list(rr.get("users")).join(", ");
        let teams = str_list(rr.get("teams")).join(", ");
        let mut parts: Vec<String> = Vec::new();
        if !users.is_empty() {
            parts.push(format!("users: {users}"));
        }
        if !teams.is_empty() {
            parts.push(format!("teams: {teams}"));
        }
        lines.push(format!(
            "review requests: {}",
            if parts.is_empty() {
                "none".to_string()
            } else {
                parts.join(" · ")
            }
        ));
    }
    let reviews = as_records(payload.get("reviews"));
    if reviews.is_empty() {
        lines.push("(no submitted reviews)".to_string());
    }
    for r in &reviews {
        lines.push(format!(
            "{} — {} @ {}",
            fmt(r.get("author")),
            fmt(r.get("state")).to_lowercase(),
            fmt(r.get("submitted_at").or_else(|| r.get("submittedAt")))
        ));
        let body = jstr(r.get("body")).unwrap_or_default();
        if !body.is_empty() {
            for l in body.split('\n') {
                lines.push(format!("  {l}"));
            }
            if bool_map(r, "body_truncated") {
                lines.push("  … (truncated)".to_string());
            }
        }
    }
    if bool_at(payload, "reviews_truncated") {
        lines.push("… (reviews truncated — see CLI for full list)".to_string());
    }
    lines
}

/// Format `pr checks` payload.
fn format_pr_checks_payload(payload: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("pr #{} — checks", fmt(payload.get("number"))));
    if let Some(head) = payload.get("head") {
        lines.push(format!(
            "head: {} @ {}",
            fmt(head.get("ref")),
            short_sha12(jstr(head.get("sha")).as_deref())
        ));
    }
    let checks = as_records(payload.get("checks"));
    if checks.is_empty() {
        lines.push("(no checks reported)".to_string());
        return lines;
    }
    for c in &checks {
        let state = [
            fmt(c.get("bucket")),
            fmt(c.get("state")),
            fmt(c.get("conclusion")),
        ]
        .into_iter()
        .filter(|s| s != "unknown" && !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
        lines.push(format!(
            "{} — {}",
            fmt(c.get("name")),
            if state.is_empty() {
                "unknown".to_string()
            } else {
                state
            }
        ));
        let mut detail: Vec<String> = Vec::new();
        if let Some(w) = jstr(c.get("workflow")) {
            detail.push(format!("workflow {w}"));
        }
        if let Some(e) = jstr(c.get("event")) {
            detail.push(format!("event {e}"));
        }
        if !detail.is_empty() {
            lines.push(format!("  {}", detail.join(" · ")));
        }
        if let Some(d) = jstr(c.get("description")) {
            lines.push(format!("  {d}"));
        }
        if let Some(l) = jstr(c.get("link")) {
            lines.push(format!("  {l}"));
        }
    }
    if bool_at(payload, "checks_truncated") {
        lines.push("… (checks truncated)".to_string());
    }
    lines
}

/// Format `pr logs` mode=job payload (bounded log tail).
fn format_pr_job_log_payload(payload: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "job #{} log — {}/{} lines{}",
        fmt(payload.get("job_id")),
        fmt(payload.get("lines_returned")),
        fmt(payload.get("total_lines")),
        if bool_at(payload, "truncated") {
            " (truncated)"
        } else {
            ""
        }
    ));
    let log = jstr(payload.get("log")).unwrap_or_default();
    if log.is_empty() {
        lines.push("(empty log)".to_string());
        return lines;
    }
    lines.push("---".to_string());
    lines.extend(log.split('\n').map(|s| s.to_string()));
    lines
}

/// Header + body lines for the issue detail view (`issue view` payload).
fn format_issue_view(issue: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "#{} {}",
        fmt(issue.get("number")),
        fmt(issue.get("title"))
    ));
    let state = fmt(issue.get("state")).to_lowercase();
    let state_reason = jstr(
        issue
            .get("state_reason")
            .or_else(|| issue.get("stateReason")),
    );
    let labels = label_names(issue.get("labels"));
    let assignees = str_list(issue.get("assignees"));
    lines.push(
        [
            format!(
                "state: {}{}",
                state,
                state_reason.map(|r| format!(" ({r})")).unwrap_or_default()
            ),
            format!("author: {}", author_of(issue.get("author"))),
            format!(
                "labels: {}",
                if labels.is_empty() {
                    "none".to_string()
                } else {
                    labels.join(", ")
                }
            ),
            format!(
                "assignees: {}",
                if assignees.is_empty() {
                    "none".to_string()
                } else {
                    assignees.join(", ")
                }
            ),
            format!("milestone: {}", fmt(issue.get("milestone"))),
            format!(
                "created: {} · updated: {}",
                fmt(issue.get("created_at").or_else(|| issue.get("createdAt"))),
                fmt(issue.get("updated_at").or_else(|| issue.get("updatedAt")))
            ),
            format!("url: {}", fmt(issue.get("url"))),
        ]
        .join("\n"),
    );
    let body = jstr(issue.get("body")).unwrap_or_default();
    if !body.is_empty() {
        lines.push("--- body ---".to_string());
        lines.extend(body.split('\n').map(|s| s.to_string()));
        if bool_at(issue, "body_truncated") {
            lines.push("… (body truncated by contract)".to_string());
        }
    } else {
        lines.push("(no body)".to_string());
    }
    let comments = as_records(issue.get("comments"));
    lines.push(String::new());
    lines.push(format!(
        "comments ({}{}):",
        comments.len(),
        if bool_at(issue, "comments_truncated") {
            "+"
        } else {
            ""
        }
    ));
    if comments.is_empty() {
        lines.push("  (none)".to_string());
    }
    for c in &comments {
        lines.push(format!(
            "  #{} {} @ {}",
            fmt(c.get("id")),
            fmt(c.get("author")),
            fmt(c.get("created_at"))
        ));
        let text = jstr(c.get("body")).unwrap_or_default();
        for l in text.split('\n') {
            lines.push(format!("    {l}"));
        }
        if bool_map(c, "body_truncated") {
            lines.push("    … (truncated)".to_string());
        }
    }
    if bool_at(issue, "comments_truncated") {
        lines.push("  … (truncated — see CLI for full list)".to_string());
    }
    lines
}

// ===========================================================================
// Mutation gating + repo override (support port)
// ===========================================================================

/// Fail-closed gate against the `capability` document.
fn mutation_allowed(capability: Option<&Value>, command: &str) -> (bool, Option<String>) {
    let Some(cap) = capability else {
        return (
            false,
            Some("capability document unavailable — mutations disabled (fail closed)".to_string()),
        );
    };
    if cap
        .get("read_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return (false, Some("plugin reports read_only".to_string()));
    }
    let commands: Vec<String> = cap
        .get("mutation_commands")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !commands.iter().any(|c| c == command) {
        return (
            false,
            Some(format!("\"{command}\" is not in mutation_commands")),
        );
    }
    (true, None)
}

/// `owner/name` override grammar — same rule the CLI enforces.
fn valid_repo_override(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    let mut parts = v.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None) => {
            !a.is_empty()
                && !b.is_empty()
                && !a.contains(char::is_whitespace)
                && !b.contains(char::is_whitespace)
        }
        _ => false,
    }
}

fn validate_repo_override(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return Some("empty repo override — cancelled".to_string());
    }
    if value.starts_with('-') {
        return Some("repo override must not start with '-'".to_string());
    }
    if !valid_repo_override(value) {
        return Some(format!(
            "invalid repo override (expected owner/name): {}",
            serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
        ));
    }
    None
}

// ===========================================================================
// CLI subprocess bridge (port of runGitCli / parseCliEnvelope)
// ===========================================================================

#[derive(Debug)]
struct CliError {
    kind: String,
    message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Child env: full environment minus `HERDR_PANE_ID` and
/// `HERDR_PLUGIN_CONTEXT_JSON` — pane-scoped commands always carry the
/// explicit positional, so ambient identity must not leak through.
fn cli_child_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(k, _)| k != "HERDR_PANE_ID" && k != "HERDR_PLUGIN_CONTEXT_JSON")
        .collect()
}

fn parse_cli_envelope(stdout: &str, stderr: &str, exit_code: i32) -> Result<Value, CliError> {
    let trimmed = stdout.trim();
    if !trimmed.is_empty() && exit_code == 0 {
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
            if parsed.is_object() {
                return Ok(parsed);
            }
        }
    }
    // Fall back to per-line candidates (last first).
    for line in trimmed
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .rev()
    {
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            if parsed.is_object() {
                return Ok(parsed);
            }
        }
    }
    // Failure path: stderr carries {contract, ok:false, error:{...}}.
    let mut kind = "cli_error".to_string();
    let mut message = format!("git cli exited {exit_code} with no JSON output");
    if let Ok(parsed) = serde_json::from_str::<Value>(stderr.trim()) {
        if let Some(err) = parsed.get("error").and_then(|e| e.as_object()) {
            if let Some(m) = jstr(err.get("message")) {
                message = m;
            }
            if let Some(k) = jstr(err.get("kind")) {
                kind = k;
            } else if let Some(c) = jstr(err.get("code")) {
                kind = c;
            }
        }
    } else {
        let tail_lines: Vec<&str> = stderr.trim().lines().collect();
        let tail_str = tail_lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join(" | ");
        if !tail_str.is_empty() {
            let cut = tail_str.chars().take(400).collect::<String>();
            message = format!("{message}: {cut}");
        }
    }
    Err(CliError { kind, message })
}

/// Run `git <args>` (this same binary) and return the parsed JSON
/// payload. No shell, argv only; the child inherits the full env minus
/// the two ambient-identity vars.
fn run_cli(args: &[String], timeout_ms: u64) -> Result<Value, CliError> {
    let exe = std::env::current_exe().map_err(|e| CliError {
        kind: "cli_spawn_error".to_string(),
        message: e.to_string(),
    })?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (k, v) in cli_child_env() {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| CliError {
        kind: "cli_spawn_error".to_string(),
        message: e.to_string(),
    })?;
    // Drain both pipes on reader threads BEFORE waiting: a child that
    // writes more than the pipe buffer (~64 KiB — reachable via `pr
    // logs`, `diff`, `compare`, unbounded `files`) would otherwise
    // deadlock against the try_wait loop below.
    let (tx_out, rx_out) = std::sync::mpsc::channel::<String>();
    if let Some(mut o) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = o.read_to_string(&mut s);
            tx_out.send(s).ok();
        });
    }
    let (tx_err, rx_err) = std::sync::mpsc::channel::<String>();
    if let Some(mut e) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            tx_err.send(s).ok();
        });
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CliError {
                        kind: "cli_timeout".to_string(),
                        message: format!("cli timed out after {timeout_ms}ms"),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return Err(CliError {
                    kind: "cli_spawn_error".to_string(),
                    message: e.to_string(),
                })
            }
        }
    };
    // Child exited → pipes hit EOF → readers have finished (or are about
    // to); recv cannot block indefinitely.
    let stdout = rx_out.recv().unwrap_or_default();
    let stderr = rx_err.recv().unwrap_or_default();
    parse_cli_envelope(&stdout, &stderr, status.code().unwrap_or(-1))
}

// ===========================================================================
// Pinned target identity (port of resolvePinnedTarget / buildPaneOpenParams)
// ===========================================================================

/// Origin context captured at launch. Fields are `None` when unknown —
/// the UI must render that honestly rather than guess "current" focus.
#[derive(Debug, Clone, Default)]
struct ForgePinned {
    workspace_id: Option<String>,
    tab_id: Option<String>,
    /// Origin focused pane at launch — the forge repo resolves from ITS cwd.
    origin_pane_id: Option<String>,
    /// Origin cwd — display hint only; the CLI re-resolves via the pane.
    origin_cwd: Option<String>,
    /// Session name when the socket maps to a named session.
    session: Option<String>,
}

fn parse_plugin_context(raw: Option<&str>) -> Map<String, Value> {
    raw.and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Resolve the pinned launch target. Precedence per field:
/// explicit `HERDR_TUI_*` env pins → `HERDR_PLUGIN_CONTEXT_JSON` origin
/// fields → null. `HERDR_PANE_ID` is deliberately ignored.
fn resolve_pinned_target() -> ForgePinned {
    let context = parse_plugin_context(std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok().as_deref());
    let focused_pane = context
        .get("focused_pane")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let env_str = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let ctx_str = |m: &Map<String, Value>, k: &str| jstr(m.get(k));

    let workspace_id =
        env_str("HERDR_TUI_WORKSPACE_ID").or_else(|| ctx_str(&context, "workspace_id"));
    let tab_id = env_str("HERDR_TUI_TAB_ID").or_else(|| ctx_str(&context, "tab_id"));
    let origin_pane_id = env_str("HERDR_TUI_PANE_ID")
        .or_else(|| ctx_str(&context, "focused_pane_id"))
        .or_else(|| ctx_str(&focused_pane, "pane_id"))
        .or_else(|| ctx_str(&focused_pane, "id"));
    let origin_cwd = env_str("HERDR_TUI_CWD")
        .or_else(|| ctx_str(&focused_pane, "cwd"))
        .or_else(|| ctx_str(&context, "cwd"))
        .or_else(|| ctx_str(&context, "workspace_cwd"));

    let mut session = None;
    if let Ok(resolved) = resolve_target(&ResolveTargetOptions::default()) {
        session = resolved.session;
    }
    ForgePinned {
        workspace_id,
        tab_id,
        origin_pane_id,
        origin_cwd,
        session,
    }
}

/// One-line human description of the pinned target for the status line.
fn format_target_label(target: &ForgePinned) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &target.session {
        parts.push(format!("session {s}"));
    }
    if let Some(w) = &target.workspace_id {
        parts.push(format!("ws {w}"));
    }
    if let Some(p) = &target.origin_pane_id {
        parts.push(format!("pane {p}"));
    }
    if let Some(c) = &target.origin_cwd {
        parts.push(format!("cwd {c}"));
    }
    if parts.is_empty() {
        "no pinned target".to_string()
    } else {
        parts.join(" · ")
    }
}

// ===========================================================================
// `open` action helper — plugin.pane.open via the documented socket API
// ===========================================================================

fn run_open() -> i32 {
    let target = resolve_pinned_target();
    let mut env = Map::new();
    if let Some(w) = &target.workspace_id {
        env.insert("HERDR_TUI_WORKSPACE_ID".into(), json!(w));
    }
    if let Some(t) = &target.tab_id {
        env.insert("HERDR_TUI_TAB_ID".into(), json!(t));
    }
    if let Some(p) = &target.origin_pane_id {
        env.insert("HERDR_TUI_PANE_ID".into(), json!(p));
    }
    if let Some(c) = &target.origin_cwd {
        env.insert("HERDR_TUI_CWD".into(), json!(c));
    }
    let mut params = json!({
        "plugin_id": PLUGIN_ID,
        "entrypoint": "forge",
        "placement": "tab",
        "env": env,
    });
    if let Some(w) = &target.workspace_id {
        params["workspace_id"] = json!(w);
    }
    // plugin.pane.open is a herdr-state mutation: fail closed on implicit
    // default-session fallback.
    let resolved = match resolve_target(&ResolveTargetOptions {
        mutation: true,
        ..Default::default()
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({"ok": false, "error": {"kind": "target_error", "message": e.to_string()}})
            );
            return 1;
        }
    };
    match socket::request(&resolved.socket_path, "plugin.pane.open", params, 10_000) {
        Ok(result) => {
            println!(
                "{}",
                serde_json::json!({"ok": true, "plugin": PLUGIN_ID, "pane": result})
            );
            0
        }
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({"ok": false, "error": {"kind": "pane_open_failed", "message": e.to_string()}})
            );
            1
        }
    }
}

// ===========================================================================
// Screens
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Prs,
    Issues,
    Comments,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadPhase {
    Loading,
    Ready,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadKind {
    PrList,
    IssueList,
    PrStatus,
    PrDetail,
    IssueDetail,
    PrComments,
    PrLogsRuns,
    PrLogsJobs,
}

#[derive(Debug, Clone)]
struct FItem {
    label: String,
    detail: String,
    value: Value,
}

#[derive(Debug, Clone)]
struct FListScreen {
    id: u64,
    kind: ListKind,
    items: Vec<FItem>,
    cursor: usize,
    scroll_top: usize,
    filter_active: bool,
    filter_draft: String,
    committed_filter: String,
    phase: LoadPhase,
    /// (kind, message) for the error phase.
    error: Option<(Option<String>, String)>,
    /// Prs/Issues: state filter; Issues: label filter; Logs: mode + run id.
    state: String,
    label_filter: String,
    log_mode: LogMode,
    current_run_id: Option<String>,
    /// PR number for Comments/Logs lists.
    pr_number: Option<i64>,
    /// Freshness snapshots carried into mutation flows (PR comments view).
    opened_updated_at: Option<String>,
    opened_head_sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogMode {
    Runs,
    Jobs,
}

impl FListScreen {
    fn filtered_indices(&self) -> Vec<usize> {
        let q = if self.filter_active {
            self.filter_draft.as_str()
        } else {
            self.committed_filter.as_str()
        };
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        let terms: Vec<&str> = q.split_whitespace().collect();
        (0..self.items.len())
            .filter(|&i| {
                let item = &self.items[i];
                let hay = format!("{} {}", item.label, item.detail).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .collect()
    }

    fn move_cursor(&mut self, delta: isize) {
        let list = self.filtered_indices();
        if list.is_empty() {
            return;
        }
        let n = list.len();
        self.cursor = ((self.cursor as isize + delta).rem_euclid(n as isize)) as usize;
    }

    fn current(&self) -> Option<&FItem> {
        self.filtered_indices()
            .get(self.cursor)
            .map(|&i| &self.items[i])
    }

    fn set_items(&mut self, items: Vec<FItem>) {
        self.items = items;
        self.cursor = 0;
        self.scroll_top = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPhase {
    Loading,
    Ready,
    Error,
}

#[derive(Debug, Clone)]
struct FDetailScreen {
    id: u64,
    /// ScreenView-ish title (e.g. "pr #12").
    title: String,
    /// Source lines (unwrapped); rendered with word wrap + scroll.
    lines: Vec<String>,
    phase: DetailPhase,
    error: Option<(Option<String>, String)>,
    scroll: usize,
    footer: String,
    /// For PR/issue detail views: opened payload snapshots for freshness.
    opened_updated_at: Option<String>,
    opened_head_sha: Option<String>,
    /// Context for reloads + actions.
    number: i64,
    is_issue: bool,
    /// Which loader refreshes this screen (`None` = payload viewer).
    load_kind: Option<LoadKind>,
    /// Raw payload snapshots for confirm targets / flows.
    pr_snapshot: Option<Value>,
    issue_snapshot: Option<Value>,
}

#[derive(Debug, Clone)]
struct FPromptScreen {
    label: String,
    draft: String,
}

#[derive(Debug, Clone)]
struct FConfirmScreen {
    title: String,
    target: Vec<String>,
    consequence: String,
    confirm_label: String,
    cancel_label: String,
    danger: bool,
}

#[derive(Debug, Clone)]
struct SelectOpt {
    label: String,
    detail: String,
    value: String,
}

#[derive(Debug, Clone)]
struct FSelectScreen {
    title: String,
    items: Vec<SelectOpt>,
    cursor: usize,
}

#[derive(Debug)]
enum FScreen {
    /// Root: pick the forge surface (3 fixed items).
    Home {
        cursor: usize,
    },
    List(FListScreen),
    Detail(FDetailScreen),
    Prompt(FPromptScreen),
    Confirm(FConfirmScreen),
    Select(FSelectScreen),
}

impl FScreen {
    fn id(&self) -> Option<u64> {
        match self {
            FScreen::List(l) => Some(l.id),
            FScreen::Detail(d) => Some(d.id),
            _ => None,
        }
    }
}

// ===========================================================================
// Background tasks
// ===========================================================================

/// What a finished background op feeds back into.
#[derive(Debug)]
enum BgResult {
    /// View load finished for screen `screen_id`.
    Load {
        screen_id: u64,
        kind: LoadKind,
        result: Result<Value, CliError>,
    },
    /// One-shot payload fetch → Detail screen, or head-sha feed.
    Payload {
        tag: PayloadTag,
        result: Result<Value, CliError>,
    },
    /// Flow-owned CLI op (mutations + freshness re-reads).
    FlowCli { result: Result<Value, CliError> },
}

#[derive(Debug, Clone)]
enum PayloadTag {
    /// `pr reviews` pushed as a browsable payload view.
    Reviews(i64),
    /// `pr reviews` consumed only for `head.sha` freshness (no view).
    HeadSha(i64),
    Checks(i64),
    /// `pr logs --job <id>` payload (bounded log tail).
    JobLog(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgKind {
    Load,
    Payload,
    FlowCli,
}

struct BgTask {
    label: String,
    rx: mpsc::Receiver<BgResult>,
}

// ===========================================================================
// Flows — sequential modal/CLI state machines for mutations
// ===========================================================================

#[derive(Debug, Clone)]
struct ConfirmSpec {
    title: String,
    target: Vec<String>,
    consequence: String,
    confirm_label: String,
    cancel_label: String,
    danger: bool,
}

/// One input step inside a flow (prompt text or select choice).
#[derive(Debug, Clone)]
enum InputStep {
    Prompt { key: &'static str, label: String },
    Select { key: &'static str, spec: SelectSpec },
}

#[derive(Debug, Clone)]
struct SelectSpec {
    title: String,
    items: Vec<SelectOpt>,
}

/// Modal result routed back into the active flow.
#[derive(Debug)]
enum ModalResult {
    Text(Option<String>),
    Bool(bool),
    Choice(Option<String>),
    /// Esc on prompt/select — abort the flow.
    Cancel,
}

/// Actions a flow asks the app to perform (applied after the scoped
/// flow borrow ends).
#[derive(Debug)]
enum FlowAction {
    PushPrompt {
        label: String,
        initial: String,
    },
    PushConfirm(ConfirmSpec),
    PushSelect(SelectSpec),
    RunCli {
        label: String,
        args: Vec<String>,
        timeout_ms: u64,
    },
    SetStatus(String),
    /// Re-run the load for the screen that owns this flow.
    ReloadOwner {
        screen_id: u64,
    },
    /// Re-run loads for ancestor list screens (post-create).
    ReloadLists,
    OpenPr(i64),
    OpenIssue(i64),
    ClearFlow,
}

// ---- shared forge args (free fn so flow compute can use a cloned target) ----

#[derive(Debug, Clone)]
struct ForgeTarget {
    pane_id: Option<String>,
    repo_override: Option<String>,
}

/// `pr|issue <action> <pane> [pos...] [--repo o/n] [flags...]`.
fn forge_args(
    forge: &ForgeTarget,
    family: &str,
    action: &str,
    positional: &[String],
    flags: &[String],
) -> Option<Vec<String>> {
    let pane = forge.pane_id.clone()?;
    let mut args = vec![family.to_string(), action.to_string(), pane];
    args.extend(positional.iter().cloned());
    if let Some(repo) = &forge.repo_override {
        args.push("--repo".to_string());
        args.push(repo.clone());
    }
    args.extend(flags.iter().cloned());
    Some(args)
}

// ---- PR mutation spec -------------------------------------------------------

#[derive(Debug, Clone)]
enum PrMutSpec {
    Comment {
        body: String,
    },
    Review {
        event: String,
        body: String,
    },
    Reviewers {
        add: String,
        remove: String,
    },
    Merge {
        mode: String,
    },
    /// Reply to a line comment (review-thread reply).
    Reply {
        comment_id: i64,
        body: String,
        loc: String,
    },
}

impl PrMutSpec {
    fn cli_label(&self) -> String {
        match self {
            PrMutSpec::Comment { .. } => "pr comment".to_string(),
            PrMutSpec::Review { event, .. } => {
                format!("pr review ({})", event.to_lowercase().replace('_', "-"))
            }
            PrMutSpec::Reviewers { .. } => "pr reviewers".to_string(),
            PrMutSpec::Merge { mode } => format!("pr merge ({mode})"),
            PrMutSpec::Reply { .. } => "pr reply".to_string(),
        }
    }

    fn command(&self) -> &'static str {
        match self {
            PrMutSpec::Comment { .. } => "comment",
            PrMutSpec::Review { .. } => "review",
            PrMutSpec::Reviewers { .. } => "reviewers",
            PrMutSpec::Merge { .. } => "merge",
            PrMutSpec::Reply { .. } => "reply",
        }
    }

    /// Extra positionals after the PR number (reply comment id).
    fn extra_positionals(&self) -> Vec<String> {
        match self {
            PrMutSpec::Reply { comment_id, .. } => vec![comment_id.to_string()],
            _ => Vec::new(),
        }
    }

    /// Input steps collected before the confirm dialog (merge mode is
    /// collected by the starter; auto-merge method appended dynamically).
    fn input_steps(&self) -> Vec<InputStep> {
        match self {
            PrMutSpec::Comment { .. } => vec![InputStep::Prompt {
                key: "body",
                label: "PR comment body (required)".to_string(),
            }],
            PrMutSpec::Review { .. } => vec![
                InputStep::Select {
                    key: "event",
                    spec: SelectSpec {
                        title: "Review event".to_string(),
                        items: vec![
                            SelectOpt {
                                label: "Approve".to_string(),
                                detail: "submit an approving review".to_string(),
                                value: "APPROVE".to_string(),
                            },
                            SelectOpt {
                                label: "Request changes".to_string(),
                                detail: "submit a changes-requested review".to_string(),
                                value: "REQUEST_CHANGES".to_string(),
                            },
                            SelectOpt {
                                label: "Comment review".to_string(),
                                detail: "submit a review with only a comment body".to_string(),
                                value: "COMMENT".to_string(),
                            },
                        ],
                    },
                },
                InputStep::Prompt {
                    key: "body",
                    label: "Review body (optional)".to_string(),
                },
            ],
            PrMutSpec::Reviewers { .. } => vec![
                InputStep::Prompt {
                    key: "add",
                    label: "Reviewers to add (comma-separated logins — empty for none)".to_string(),
                },
                InputStep::Prompt {
                    key: "remove",
                    label: "Reviewers to remove (comma-separated logins — empty for none)"
                        .to_string(),
                },
            ],
            PrMutSpec::Merge { .. } => Vec::new(),
            PrMutSpec::Reply { comment_id, .. } => vec![InputStep::Prompt {
                key: "body",
                label: format!("Reply body for comment #{comment_id} (required)"),
            }],
        }
    }

    /// Flag tokens; `--expect-head <sha>` appended last by the caller.
    fn flag_args(&self, inputs: &BTreeMap<String, String>) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        match self {
            PrMutSpec::Comment { body } => {
                args.extend(["--body".to_string(), body.clone()]);
            }
            PrMutSpec::Review { event, body } => {
                let flag = match event.as_str() {
                    "APPROVE" => "--approve",
                    "REQUEST_CHANGES" => "--request-changes",
                    _ => "--comment",
                };
                args.push(flag.to_string());
                if !body.is_empty() {
                    args.extend(["--body".to_string(), body.clone()]);
                }
            }
            PrMutSpec::Reviewers { add, remove } => {
                if !add.is_empty() {
                    args.extend(["--add-reviewer".to_string(), add.clone()]);
                }
                if !remove.is_empty() {
                    args.extend(["--remove-reviewer".to_string(), remove.clone()]);
                }
            }
            PrMutSpec::Merge { mode } => match mode.as_str() {
                "auto" => {
                    if let Some(m) = inputs.get("method") {
                        args.push(format!("--{m}"));
                    }
                    args.push("--auto".to_string());
                }
                "disable-auto" => args.push("--disable-auto".to_string()),
                "default" => {}
                other => args.push(format!("--{other}")),
            },
            PrMutSpec::Reply { body, .. } => {
                args.extend(["--body".to_string(), body.clone()]);
            }
        }
        args
    }
}

/// PR mutation flow state.
#[derive(Debug)]
struct PrMutFlow {
    number: i64,
    inputs: BTreeMap<String, String>,
    /// Remaining input steps; `step_idx` counts collected ones.
    steps: Vec<InputStep>,
    step_idx: usize,
    /// Built once all inputs are in (placeholders resolved).
    spec: Option<PrMutSpec>,
    stage: PrMutStage,
    opened_updated_at: Option<String>,
    opened_head_sha: Option<String>,
    /// Freshness re-read results.
    fresh_reviews: Option<Value>,
    fresh_head_sha: Option<String>,
    fresh_updated_at: Option<String>,
    /// PR meta snapshot for confirm targets.
    pr_title: String,
    pr_state: String,
    pr_is_draft: bool,
    pr_merge_state: String,
    pr_review_decision: String,
    owner_screen_id: Option<u64>,
    /// Stashed confirm spec for the drift-apply dialog.
    pending_confirm: Option<ConfirmSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrMutStage {
    /// Collecting `steps[step_idx]`; awaiting modal.
    Inputs,
    /// Awaiting confirm dialog.
    Confirm,
    /// Running `pr reviews` freshness read.
    RevalReviews,
    /// Running `pr view` freshness read.
    RevalView,
    /// Drift detected — awaiting apply-anyway confirm.
    DriftConfirm,
    /// Running the mutation CLI.
    Mutate,
}

// ---- Issue mutation ----------------------------------------------------------

#[derive(Debug, Clone)]
enum IssueMutSpec {
    Comment {
        body: String,
    },
    Edit {
        flags: Vec<String>,
    },
    Close {
        reason: String,
    },
    /// Reopen = `issue edit --state open`.
    Reopen,
}

impl IssueMutSpec {
    fn action(&self) -> &'static str {
        match self {
            IssueMutSpec::Comment { .. } => "comment",
            IssueMutSpec::Edit { .. } | IssueMutSpec::Reopen => "edit",
            IssueMutSpec::Close { .. } => "close",
        }
    }
    fn cli_label(&self) -> String {
        match self {
            IssueMutSpec::Comment { .. } => "issue comment".to_string(),
            IssueMutSpec::Edit { .. } => "issue edit".to_string(),
            IssueMutSpec::Close { .. } => "issue close".to_string(),
            IssueMutSpec::Reopen => "issue reopen".to_string(),
        }
    }
}

/// Issue mutation flow state.
#[derive(Debug)]
struct IssueMutFlow {
    number: i64,
    inputs: BTreeMap<String, String>,
    steps: Vec<InputStep>,
    step_idx: usize,
    /// `None` until inputs complete (edit builds flags then).
    spec: Option<IssueMutSpec>,
    stage: IssueMutStage,
    opened_updated_at: Option<String>,
    fresh_updated_at: Option<String>,
    /// Current title at flow start (edit "hasChange" check).
    edit_current_title: Option<String>,
    owner_screen_id: Option<u64>,
    /// Stashed confirm spec for the drift-apply dialog.
    pending_confirm: Option<ConfirmSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueMutStage {
    Inputs,
    Confirm,
    /// Running `issue view` freshness read.
    Reval,
    /// Drift detected — awaiting apply-anyway confirm.
    DriftConfirm,
    Mutate,
}

// ---- Create flows ------------------------------------------------------------

#[derive(Debug)]
struct PrCreateFlow {
    inputs: BTreeMap<String, String>,
    steps: Vec<InputStep>,
    step_idx: usize,
    stage: PrCreateStage,
    draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrCreateStage {
    Inputs,
    DraftConfirm,
    CreateConfirm,
    Cli,
}

#[derive(Debug)]
struct IssueCreateFlow {
    inputs: BTreeMap<String, String>,
    steps: Vec<InputStep>,
    step_idx: usize,
    stage: IssueCreateStage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueCreateStage {
    Inputs,
    Confirm,
    Cli,
}

/// The single active flow (mutations are sequential; one modal at a time).
#[derive(Debug)]
enum Flow {
    PrMut(PrMutFlow),
    IssueMut(IssueMutFlow),
    PrCreate(PrCreateFlow),
    IssueCreate(IssueCreateFlow),
}

// ===========================================================================
// Flow compute functions — pure state transitions returning actions
// ===========================================================================

fn pr_create_steps() -> Vec<InputStep> {
    vec![
        InputStep::Prompt {
            key: "title",
            label: "PR title (required)".to_string(),
        },
        InputStep::Prompt {
            key: "body",
            label: "PR body (optional — single line; use CLI --body-file for long text)"
                .to_string(),
        },
        InputStep::Prompt {
            key: "base",
            label: "Base branch (optional — Enter for repo default)".to_string(),
        },
        InputStep::Prompt {
            key: "head",
            label: "Head branch/ref (optional — Enter for current branch)".to_string(),
        },
    ]
}

fn issue_create_steps() -> Vec<InputStep> {
    vec![
        InputStep::Prompt {
            key: "title",
            label: "Issue title (required)".to_string(),
        },
        InputStep::Prompt {
            key: "body",
            label: "Issue body (optional — single line; use CLI --body-file for long text)"
                .to_string(),
        },
        InputStep::Prompt {
            key: "labels",
            label: "Labels (comma-separated — optional)".to_string(),
        },
        InputStep::Prompt {
            key: "assignees",
            label: "Assignees (comma-separated — optional)".to_string(),
        },
    ]
}

fn step_modal(step: &InputStep) -> FlowAction {
    match step {
        InputStep::Prompt { label, .. } => FlowAction::PushPrompt {
            label: label.clone(),
            initial: String::new(),
        },
        InputStep::Select { spec, .. } => FlowAction::PushSelect(SelectSpec {
            title: spec.title.clone(),
            items: spec.items.clone(),
        }),
    }
}

/// Store a modal result into flow inputs; returns the step key consumed
/// or None on cancel/mismatch.
fn consume_input(
    inputs: &mut BTreeMap<String, String>,
    steps: &[InputStep],
    step_idx: &mut usize,
    result: &ModalResult,
) -> Option<String> {
    let step = steps.get(*step_idx)?;
    let (key, value) = match (step, result) {
        (InputStep::Prompt { key, .. }, ModalResult::Text(t)) => (*key, t.clone()?),
        (InputStep::Select { key, .. }, ModalResult::Choice(c)) => (*key, c.clone()?),
        _ => return None,
    };
    inputs.insert(key.to_string(), value);
    *step_idx += 1;
    Some(key.to_string())
}

// ---- PR mutation compute -----------------------------------------------------

/// Resolve placeholder spec fields from collected inputs. Returns
/// actions to emit when input collection completes (confirm push) or an
/// abort status.
fn pr_mut_finish_inputs(
    f: &mut PrMutFlow,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    // Merge: resolve the pending mode placeholder, then maybe append the
    // auto-merge method step.
    if let Some(PrMutSpec::Merge { mode }) = f.spec.clone() {
        if mode == "__pending__" {
            let mode = f.inputs.get("mode").cloned().unwrap_or_default();
            f.spec = Some(PrMutSpec::Merge { mode: mode.clone() });
            if mode == "auto"
                && !f
                    .steps
                    .iter()
                    .any(|s| matches!(s, InputStep::Select { key: "method", .. }))
            {
                f.steps.push(InputStep::Select {
                    key: "method",
                    spec: SelectSpec {
                        title: "Auto-merge method".to_string(),
                        items: vec![
                            SelectOpt {
                                label: "Squash".to_string(),
                                detail: String::new(),
                                value: "squash".to_string(),
                            },
                            SelectOpt {
                                label: "Merge commit".to_string(),
                                detail: String::new(),
                                value: "merge".to_string(),
                            },
                            SelectOpt {
                                label: "Rebase".to_string(),
                                detail: String::new(),
                                value: "rebase".to_string(),
                            },
                        ],
                    },
                });
                return vec![step_modal(f.steps.last().expect("step"))];
            }
            // Non-auto (or method already collected): fall through to confirm.
        } else if mode == "auto"
            && f.inputs.contains_key("method")
            && !f
                .steps
                .iter()
                .any(|s| matches!(s, InputStep::Select { key: "method", .. }))
        {
            // unreachable guard
        }
    }
    // Resolve spec placeholders.
    let spec = match f.spec.clone() {
        Some(PrMutSpec::Review { .. }) => Some(PrMutSpec::Review {
            event: f.inputs.get("event").cloned().unwrap_or_default(),
            body: f.inputs.get("body").cloned().unwrap_or_default(),
        }),
        Some(PrMutSpec::Reviewers { .. }) => {
            let add = f.inputs.get("add").cloned().unwrap_or_default();
            let remove = f.inputs.get("remove").cloned().unwrap_or_default();
            if add.is_empty() && remove.is_empty() {
                return vec![
                    FlowAction::SetStatus("no reviewers given — cancelled".to_string()),
                    FlowAction::ClearFlow,
                ];
            }
            Some(PrMutSpec::Reviewers { add, remove })
        }
        Some(PrMutSpec::Comment { .. }) => Some(PrMutSpec::Comment {
            body: f.inputs.get("body").cloned().unwrap_or_default(),
        }),
        Some(PrMutSpec::Reply {
            comment_id, loc, ..
        }) => Some(PrMutSpec::Reply {
            comment_id,
            body: f.inputs.get("body").cloned().unwrap_or_default(),
            loc,
        }),
        other => other,
    };
    f.spec = spec;
    f.stage = PrMutStage::Confirm;
    let spec = f.spec.clone().expect("spec");
    let confirm = build_pr_mut_confirm(f, &spec, forge, target_label);
    vec![FlowAction::PushConfirm(confirm)]
}

fn pr_mut_advance(f: &mut PrMutFlow, forge: &ForgeTarget, target_label: &str) -> Vec<FlowAction> {
    match f.stage {
        PrMutStage::Inputs => {
            if f.step_idx < f.steps.len() {
                let step = f.steps[f.step_idx].clone();
                vec![step_modal(&step)]
            } else {
                pr_mut_finish_inputs(f, forge, target_label)
            }
        }
        PrMutStage::RevalReviews => {
            match forge_args(forge, "pr", "reviews", &[f.number.to_string()], &[]) {
                Some(args) => vec![FlowAction::RunCli {
                    label: "pr reviews (freshness)".to_string(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![
                    FlowAction::SetStatus("no pane pinned — cannot revalidate PR head".to_string()),
                    FlowAction::ClearFlow,
                ],
            }
        }
        PrMutStage::RevalView => {
            match forge_args(forge, "pr", "view", &[f.number.to_string()], &[]) {
                Some(args) => vec![FlowAction::RunCli {
                    label: "pr view (freshness)".to_string(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![
                    FlowAction::SetStatus("no pane pinned — cannot revalidate PR head".to_string()),
                    FlowAction::ClearFlow,
                ],
            }
        }
        PrMutStage::DriftConfirm => {
            // Confirm spec was stashed on the flow by the drift branch.
            match f.pending_confirm.take() {
                Some(c) => vec![FlowAction::PushConfirm(c)],
                None => vec![FlowAction::ClearFlow],
            }
        }
        PrMutStage::Mutate => {
            let Some(spec) = f.spec.clone() else {
                return vec![FlowAction::ClearFlow];
            };
            let head_sha = f.fresh_head_sha.clone().unwrap_or_default();
            let mut flags = spec.flag_args(&f.inputs);
            flags.push("--expect-head".to_string());
            flags.push(head_sha);
            let mut positional = vec![f.number.to_string()];
            positional.extend(spec.extra_positionals());
            match forge_args(forge, "pr", spec.command(), &positional, &flags) {
                Some(args) => vec![FlowAction::RunCli {
                    label: spec.cli_label(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![FlowAction::ClearFlow],
            }
        }
        _ => Vec::new(),
    }
}

/// Confirm target lines shared by PR mutations.
fn pr_target_lines(f: &PrMutFlow, forge: &ForgeTarget, target_label: &str) -> Vec<String> {
    let repo = forge
        .repo_override
        .clone()
        .unwrap_or_else(|| "(pane repo)".to_string());
    let pane = forge.pane_id.clone().unwrap_or_else(|| "?".into());
    let head_short = f
        .opened_head_sha
        .as_deref()
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".to_string());
    vec![
        format!("pr: #{} {}", f.number, f.pr_title),
        format!("repo: {repo}"),
        format!("pane: {pane}"),
        format!("head: {head_short} (fresh head verified before write)"),
        format!(
            "state: {}{}",
            f.pr_state,
            if f.pr_is_draft { " (draft)" } else { "" }
        ),
        format!("mergeState: {}", f.pr_merge_state),
        format!("target: {target_label}"),
    ]
}

fn build_pr_mut_confirm(
    f: &PrMutFlow,
    spec: &PrMutSpec,
    forge: &ForgeTarget,
    target_label: &str,
) -> ConfirmSpec {
    let mut target = pr_target_lines(f, forge, target_label);
    let (title, consequence, confirm_label, danger) = match spec {
        PrMutSpec::Comment { body } => {
            target.push(format!(
                "body: {}",
                body.chars().take(160).collect::<String>()
            ));
            (
                "Post PR comment?".to_string(),
                "posts a public comment on the pull request".to_string(),
                "Post".to_string(),
                false,
            )
        }
        PrMutSpec::Review { event, body } => {
            target.push(format!("event: {event}"));
            if !body.is_empty() {
                target.push(format!(
                    "body: {}",
                    body.chars().take(160).collect::<String>()
                ));
            }
            let ev = event.to_lowercase().replace('_', " ");
            (
                format!("Submit {ev} review?"),
                "submits a formal review on the pull request (visible to collaborators)"
                    .to_string(),
                "Submit review".to_string(),
                *event == "REQUEST_CHANGES",
            )
        }
        PrMutSpec::Reviewers { add, remove } => {
            target.push(format!(
                "add: {}",
                if add.is_empty() { "none" } else { add.as_str() }
            ));
            target.push(format!(
                "remove: {}",
                if remove.is_empty() {
                    "none"
                } else {
                    remove.as_str()
                }
            ));
            // current reviewers line (from reviews payload when available)
            target.push(format!("current reviewers: {}", f.current_reviewers_text()));
            (
                "Update PR reviewers?".to_string(),
                "requests/dismisses reviewers on the pull request".to_string(),
                "Update reviewers".to_string(),
                false,
            )
        }
        PrMutSpec::Merge { mode } => {
            let irreversible = match mode.as_str() {
                "disable-auto" => {
                    "disarms GitHub auto-merge — the PR will no longer merge automatically"
                }
                "auto" => {
                    "arms GitHub auto-merge — the PR merges automatically once checks/reviews clear"
                }
                _ => "MERGES the pull request immediately — this cannot be undone from the TUI",
            };
            target.push(format!("mode: {mode}"));
            target.push(format!("reviewDecision: {}", f.pr_review_decision));
            target.push(format!("isDraft: {}", f.pr_is_draft));
            (
                match mode.as_str() {
                    "disable-auto" => "Disable auto-merge?".to_string(),
                    "auto" => "Enable auto-merge?".to_string(),
                    _ => "Merge pull request?".to_string(),
                },
                irreversible.to_string(),
                match mode.as_str() {
                    "disable-auto" => "Disable".to_string(),
                    "auto" => "Enable".to_string(),
                    _ => "Merge".to_string(),
                },
                mode != "auto" && mode != "disable-auto",
            )
        }
        PrMutSpec::Reply {
            comment_id,
            body,
            loc,
            ..
        } => {
            target = vec![
                format!("pr: #{}", f.number),
                format!("reply to comment: #{comment_id}"),
                format!("path: {loc}"),
                format!(
                    "repo: {}",
                    forge
                        .repo_override
                        .clone()
                        .unwrap_or_else(|| "(pane repo)".to_string())
                ),
                format!(
                    "pane: {}",
                    forge.pane_id.clone().unwrap_or_else(|| "?".into())
                ),
                format!("target: {target_label}"),
            ];
            target.push(format!(
                "body: {}",
                body.chars().take(160).collect::<String>()
            ));
            (
                "Post review-thread reply?".to_string(),
                "posts a public reply inside the review thread".to_string(),
                "Post reply".to_string(),
                false,
            )
        }
    };
    ConfirmSpec {
        title,
        target,
        consequence,
        confirm_label,
        cancel_label: "Cancel".to_string(),
        danger,
    }
}

impl PrMutFlow {
    fn current_reviewers_text(&self) -> String {
        // Best-effort from the freshness reviews payload.
        let Some(rr) = self
            .fresh_reviews
            .as_ref()
            .and_then(|r| r.get("review_requests"))
        else {
            return "unknown".to_string();
        };
        let mut parts: Vec<String> = Vec::new();
        parts.extend(str_list(rr.get("users")));
        parts.extend(str_list(rr.get("teams")));
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Handle a modal result inside a PR mutation flow.
fn pr_mut_on_modal(
    f: &mut PrMutFlow,
    result: ModalResult,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        PrMutStage::Inputs => match result {
            ModalResult::Cancel => vec![FlowAction::ClearFlow],
            _ => {
                // Validate comment/reply body before storing.
                if let Some(InputStep::Prompt { key, .. }) = f.steps.get(f.step_idx) {
                    if *key == "body" {
                        let is_required = matches!(
                            f.spec,
                            Some(PrMutSpec::Comment { .. } | PrMutSpec::Reply { .. })
                        );
                        if is_required {
                            if let ModalResult::Text(Some(t)) = &result {
                                if t.trim().is_empty() {
                                    return vec![
                                        FlowAction::SetStatus(
                                            "empty comment — cancelled".to_string(),
                                        ),
                                        FlowAction::ClearFlow,
                                    ];
                                }
                            }
                        }
                    }
                }
                consume_input(&mut f.inputs, &f.steps, &mut f.step_idx, &result);
                pr_mut_advance(f, forge, target_label)
            }
        },
        PrMutStage::Confirm => match result {
            ModalResult::Bool(true) => {
                f.stage = PrMutStage::RevalReviews;
                pr_mut_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("cancelled".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        PrMutStage::DriftConfirm => match result {
            ModalResult::Bool(true) => {
                f.stage = PrMutStage::Mutate;
                pr_mut_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("refresh instead — mutation aborted".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

/// Handle a finished flow CLI result inside a PR mutation flow.
fn pr_mut_on_cli(
    f: &mut PrMutFlow,
    result: Result<Value, CliError>,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        PrMutStage::RevalReviews => match result {
            Ok(payload) => {
                f.fresh_reviews = Some(payload.clone());
                f.fresh_head_sha = head_sha_of(&payload);
                if f.fresh_head_sha.is_none() {
                    return vec![
                        FlowAction::SetStatus(
                            "revalidate failed: PR head sha unavailable".to_string(),
                        ),
                        FlowAction::ClearFlow,
                    ];
                }
                f.stage = PrMutStage::RevalView;
                pr_mut_advance(f, forge, target_label)
            }
            Err(err) => vec![
                FlowAction::SetStatus(format!("revalidate failed: {err}")),
                FlowAction::ClearFlow,
            ],
        },
        PrMutStage::RevalView => match result {
            Ok(payload) => {
                let fresh_updated_at = payload
                    .get("pr")
                    .and_then(|p| p.as_object())
                    .and_then(|p| jstr(p.get("updatedAt").or_else(|| p.get("updated_at"))));
                f.fresh_updated_at = fresh_updated_at.clone();
                let opened_sha = f.opened_head_sha.clone();
                let opened_updated = f.opened_updated_at.clone();
                let drifted = (opened_updated.is_some()
                    && fresh_updated_at.is_some()
                    && fresh_updated_at != opened_updated)
                    || (opened_sha.is_some()
                        && opened_sha.as_deref() != f.fresh_head_sha.as_deref());
                if drifted {
                    let head12 = |s: &Option<String>| {
                        s.as_deref()
                            .map(|x| x.chars().take(12).collect::<String>())
                            .unwrap_or_else(|| "unknown".to_string())
                    };
                    let confirm = ConfirmSpec {
                        title: "PR changed since this view opened".to_string(),
                        target: vec![
                            format!("pr: #{}", f.number),
                            format!(
                                "opened updated_at: {}",
                                opened_updated.clone().unwrap_or_else(|| "unknown".into())
                            ),
                            format!(
                                "current updated_at: {}",
                                fresh_updated_at.clone().unwrap_or_else(|| "unknown".into())
                            ),
                            format!("opened head: {}", head12(&opened_sha)),
                            format!("current head: {}", head12(&f.fresh_head_sha)),
                        ],
                        consequence: "the PR moved since this view loaded; applying now binds the mutation to the CURRENT head sha (--expect-head)".to_string(),
                        confirm_label: "Apply to current head".to_string(),
                        cancel_label: "Refresh instead".to_string(),
                        danger: false,
                    };
                    f.pending_confirm = Some(confirm);
                    f.stage = PrMutStage::DriftConfirm;
                    return pr_mut_advance(f, forge, target_label);
                }
                f.stage = PrMutStage::Mutate;
                pr_mut_advance(f, forge, target_label)
            }
            Err(err) => vec![
                FlowAction::SetStatus(format!("revalidate failed: {err}")),
                FlowAction::ClearFlow,
            ],
        },
        PrMutStage::Mutate => match result {
            Ok(_) => {
                let label = f.spec.as_ref().map(|s| s.cli_label()).unwrap_or_default();
                let owner = f.owner_screen_id;
                vec![
                    FlowAction::SetStatus(format!("{label} ok")),
                    FlowAction::ClearFlow,
                ]
                .into_iter()
                .chain(owner.map(|id| FlowAction::ReloadOwner { screen_id: id }))
                .collect()
            }
            Err(err) => vec![
                // Honest kind:message — stale_head/merge_blocked/etc.
                FlowAction::SetStatus(format!("{}: {}", err.kind, err.message)),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

// ---- Issue mutation compute --------------------------------------------------

fn issue_comment_steps() -> Vec<InputStep> {
    vec![InputStep::Prompt {
        key: "body",
        label: "Issue comment body (required)".to_string(),
    }]
}

fn issue_close_steps() -> Vec<InputStep> {
    vec![InputStep::Select {
        key: "reason",
        spec: SelectSpec {
            title: "Close reason".to_string(),
            items: vec![
                SelectOpt {
                    label: "Completed".to_string(),
                    detail: "close as completed".to_string(),
                    value: "completed".to_string(),
                },
                SelectOpt {
                    label: "Not planned".to_string(),
                    detail: "close as not planned".to_string(),
                    value: "not planned".to_string(),
                },
                SelectOpt {
                    label: "No reason".to_string(),
                    detail: "close without a reason".to_string(),
                    value: String::new(),
                },
            ],
        },
    }]
}

fn issue_edit_steps(_current_title: &str, current_state: &str) -> Vec<InputStep> {
    vec![
        InputStep::Prompt {
            key: "title",
            label: "New title (empty keeps current)".to_string(),
        },
        InputStep::Select {
            key: "state",
            spec: SelectSpec {
                title: "State".to_string(),
                items: vec![
                    SelectOpt {
                        label: "Keep state".to_string(),
                        detail: format!("current: {current_state}"),
                        value: "keep".to_string(),
                    },
                    SelectOpt {
                        label: "Open".to_string(),
                        detail: "reopen / mark open".to_string(),
                        value: "open".to_string(),
                    },
                    SelectOpt {
                        label: "Closed".to_string(),
                        detail: "close the issue".to_string(),
                        value: "closed".to_string(),
                    },
                ],
            },
        },
        InputStep::Prompt {
            key: "add_labels",
            label: "Labels to add (comma-separated — empty for none)".to_string(),
        },
        InputStep::Prompt {
            key: "remove_labels",
            label: "Labels to remove (comma-separated — empty for none)".to_string(),
        },
        InputStep::Prompt {
            key: "add_assignees",
            label: "Assignees to add (comma-separated — empty for none)".to_string(),
        },
        InputStep::Prompt {
            key: "remove_assignees",
            label: "Assignees to remove (comma-separated — empty for none)".to_string(),
        },
    ]
    .into_iter()
    .map(|mut s| {
        if let InputStep::Prompt { label, .. } = &mut s {
            if *label == "New title (empty keeps current)" {
                // Prefill handled at push time via `initial`.
            }
        }
        s
    })
    .collect()
}

/// Resolve the issue spec once all inputs are collected.
/// Returns (spec, error-status) — error aborts the flow.
fn resolve_issue_spec(f: &mut IssueMutFlow) -> Result<IssueMutSpec, String> {
    // The spec kind is determined by which starter built the flow; we
    // stored a marker in `spec` already for Comment/Close/Reopen, while
    // Edit leaves `spec = None`. Distinguish via steps content.
    if f.spec.is_some() {
        return Ok(f.spec.clone().expect("spec"));
    }
    // Edit flow: build flags from inputs.
    let title = f.inputs.get("title").cloned().unwrap_or_default();
    let state = f
        .inputs
        .get("state")
        .cloned()
        .unwrap_or_else(|| "keep".into());
    let a_labels = split_csv(f.inputs.get("add_labels").map(|s| s.as_str()).unwrap_or(""));
    let r_labels = split_csv(
        f.inputs
            .get("remove_labels")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );
    let a_assignees = split_csv(
        f.inputs
            .get("add_assignees")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );
    let r_assignees = split_csv(
        f.inputs
            .get("remove_assignees")
            .map(|s| s.as_str())
            .unwrap_or(""),
    );
    let current_title = f.edit_current_title.clone().unwrap_or_default();
    let has_change = (!title.is_empty() && title != current_title)
        || state != "keep"
        || !a_labels.is_empty()
        || !r_labels.is_empty()
        || !a_assignees.is_empty()
        || !r_assignees.is_empty();
    if !has_change {
        return Err("no changes — cancelled".to_string());
    }
    let mut flags: Vec<String> = Vec::new();
    if !title.is_empty() && title != current_title {
        flags.extend(["--title".to_string(), title]);
    }
    if state != "keep" {
        flags.extend(["--state".to_string(), state]);
    }
    for l in &a_labels {
        flags.extend(["--add-label".to_string(), l.clone()]);
    }
    for l in &r_labels {
        flags.extend(["--remove-label".to_string(), l.clone()]);
    }
    for a in &a_assignees {
        flags.extend(["--add-assignee".to_string(), a.clone()]);
    }
    for a in &r_assignees {
        flags.extend(["--remove-assignee".to_string(), a.clone()]);
    }
    Ok(IssueMutSpec::Edit { flags })
}

fn issue_mut_advance(
    f: &mut IssueMutFlow,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        IssueMutStage::Inputs => {
            if f.step_idx < f.steps.len() {
                let step = f.steps[f.step_idx].clone();
                // Prefill the edit title prompt.
                if let InputStep::Prompt { key, .. } = &step {
                    if *key == "title" {
                        if let Some(t) = &f.edit_current_title {
                            return vec![FlowAction::PushPrompt {
                                label: step_label(&step),
                                initial: t.clone(),
                            }];
                        }
                    }
                }
                vec![step_modal(&step)]
            } else {
                // Inputs complete → resolve spec → confirm.
                match resolve_issue_spec(f) {
                    Ok(spec) => {
                        f.spec = Some(spec);
                        f.stage = IssueMutStage::Confirm;
                        let spec = f.spec.clone().expect("spec");
                        vec![FlowAction::PushConfirm(build_issue_mut_confirm(
                            f,
                            &spec,
                            forge,
                            target_label,
                        ))]
                    }
                    Err(msg) => vec![FlowAction::SetStatus(msg), FlowAction::ClearFlow],
                }
            }
        }
        IssueMutStage::Reval => {
            match forge_args(forge, "issue", "view", &[f.number.to_string()], &[]) {
                Some(args) => vec![FlowAction::RunCli {
                    label: "issue view (freshness)".to_string(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![
                    FlowAction::SetStatus("no pane pinned — cannot revalidate issue".to_string()),
                    FlowAction::ClearFlow,
                ],
            }
        }
        IssueMutStage::DriftConfirm => match f.pending_confirm.take() {
            Some(c) => vec![FlowAction::PushConfirm(c)],
            None => vec![FlowAction::ClearFlow],
        },
        IssueMutStage::Mutate => {
            let Some(spec) = f.spec.clone() else {
                return vec![FlowAction::ClearFlow];
            };
            let mut flags: Vec<String> = Vec::new();
            match &spec {
                IssueMutSpec::Comment { body } => {
                    flags.extend(["--body".to_string(), body.clone()]);
                }
                IssueMutSpec::Edit { flags: f2 } => flags = f2.clone(),
                IssueMutSpec::Close { reason } => {
                    if !reason.is_empty() {
                        flags.extend(["--reason".to_string(), reason.clone()]);
                    }
                }
                IssueMutSpec::Reopen => {
                    flags.extend(["--state".to_string(), "open".to_string()]);
                }
            }
            match forge_args(
                forge,
                "issue",
                spec.action(),
                &[f.number.to_string()],
                &flags,
            ) {
                Some(args) => vec![FlowAction::RunCli {
                    label: spec.cli_label(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![FlowAction::ClearFlow],
            }
        }
        _ => Vec::new(),
    }
}

fn step_label(step: &InputStep) -> String {
    match step {
        InputStep::Prompt { label, .. } => label.clone(),
        InputStep::Select { spec, .. } => spec.title.clone(),
    }
}

fn issue_target_lines(f: &IssueMutFlow, forge: &ForgeTarget, target_label: &str) -> Vec<String> {
    let repo = forge
        .repo_override
        .clone()
        .unwrap_or_else(|| "(pane repo)".to_string());
    let pane = forge.pane_id.clone().unwrap_or_else(|| "?".into());
    vec![
        format!("issue: #{}", f.number),
        format!("repo: {repo}"),
        format!("pane: {pane}"),
        format!("target: {target_label}"),
    ]
}

fn build_issue_mut_confirm(
    f: &IssueMutFlow,
    spec: &IssueMutSpec,
    forge: &ForgeTarget,
    target_label: &str,
) -> ConfirmSpec {
    let base = issue_target_lines(f, forge, target_label);
    match spec {
        IssueMutSpec::Comment { body } => {
            let mut t = base;
            t.push(format!(
                "body: {}",
                body.chars().take(160).collect::<String>()
            ));
            ConfirmSpec {
                title: "Post issue comment?".to_string(),
                target: t,
                consequence: "posts a public comment on the issue".to_string(),
                confirm_label: "Post".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: false,
            }
        }
        IssueMutSpec::Edit { flags } => {
            let mut t = base;
            let mut i = 0;
            while i + 1 < flags.len() {
                let flag = &flags[i];
                let val = &flags[i + 1];
                let label = match flag.as_str() {
                    "--title" => "new title",
                    "--state" => "new state",
                    "--add-label" => "add label",
                    "--remove-label" => "remove label",
                    "--add-assignee" => "add assignee",
                    "--remove-assignee" => "remove assignee",
                    _ => flag.as_str(),
                };
                t.push(format!("{label}: {val}"));
                i += 2;
            }
            ConfirmSpec {
                title: "Edit issue?".to_string(),
                target: t,
                consequence: "updates the issue on the forge (title/state/labels/assignees)"
                    .to_string(),
                confirm_label: "Apply edit".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: false,
            }
        }
        IssueMutSpec::Close { reason } => {
            let mut t = base;
            if !reason.is_empty() {
                t.push(format!("reason: {reason}"));
            }
            ConfirmSpec {
                title: "Close issue?".to_string(),
                target: t,
                consequence: "closes the issue on the forge".to_string(),
                confirm_label: "Close".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: false,
            }
        }
        IssueMutSpec::Reopen => ConfirmSpec {
            title: "Reopen issue?".to_string(),
            target: base,
            consequence: "reopens the issue on the forge".to_string(),
            confirm_label: "Reopen".to_string(),
            cancel_label: "Cancel".to_string(),
            danger: false,
        },
    }
}

fn issue_mut_on_modal(
    f: &mut IssueMutFlow,
    result: ModalResult,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        IssueMutStage::Inputs => match result {
            ModalResult::Cancel => vec![FlowAction::ClearFlow],
            _ => {
                // Comment body required.
                if f.spec
                    .as_ref()
                    .map(|s| matches!(s, IssueMutSpec::Comment { .. }))
                    .unwrap_or(false)
                {
                    if let ModalResult::Text(Some(t)) = &result {
                        if t.trim().is_empty() {
                            return vec![
                                FlowAction::SetStatus("empty comment — cancelled".to_string()),
                                FlowAction::ClearFlow,
                            ];
                        }
                    }
                }
                consume_input(&mut f.inputs, &f.steps, &mut f.step_idx, &result);
                issue_mut_advance(f, forge, target_label)
            }
        },
        IssueMutStage::Confirm => match result {
            ModalResult::Bool(true) => {
                f.stage = IssueMutStage::Reval;
                issue_mut_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("cancelled".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        IssueMutStage::DriftConfirm => match result {
            ModalResult::Bool(true) => {
                f.stage = IssueMutStage::Mutate;
                issue_mut_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("refresh instead — mutation aborted".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

fn issue_mut_on_cli(
    f: &mut IssueMutFlow,
    result: Result<Value, CliError>,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        IssueMutStage::Reval => match result {
            Ok(payload) => {
                let issue = payload.get("issue").and_then(|i| i.as_object());
                let Some(issue) = issue else {
                    return vec![
                        FlowAction::SetStatus(
                            "revalidate failed: issue view returned no payload".to_string(),
                        ),
                        FlowAction::ClearFlow,
                    ];
                };
                let issue_v = Value::Object(issue.clone());
                let fresh_updated_at = jstr(
                    issue_v
                        .get("updated_at")
                        .or_else(|| issue_v.get("updatedAt")),
                );
                f.fresh_updated_at = fresh_updated_at.clone();
                let drifted = f.opened_updated_at.is_some()
                    && fresh_updated_at.is_some()
                    && fresh_updated_at != f.opened_updated_at;
                if drifted {
                    let confirm = ConfirmSpec {
                        title: "Issue changed since this view opened".to_string(),
                        target: vec![
                            format!("issue: #{}", f.number),
                            format!(
                                "opened updated_at: {}",
                                f.opened_updated_at
                                    .clone()
                                    .unwrap_or_else(|| "unknown".into())
                            ),
                            format!(
                                "current updated_at: {}",
                                fresh_updated_at.unwrap_or_else(|| "unknown".into())
                            ),
                        ],
                        consequence: "the issue changed underneath this view; applying now writes to the latest state".to_string(),
                        confirm_label: "Apply anyway".to_string(),
                        cancel_label: "Refresh instead".to_string(),
                        danger: false,
                    };
                    f.pending_confirm = Some(confirm);
                    f.stage = IssueMutStage::DriftConfirm;
                    return issue_mut_advance(f, forge, target_label);
                }
                f.stage = IssueMutStage::Mutate;
                issue_mut_advance(f, forge, target_label)
            }
            Err(err) => vec![
                FlowAction::SetStatus(format!("revalidate failed: {err}")),
                FlowAction::ClearFlow,
            ],
        },
        IssueMutStage::Mutate => match result {
            Ok(_) => {
                let status = match &f.spec {
                    Some(IssueMutSpec::Comment { .. }) => "comment posted".to_string(),
                    Some(IssueMutSpec::Reopen) => "issue reopened".to_string(),
                    Some(IssueMutSpec::Close { .. }) => "issue closed".to_string(),
                    _ => format!(
                        "{} ok",
                        f.spec.as_ref().map(|s| s.cli_label()).unwrap_or_default()
                    ),
                };
                let owner = f.owner_screen_id;
                vec![FlowAction::SetStatus(status), FlowAction::ClearFlow]
                    .into_iter()
                    .chain(owner.map(|id| FlowAction::ReloadOwner { screen_id: id }))
                    .collect()
            }
            Err(err) => vec![
                FlowAction::SetStatus(format!("{}: {}", err.kind, err.message)),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

// ---- Create flows compute ----------------------------------------------------

fn pr_create_on_modal(
    f: &mut PrCreateFlow,
    result: ModalResult,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        PrCreateStage::Inputs => match result {
            ModalResult::Cancel => vec![FlowAction::ClearFlow],
            ModalResult::Text(Some(text)) => {
                let Some(InputStep::Prompt { key, .. }) = f.steps.get(f.step_idx) else {
                    return vec![FlowAction::ClearFlow];
                };
                let key = *key;
                if key == "title" && text.trim().is_empty() {
                    return vec![
                        FlowAction::SetStatus("empty title — cancelled".to_string()),
                        FlowAction::ClearFlow,
                    ];
                }
                f.inputs.insert(key.to_string(), text);
                f.step_idx += 1;
                pr_create_advance(f, forge, target_label)
            }
            _ => vec![FlowAction::ClearFlow],
        },
        // Draft confirm: Bool(true)=draft, Bool(false)="Ready" (not draft,
        // continue) — faithful to ui.confirm semantics where the cancel
        // label resolves false but the flow continues.
        PrCreateStage::DraftConfirm => match result {
            ModalResult::Bool(draft) => {
                f.draft = draft;
                f.stage = PrCreateStage::CreateConfirm;
                pr_create_advance(f, forge, target_label)
            }
            ModalResult::Cancel => vec![
                FlowAction::SetStatus("cancelled".to_string()),
                FlowAction::ClearFlow,
            ],
            _ => Vec::new(),
        },
        PrCreateStage::CreateConfirm => match result {
            ModalResult::Bool(true) => {
                f.stage = PrCreateStage::Cli;
                pr_create_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("cancelled".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

fn pr_create_advance(
    f: &mut PrCreateFlow,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        PrCreateStage::Inputs => {
            if f.step_idx < f.steps.len() {
                let step = f.steps[f.step_idx].clone();
                vec![step_modal(&step)]
            } else {
                f.stage = PrCreateStage::DraftConfirm;
                vec![FlowAction::PushConfirm(ConfirmSpec {
                    title: "Create as draft?".to_string(),
                    target: vec![
                        format!(
                            "repo: {}",
                            forge
                                .repo_override
                                .clone()
                                .unwrap_or_else(|| "(pane repo)".to_string())
                        ),
                        format!(
                            "pane: {}",
                            forge.pane_id.clone().unwrap_or_else(|| "?".into())
                        ),
                    ],
                    consequence: "a draft PR is created on the forge (not marked ready for review)"
                        .to_string(),
                    confirm_label: "Draft".to_string(),
                    cancel_label: "Ready".to_string(),
                    danger: false,
                })]
            }
        }
        PrCreateStage::CreateConfirm => {
            let title = f.inputs.get("title").cloned().unwrap_or_default();
            let body = f.inputs.get("body").cloned().unwrap_or_default();
            let base = f.inputs.get("base").cloned().unwrap_or_default();
            let head = f.inputs.get("head").cloned().unwrap_or_default();
            let mut target = vec![format!("title: {title}")];
            if !body.trim().is_empty() {
                target.push(format!(
                    "body: {}",
                    body.trim().chars().take(120).collect::<String>()
                ));
            }
            if !base.trim().is_empty() {
                target.push(format!("base: {}", base.trim()));
            }
            if !head.trim().is_empty() {
                target.push(format!("head: {}", head.trim()));
            }
            target.push(format!("draft: {}", f.draft));
            target.push(format!(
                "repo: {}",
                forge
                    .repo_override
                    .clone()
                    .unwrap_or_else(|| "(pane repo)".to_string())
            ));
            target.push(format!(
                "pane: {}",
                forge.pane_id.clone().unwrap_or_else(|| "?".into())
            ));
            target.push(format!("target: {target_label}"));
            vec![FlowAction::PushConfirm(ConfirmSpec {
                title: "Create pull request?".to_string(),
                target,
                consequence: "creates a pull request on the forge (visible to collaborators)"
                    .to_string(),
                confirm_label: "Create PR".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: false,
            })]
        }
        PrCreateStage::Cli => {
            let title = f.inputs.get("title").cloned().unwrap_or_default();
            let body = f.inputs.get("body").cloned().unwrap_or_default();
            let base = f.inputs.get("base").cloned().unwrap_or_default();
            let head = f.inputs.get("head").cloned().unwrap_or_default();
            let draft = f.draft;
            let mut flags = vec!["--title".to_string(), title];
            if !body.trim().is_empty() {
                flags.extend(["--body".to_string(), body.trim().to_string()]);
            }
            if !base.trim().is_empty() {
                flags.extend(["--base".to_string(), base.trim().to_string()]);
            }
            if !head.trim().is_empty() {
                flags.extend(["--head".to_string(), head.trim().to_string()]);
            }
            if draft {
                flags.push("--draft".to_string());
            }
            match forge_args(forge, "pr", "create", &[], &flags) {
                Some(args) => vec![FlowAction::RunCli {
                    label: "pr create".to_string(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![FlowAction::ClearFlow],
            }
        }
        _ => Vec::new(),
    }
}

fn pr_create_on_cli(f: &mut PrCreateFlow, result: Result<Value, CliError>) -> Vec<FlowAction> {
    match result {
        Ok(payload) => {
            let url = jstr(payload.get("url"));
            let number = num(payload.get("number")).map(|n| n as i64);
            let _ = f;
            vec![
                FlowAction::SetStatus(format!(
                    "PR created{}{}",
                    number.map(|n| format!(" #{n}")).unwrap_or_default(),
                    url.map(|u| format!(" — {u}")).unwrap_or_default()
                )),
                FlowAction::ClearFlow,
                FlowAction::ReloadLists,
            ]
            .into_iter()
            .chain(number.map(FlowAction::OpenPr))
            .collect()
        }
        Err(err) => vec![
            FlowAction::SetStatus(format!("{}: {}", err.kind, err.message)),
            FlowAction::ClearFlow,
        ],
    }
}

fn issue_create_on_modal(
    f: &mut IssueCreateFlow,
    result: ModalResult,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        IssueCreateStage::Inputs => match result {
            ModalResult::Cancel => vec![FlowAction::ClearFlow],
            ModalResult::Text(Some(text)) => {
                let Some(InputStep::Prompt { key, .. }) = f.steps.get(f.step_idx) else {
                    return vec![FlowAction::ClearFlow];
                };
                let key = *key;
                if key == "title" && text.trim().is_empty() {
                    return vec![
                        FlowAction::SetStatus("empty title — cancelled".to_string()),
                        FlowAction::ClearFlow,
                    ];
                }
                f.inputs.insert(key.to_string(), text);
                f.step_idx += 1;
                issue_create_advance(f, forge, target_label)
            }
            _ => vec![FlowAction::ClearFlow],
        },
        IssueCreateStage::Confirm => match result {
            ModalResult::Bool(true) => {
                f.stage = IssueCreateStage::Cli;
                issue_create_advance(f, forge, target_label)
            }
            _ => vec![
                FlowAction::SetStatus("cancelled".to_string()),
                FlowAction::ClearFlow,
            ],
        },
        _ => Vec::new(),
    }
}

fn issue_create_advance(
    f: &mut IssueCreateFlow,
    forge: &ForgeTarget,
    target_label: &str,
) -> Vec<FlowAction> {
    match f.stage {
        IssueCreateStage::Inputs => {
            if f.step_idx < f.steps.len() {
                let step = f.steps[f.step_idx].clone();
                vec![step_modal(&step)]
            } else {
                f.stage = IssueCreateStage::Confirm;
                let title = f.inputs.get("title").cloned().unwrap_or_default();
                let body = f.inputs.get("body").cloned().unwrap_or_default();
                let labels = split_csv(f.inputs.get("labels").map(|s| s.as_str()).unwrap_or(""));
                let assignees =
                    split_csv(f.inputs.get("assignees").map(|s| s.as_str()).unwrap_or(""));
                let mut target = vec![format!("title: {title}")];
                if !body.trim().is_empty() {
                    target.push(format!(
                        "body: {}",
                        body.trim().chars().take(120).collect::<String>()
                    ));
                }
                if !labels.is_empty() {
                    target.push(format!("labels: {}", labels.join(", ")));
                }
                if !assignees.is_empty() {
                    target.push(format!("assignees: {}", assignees.join(", ")));
                }
                target.push(format!(
                    "repo: {}",
                    forge
                        .repo_override
                        .clone()
                        .unwrap_or_else(|| "(pane repo)".to_string())
                ));
                target.push(format!(
                    "pane: {}",
                    forge.pane_id.clone().unwrap_or_else(|| "?".into())
                ));
                target.push(format!("target: {target_label}"));
                vec![FlowAction::PushConfirm(ConfirmSpec {
                    title: "Create issue?".to_string(),
                    target,
                    consequence: "creates an issue on the forge (visible to collaborators)"
                        .to_string(),
                    confirm_label: "Create issue".to_string(),
                    cancel_label: "Cancel".to_string(),
                    danger: false,
                })]
            }
        }
        IssueCreateStage::Cli => {
            let title = f.inputs.get("title").cloned().unwrap_or_default();
            let body = f.inputs.get("body").cloned().unwrap_or_default();
            let labels = split_csv(f.inputs.get("labels").map(|s| s.as_str()).unwrap_or(""));
            let assignees = split_csv(f.inputs.get("assignees").map(|s| s.as_str()).unwrap_or(""));
            let mut flags = vec!["--title".to_string(), title];
            if !body.trim().is_empty() {
                flags.extend(["--body".to_string(), body.trim().to_string()]);
            }
            for l in &labels {
                flags.extend(["--label".to_string(), l.clone()]);
            }
            for a in &assignees {
                flags.extend(["--assignee".to_string(), a.clone()]);
            }
            match forge_args(forge, "issue", "create", &[], &flags) {
                Some(args) => vec![FlowAction::RunCli {
                    label: "issue create".to_string(),
                    args,
                    timeout_ms: OP_TIMEOUT_MS,
                }],
                None => vec![FlowAction::ClearFlow],
            }
        }
        _ => Vec::new(),
    }
}

fn issue_create_on_cli(
    f: &mut IssueCreateFlow,
    result: Result<Value, CliError>,
) -> Vec<FlowAction> {
    match result {
        Ok(payload) => {
            let url = jstr(payload.get("url"));
            let number = num(payload.get("number")).map(|n| n as i64);
            let _ = f;
            vec![
                FlowAction::SetStatus(format!(
                    "issue created{}{}",
                    number.map(|n| format!(" #{n}")).unwrap_or_default(),
                    url.map(|u| format!(" — {u}")).unwrap_or_default()
                )),
                FlowAction::ClearFlow,
                FlowAction::ReloadLists,
            ]
            .into_iter()
            .chain(number.map(FlowAction::OpenIssue))
            .collect()
        }
        Err(err) => vec![
            FlowAction::SetStatus(format!("{}: {}", err.kind, err.message)),
            FlowAction::ClearFlow,
        ],
    }
}

// ===========================================================================
// ForgeTui controller
// ===========================================================================

struct ForgeTui {
    target_label: String,
    capability: Option<Value>,
    forge: ForgeTarget,
    status_left: String,
    stack: Vec<FScreen>,
    tasks: Vec<BgTask>,
    next_screen_id: u64,
    flow: Option<Flow>,
    /// Set when the top modal belongs to the active flow.
    modal_for_flow: bool,
    /// Global prompt awaiting resolution (p = pane id, o = repo override).
    global_prompt: Option<GlobalPrompt>,
    /// Issue label-filter prompt pending (not a flow, not global p/o).
    pending_label_prompt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalPrompt {
    PaneId,
    RepoOverride,
}

impl ForgeTui {
    fn new(pinned: ForgePinned) -> Self {
        let target_label = format_target_label(&pinned);
        let forge = ForgeTarget {
            pane_id: pinned.origin_pane_id.clone(),
            repo_override: None,
        };
        ForgeTui {
            target_label: target_label.clone(),
            capability: None,
            forge,
            status_left: "Git Forge".to_string(),
            stack: vec![FScreen::Home { cursor: 0 }],
            tasks: Vec::new(),
            next_screen_id: 1,
            flow: None,
            modal_for_flow: false,
            global_prompt: None,
            pending_label_prompt: false,
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_screen_id;
        self.next_screen_id += 1;
        id
    }

    fn current(&self) -> &FScreen {
        self.stack.last().expect("stack never empty")
    }

    fn current_mut(&mut self) -> &mut FScreen {
        self.stack.last_mut().expect("stack never empty")
    }

    fn push(&mut self, screen: FScreen) {
        self.stack.push(screen);
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    fn at_root(&self) -> bool {
        self.stack.len() == 1
    }

    fn set_status(&mut self, left: impl Into<String>) {
        self.status_left = left.into();
    }

    // --- load args (computed per-screen so reloads hit the right target) ------

    fn load_args_for(&self, screen_id: u64, kind: LoadKind) -> Option<Vec<String>> {
        // Find the owning screen for its filters/number.
        let screen = self.stack.iter().find(|s| s.id() == Some(screen_id));
        match kind {
            LoadKind::PrList => {
                let state = screen
                    .and_then(|s| match s {
                        FScreen::List(l) if l.kind == ListKind::Prs => Some(l.state.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "open".to_string());
                forge_args(
                    &self.forge,
                    "pr",
                    "list",
                    &[],
                    &["--state".into(), state, "--limit".into(), "50".into()],
                )
            }
            LoadKind::IssueList => {
                let (state, label_filter) = screen
                    .and_then(|s| match s {
                        FScreen::List(l) if l.kind == ListKind::Issues => {
                            Some((l.state.clone(), l.label_filter.clone()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| ("open".to_string(), String::new()));
                let mut flags = vec!["--state".into(), state, "--limit".into(), "50".into()];
                for label in split_csv(&label_filter) {
                    flags.push("--label".into());
                    flags.push(label);
                }
                forge_args(&self.forge, "issue", "list", &[], &flags)
            }
            LoadKind::PrStatus => forge_args(&self.forge, "pr", "status", &[], &[]),
            LoadKind::PrDetail | LoadKind::IssueDetail => {
                let number = screen.and_then(|s| match s {
                    FScreen::Detail(d) => Some(d.number),
                    _ => None,
                })?;
                let (family, action) = if kind == LoadKind::PrDetail {
                    ("pr", "view")
                } else {
                    ("issue", "view")
                };
                forge_args(&self.forge, family, action, &[number.to_string()], &[])
            }
            LoadKind::PrComments => {
                let number = screen.and_then(|s| match s {
                    FScreen::List(l) if l.kind == ListKind::Comments => l.pr_number,
                    _ => None,
                })?;
                forge_args(&self.forge, "pr", "comments", &[number.to_string()], &[])
            }
            LoadKind::PrLogsRuns => {
                let number = screen.and_then(|s| match s {
                    FScreen::List(l) if l.kind == ListKind::Logs => l.pr_number,
                    _ => None,
                })?;
                forge_args(&self.forge, "pr", "logs", &[number.to_string()], &[])
            }
            LoadKind::PrLogsJobs => {
                let (number, run) = screen.and_then(|s| match s {
                    FScreen::List(l) if l.kind == ListKind::Logs => {
                        Some((l.pr_number?, l.current_run_id.clone()?))
                    }
                    _ => None,
                })?;
                forge_args(
                    &self.forge,
                    "pr",
                    "logs",
                    &[number.to_string()],
                    &["--run".into(), run],
                )
            }
        }
    }

    // --- background tasks ---------------------------------------------------------

    fn spawn_load(&mut self, screen_id: u64, kind: LoadKind, label: &str) {
        let Some(args) = self.load_args_for(screen_id, kind) else {
            return;
        };
        self.spawn_task(BgKind::Load, label, move || BgResult::Load {
            screen_id,
            kind,
            result: run_cli(&args, OP_TIMEOUT_MS),
        });
    }

    fn spawn_task<F>(&mut self, kind: BgKind, label: &str, f: F)
    where
        F: FnOnce() -> BgResult + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<BgResult>();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let _ = kind;
        self.tasks.push(BgTask {
            label: label.to_string(),
            rx,
        });
    }

    fn poll_tasks(&mut self) -> bool {
        let mut i = 0;
        let mut any_running = false;
        while i < self.tasks.len() {
            match self.tasks[i].rx.try_recv() {
                Ok(result) => {
                    let _task = self.tasks.remove(i);
                    self.apply_task_result(result);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    any_running = true;
                    i += 1;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tasks.remove(i);
                }
            }
        }
        any_running
    }

    fn busy_label(&self) -> Option<String> {
        self.tasks.last().map(|t| t.label.clone())
    }

    fn apply_task_result(&mut self, result: BgResult) {
        match result {
            BgResult::Load {
                screen_id,
                kind,
                result,
            } => self.apply_load_result(screen_id, kind, result),
            BgResult::Payload { tag, result } => self.apply_payload_result(tag, result),
            BgResult::FlowCli { result } => self.apply_flow_cli_result(result),
        }
    }

    fn apply_load_result(
        &mut self,
        screen_id: u64,
        kind: LoadKind,
        result: Result<Value, CliError>,
    ) {
        let pos = self.stack.iter().rposition(|s| s.id() == Some(screen_id));
        let Some(pos) = pos else { return };
        match result {
            Ok(payload) => match &mut self.stack[pos] {
                FScreen::List(list) => apply_list_payload(list, kind, &payload),
                FScreen::Detail(detail) => apply_detail_payload(detail, kind, &payload),
                _ => {}
            },
            Err(err) => {
                let err_pair = (Some(err.kind.clone()), err.message.clone());
                match &mut self.stack[pos] {
                    FScreen::List(list) => {
                        list.phase = LoadPhase::Error;
                        list.error = Some(err_pair);
                    }
                    FScreen::Detail(d) => {
                        d.phase = DetailPhase::Error;
                        d.error = Some(err_pair);
                    }
                    _ => {}
                }
            }
        }
    }

    fn apply_payload_result(&mut self, tag: PayloadTag, result: Result<Value, CliError>) {
        match result {
            Ok(payload) => {
                // Head-sha feed: update the owning PR detail, no pushed view.
                if let PayloadTag::HeadSha(_) = tag {
                    if let Some(sha) = head_sha_of(&payload) {
                        self.apply_head_sha(sha);
                    }
                    return;
                }
                let (title, lines) = match tag {
                    PayloadTag::Reviews(n) => (
                        format!("pr #{n} — reviews"),
                        format_pr_reviews_payload(&payload),
                    ),
                    PayloadTag::Checks(n) => (
                        format!("pr #{n} — checks"),
                        format_pr_checks_payload(&payload),
                    ),
                    PayloadTag::JobLog(job) => (
                        format!("job #{job} log"),
                        format_pr_job_log_payload(&payload),
                    ),
                    PayloadTag::HeadSha(_) => unreachable!(),
                };
                let id = self.next_id();
                self.push(FScreen::Detail(FDetailScreen {
                    id,
                    title,
                    lines,
                    phase: DetailPhase::Ready,
                    error: None,
                    scroll: 0,
                    footer: "esc — back".to_string(),
                    opened_updated_at: None,
                    opened_head_sha: None,
                    number: 0,
                    is_issue: false,
                    load_kind: None,
                    pr_snapshot: None,
                    issue_snapshot: None,
                }));
            }
            Err(err) => {
                if matches!(tag, PayloadTag::HeadSha(_)) {
                    // Head-sha feed failure: mutations will revalidate anyway.
                    return;
                }
                let title = match &tag {
                    PayloadTag::Reviews(n) | PayloadTag::HeadSha(n) => {
                        format!("pr #{n} — reviews")
                    }
                    PayloadTag::Checks(n) => format!("pr #{n} — checks"),
                    PayloadTag::JobLog(job) => format!("job {job} log"),
                };
                let lines = forge_error_lines(Some(&err.kind), &err.message);
                let id = self.next_id();
                self.push(FScreen::Detail(FDetailScreen {
                    id,
                    title,
                    lines,
                    phase: DetailPhase::Error,
                    error: Some((Some(err.kind), err.message)),
                    scroll: 0,
                    footer: "esc — back".to_string(),
                    opened_updated_at: None,
                    opened_head_sha: None,
                    number: 0,
                    is_issue: false,
                    load_kind: None,
                    pr_snapshot: None,
                    issue_snapshot: None,
                }));
            }
        }
    }

    /// Store a fresh head sha on the topmost actionable PR detail.
    fn apply_head_sha(&mut self, sha: String) {
        for s in self.stack.iter_mut().rev() {
            if let FScreen::Detail(d) = s {
                if !d.is_issue && d.load_kind == Some(LoadKind::PrDetail) {
                    d.opened_head_sha = Some(sha.clone());
                    if d.phase == DetailPhase::Ready {
                        let line = format!("head sha (expect-head): {sha}");
                        if let Some(pos) = d.lines.iter().position(|l| l.starts_with("head sha")) {
                            d.lines[pos] = line;
                        } else {
                            d.lines.push(line);
                        }
                    }
                    return;
                }
            }
        }
    }

    /// Route a finished flow CLI result into the active flow.
    fn apply_flow_cli_result(&mut self, result: Result<Value, CliError>) {
        let forge = self.forge.clone();
        let label = self.target_label.clone();
        let actions = match self.flow.as_mut() {
            Some(Flow::PrMut(f)) => pr_mut_on_cli(f, result, &forge, &label),
            Some(Flow::IssueMut(f)) => issue_mut_on_cli(f, result, &forge, &label),
            Some(Flow::PrCreate(f)) => pr_create_on_cli(f, result),
            Some(Flow::IssueCreate(f)) => issue_create_on_cli(f, result),
            None => return,
        };
        for a in actions {
            self.apply_flow_action(a);
        }
    }

    /// Route a resolved modal into the active flow (or global handlers).
    fn route_modal_result(&mut self, result: ModalResult) {
        if self.modal_for_flow {
            let forge = self.forge.clone();
            let label = self.target_label.clone();
            let actions = match self.flow.as_mut() {
                Some(Flow::PrMut(f)) => pr_mut_on_modal(f, result, &forge, &label),
                Some(Flow::IssueMut(f)) => issue_mut_on_modal(f, result, &forge, &label),
                Some(Flow::PrCreate(f)) => pr_create_on_modal(f, result, &forge, &label),
                Some(Flow::IssueCreate(f)) => issue_create_on_modal(f, result, &forge, &label),
                None => Vec::new(),
            };
            self.modal_for_flow = false;
            for a in actions {
                self.apply_flow_action(a);
            }
            return;
        }
        if self.pending_label_prompt {
            self.pending_label_prompt = false;
            if let ModalResult::Text(Some(text)) = result {
                self.apply_label_filter(text);
            }
            return;
        }
        if self.global_prompt.is_some() {
            let which = self.global_prompt.take();
            if let (Some(w), ModalResult::Text(text)) = (which, result) {
                self.apply_global_prompt(w, text);
            }
        }
    }

    fn apply_flow_action(&mut self, action: FlowAction) {
        match action {
            FlowAction::PushPrompt { label, initial } => {
                self.modal_for_flow = true;
                self.push(FScreen::Prompt(FPromptScreen {
                    label,
                    draft: initial,
                }));
            }
            FlowAction::PushConfirm(spec) => {
                self.modal_for_flow = true;
                self.push(FScreen::Confirm(FConfirmScreen {
                    title: spec.title,
                    target: spec.target,
                    consequence: spec.consequence,
                    confirm_label: spec.confirm_label,
                    cancel_label: spec.cancel_label,
                    danger: spec.danger,
                }));
            }
            FlowAction::PushSelect(spec) => {
                self.modal_for_flow = true;
                self.push(FScreen::Select(FSelectScreen {
                    title: spec.title,
                    items: spec.items,
                    cursor: 0,
                }));
            }
            FlowAction::RunCli {
                label,
                args,
                timeout_ms,
            } => {
                self.spawn_task(BgKind::FlowCli, &label, move || BgResult::FlowCli {
                    result: run_cli(&args, timeout_ms),
                });
            }
            FlowAction::SetStatus(s) => self.set_status(s),
            FlowAction::ReloadOwner { screen_id } => self.reload_owner_screen(screen_id),
            FlowAction::ReloadLists => self.reload_lists(),
            FlowAction::OpenPr(n) => self.open_pr_detail(n),
            FlowAction::OpenIssue(n) => self.open_issue_detail(n),
            FlowAction::ClearFlow => {
                self.flow = None;
                self.modal_for_flow = false;
            }
        }
    }

    /// Begin a flow: install it and drive the first step.
    fn start_flow(&mut self, flow: Flow) {
        self.flow = Some(flow);
        self.drive_flow();
    }

    fn drive_flow(&mut self) {
        let forge = self.forge.clone();
        let label = self.target_label.clone();
        let actions = match self.flow.as_mut() {
            Some(Flow::PrMut(f)) => pr_mut_advance(f, &forge, &label),
            Some(Flow::IssueMut(f)) => issue_mut_advance(f, &forge, &label),
            Some(Flow::PrCreate(f)) => pr_create_advance(f, &forge, &label),
            Some(Flow::IssueCreate(f)) => issue_create_advance(f, &forge, &label),
            None => return,
        };
        for a in actions {
            self.apply_flow_action(a);
        }
    }

    /// Re-run the load for whatever screen owns the active flow.
    fn reload_owner_screen(&mut self, screen_id: u64) {
        let found = self.stack.iter().find_map(|s| match s {
            FScreen::Detail(d) if d.id == screen_id => Some((
                screen_id,
                d.load_kind,
                if d.is_issue { "issue view" } else { "pr view" },
            )),
            FScreen::List(l) if l.id == screen_id => {
                let kind = match l.kind {
                    ListKind::Comments => Some(LoadKind::PrComments),
                    ListKind::Logs => Some(if l.log_mode == LogMode::Jobs {
                        LoadKind::PrLogsJobs
                    } else {
                        LoadKind::PrLogsRuns
                    }),
                    _ => None,
                };
                kind.map(|k| (screen_id, Some(k), "refresh"))
            }
            _ => None,
        });
        if let Some((id, Some(k), label)) = found {
            // Mark loading first.
            for s in self.stack.iter_mut() {
                if s.id() == Some(id) {
                    match s {
                        FScreen::Detail(d) => d.phase = DetailPhase::Loading,
                        FScreen::List(l) => l.phase = LoadPhase::Loading,
                        _ => {}
                    }
                }
            }
            self.spawn_load(id, k, label);
        }
    }

    fn reload_lists(&mut self) {
        let ids: Vec<(u64, LoadKind)> = self
            .stack
            .iter()
            .filter_map(|s| match s {
                FScreen::List(l) => {
                    let kind = match l.kind {
                        ListKind::Prs => LoadKind::PrList,
                        ListKind::Issues => LoadKind::IssueList,
                        ListKind::Comments => LoadKind::PrComments,
                        ListKind::Logs => {
                            if l.log_mode == LogMode::Jobs {
                                LoadKind::PrLogsJobs
                            } else {
                                LoadKind::PrLogsRuns
                            }
                        }
                    };
                    Some((l.id, kind))
                }
                _ => None,
            })
            .collect();
        for (id, kind) in ids {
            if let FScreen::List(l) = self
                .stack
                .iter_mut()
                .find(|s| s.id() == Some(id))
                .expect("list")
            {
                l.phase = LoadPhase::Loading;
            }
            self.spawn_load(id, kind, "refresh");
        }
    }

    // ===========================================================================
    // View navigation
    // ===========================================================================

    fn open_pr_list(&mut self) {
        let id = self.next_id();
        self.push(FScreen::List(FListScreen {
            id,
            kind: ListKind::Prs,
            items: Vec::new(),
            cursor: 0,
            scroll_top: 0,
            filter_active: false,
            filter_draft: String::new(),
            committed_filter: String::new(),
            phase: LoadPhase::Loading,
            error: None,
            state: "open".to_string(),
            label_filter: String::new(),
            log_mode: LogMode::Runs,
            current_run_id: None,
            pr_number: None,
            opened_updated_at: None,
            opened_head_sha: None,
        }));
        self.spawn_load(id, LoadKind::PrList, "pr list");
    }

    fn open_issue_list(&mut self) {
        let id = self.next_id();
        self.push(FScreen::List(FListScreen {
            id,
            kind: ListKind::Issues,
            items: Vec::new(),
            cursor: 0,
            scroll_top: 0,
            filter_active: false,
            filter_draft: String::new(),
            committed_filter: String::new(),
            phase: LoadPhase::Loading,
            error: None,
            state: "open".to_string(),
            label_filter: String::new(),
            log_mode: LogMode::Runs,
            current_run_id: None,
            pr_number: None,
            opened_updated_at: None,
            opened_head_sha: None,
        }));
        self.spawn_load(id, LoadKind::IssueList, "issue list");
    }

    fn open_pr_status(&mut self) {
        let id = self.next_id();
        self.push(FScreen::Detail(FDetailScreen {
            id,
            title: "pr status".to_string(),
            lines: vec!["loading…".to_string()],
            phase: DetailPhase::Loading,
            error: None,
            scroll: 0,
            footer: "R — refresh · esc — back".to_string(),
            opened_updated_at: None,
            opened_head_sha: None,
            number: 0,
            is_issue: false,
            load_kind: Some(LoadKind::PrStatus),
            pr_snapshot: None,
            issue_snapshot: None,
        }));
        self.spawn_load(id, LoadKind::PrStatus, "pr status");
    }

    fn open_pr_detail(&mut self, number: i64) {
        let id = self.next_id();
        self.push(FScreen::Detail(FDetailScreen {
            id,
            title: format!("pr #{number}"),
            lines: vec!["loading…".to_string()],
            phase: DetailPhase::Loading,
            error: None,
            scroll: 0,
            footer: "c comments · v reviews · t checks · l logs · m comment · r review · e reviewers · g merge · u url · R refresh".to_string(),
            opened_updated_at: None,
            opened_head_sha: None,
            number,
            is_issue: false,
            load_kind: Some(LoadKind::PrDetail),
            pr_snapshot: None,
            issue_snapshot: None,
        }));
        self.spawn_load(id, LoadKind::PrDetail, "pr view");
        // `pr reviews` feeds head.sha freshness (no pushed view).
        if let Some(args) = forge_args(&self.forge, "pr", "reviews", &[number.to_string()], &[]) {
            self.spawn_task(BgKind::Payload, "pr reviews", move || BgResult::Payload {
                tag: PayloadTag::HeadSha(number),
                result: run_cli(&args, OP_TIMEOUT_MS),
            });
        }
    }

    fn open_issue_detail(&mut self, number: i64) {
        let id = self.next_id();
        self.push(FScreen::Detail(FDetailScreen {
            id,
            title: format!("issue #{number}"),
            lines: vec!["loading…".to_string()],
            phase: DetailPhase::Loading,
            error: None,
            scroll: 0,
            footer: "c comment · e edit · x close/reopen · u url · R refresh".to_string(),
            opened_updated_at: None,
            opened_head_sha: None,
            number,
            is_issue: true,
            load_kind: Some(LoadKind::IssueDetail),
            pr_snapshot: None,
            issue_snapshot: None,
        }));
        self.spawn_load(id, LoadKind::IssueDetail, "issue view");
    }

    fn open_pr_comments(
        &mut self,
        number: i64,
        opened_updated_at: Option<String>,
        opened_head_sha: Option<String>,
    ) {
        let id = self.next_id();
        self.push(FScreen::List(FListScreen {
            id,
            kind: ListKind::Comments,
            items: Vec::new(),
            cursor: 0,
            scroll_top: 0,
            filter_active: false,
            filter_draft: String::new(),
            committed_filter: String::new(),
            phase: LoadPhase::Loading,
            error: None,
            state: String::new(),
            label_filter: String::new(),
            log_mode: LogMode::Runs,
            current_run_id: None,
            pr_number: Some(number),
            opened_updated_at,
            opened_head_sha,
        }));
        self.spawn_load(id, LoadKind::PrComments, "pr comments");
    }

    fn open_pr_logs(&mut self, number: i64) {
        let id = self.next_id();
        self.push(FScreen::List(FListScreen {
            id,
            kind: ListKind::Logs,
            items: Vec::new(),
            cursor: 0,
            scroll_top: 0,
            filter_active: false,
            filter_draft: String::new(),
            committed_filter: String::new(),
            phase: LoadPhase::Loading,
            error: None,
            state: String::new(),
            label_filter: String::new(),
            log_mode: LogMode::Runs,
            current_run_id: None,
            pr_number: Some(number),
            opened_updated_at: None,
            opened_head_sha: None,
        }));
        self.spawn_load(id, LoadKind::PrLogsRuns, "pr logs (runs)");
    }

    // ===========================================================================
    // Payload application (load results → screen state)
    // ===========================================================================

    // ===========================================================================
    // Flow starters (called from key handling)
    // ===========================================================================

    fn gate(&mut self, command: &str) -> bool {
        let (allowed, reason) = mutation_allowed(self.capability.as_ref(), command);
        if !allowed {
            self.set_status(format!(
                "mutations disabled: {}",
                reason.unwrap_or_else(|| command.to_string())
            ));
            return false;
        }
        true
    }

    fn detail_snapshot(&self, owner_id: u64) -> Option<(Value, bool)> {
        for s in self.stack.iter() {
            if let FScreen::Detail(d) = s {
                if d.id == owner_id {
                    let snap = if d.is_issue {
                        d.issue_snapshot.clone()
                    } else {
                        d.pr_snapshot.clone()
                    };
                    return snap.map(|v| (v, d.is_issue));
                }
            }
        }
        None
    }

    fn detail_freshness(&self, owner_id: u64) -> (Option<String>, Option<String>) {
        for s in self.stack.iter() {
            if let FScreen::Detail(d) = s {
                if d.id == owner_id {
                    return (d.opened_updated_at.clone(), d.opened_head_sha.clone());
                }
            }
        }
        (None, None)
    }

    fn detail_pr_meta(&self, owner_id: u64) -> (String, String, bool, String, String) {
        if let Some((pr, false)) = self.detail_snapshot(owner_id) {
            return (
                fmt(pr.get("title")),
                fmt(pr.get("state")),
                bool_at(&pr, "isDraft") || bool_at(&pr, "is_draft"),
                fmt(pr
                    .get("mergeStateStatus")
                    .or_else(|| pr.get("merge_state_status"))),
                fmt(pr
                    .get("reviewDecision")
                    .or_else(|| pr.get("review_decision"))),
            );
        }
        (
            String::new(),
            String::new(),
            false,
            String::new(),
            String::new(),
        )
    }

    fn start_pr_mut(
        &mut self,
        number: i64,
        spec: PrMutSpec,
        owner_id: u64,
        merge_mode_preset: Option<String>,
    ) {
        let (opened_updated_at, opened_head_sha) = self.detail_freshness(owner_id);
        let (pr_title, pr_state, pr_is_draft, pr_merge_state, pr_review_decision) =
            self.detail_pr_meta(owner_id);
        let is_merge = merge_mode_preset.is_some();
        let steps = if is_merge {
            // Merge flow: mode select first (spec built after).
            vec![InputStep::Select {
                key: "mode",
                spec: SelectSpec {
                    title: "Merge method".to_string(),
                    items: vec![
                        SelectOpt {
                            label: "Squash merge".to_string(),
                            detail: "merge now with squash method".to_string(),
                            value: "squash".to_string(),
                        },
                        SelectOpt {
                            label: "Merge commit".to_string(),
                            detail: "merge now with a merge commit".to_string(),
                            value: "merge".to_string(),
                        },
                        SelectOpt {
                            label: "Rebase merge".to_string(),
                            detail: "merge now with rebase method".to_string(),
                            value: "rebase".to_string(),
                        },
                        SelectOpt {
                            label: "Merge (repo default)".to_string(),
                            detail: "merge now, GitHub default method".to_string(),
                            value: "default".to_string(),
                        },
                        SelectOpt {
                            label: "Enable auto-merge".to_string(),
                            detail: "arm GitHub auto-merge (method required)".to_string(),
                            value: "auto".to_string(),
                        },
                        SelectOpt {
                            label: "Disable auto-merge".to_string(),
                            detail: "disarm GitHub auto-merge".to_string(),
                            value: "disable-auto".to_string(),
                        },
                    ],
                },
            }]
        } else {
            spec.input_steps()
        };
        let inputs = BTreeMap::new();
        self.start_flow(Flow::PrMut(PrMutFlow {
            number,
            inputs,
            steps,
            step_idx: 0,
            spec: if is_merge {
                Some(PrMutSpec::Merge {
                    mode: "__pending__".to_string(),
                })
            } else {
                Some(spec)
            },
            stage: PrMutStage::Inputs,
            opened_updated_at,
            opened_head_sha,
            fresh_reviews: None,
            fresh_head_sha: None,
            fresh_updated_at: None,
            pr_title,
            pr_state,
            pr_is_draft,
            pr_merge_state,
            pr_review_decision,
            owner_screen_id: Some(owner_id),
            pending_confirm: None,
        }));
    }

    fn start_pr_comment_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("pr") {
            return;
        }
        self.start_pr_mut(
            number,
            PrMutSpec::Comment {
                body: String::new(),
            },
            owner_id,
            None,
        );
    }

    fn start_pr_review_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("pr") {
            return;
        }
        self.start_pr_mut(
            number,
            PrMutSpec::Review {
                event: String::new(),
                body: String::new(),
            },
            owner_id,
            None,
        );
    }

    fn start_pr_reviewers_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("pr") {
            return;
        }
        self.start_pr_mut(
            number,
            PrMutSpec::Reviewers {
                add: String::new(),
                remove: String::new(),
            },
            owner_id,
            None,
        );
    }

    fn start_pr_merge_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("pr") {
            return;
        }
        self.start_pr_mut(
            number,
            PrMutSpec::Merge {
                mode: String::new(),
            },
            owner_id,
            Some(String::new()),
        );
    }

    /// Reply to a line comment from the comments list.
    fn start_pr_reply_flow(&mut self, number: i64, comment: &Value, owner_id: u64) {
        if !self.gate("pr") {
            return;
        }
        let comment_id = num(comment.get("id")).map(|n| n as i64);
        let Some(cid) = comment_id else {
            self.set_status("selected comment has no id — cannot reply");
            return;
        };
        let loc = format!("{}:{}", fmt(comment.get("path")), fmt(comment.get("line")));
        let (opened_updated_at, opened_head_sha) = self.list_freshness(owner_id);
        let (pr_title, pr_state, pr_is_draft, pr_merge_state, pr_review_decision) =
            self.detail_pr_meta_for_reply(number);
        self.start_flow(Flow::PrMut(PrMutFlow {
            number,
            inputs: BTreeMap::new(),
            steps: PrMutSpec::Reply {
                comment_id: cid,
                body: String::new(),
                loc,
            }
            .input_steps(),
            step_idx: 0,
            spec: Some(PrMutSpec::Reply {
                comment_id: cid,
                body: String::new(),
                loc: format!("{}:{}", fmt(comment.get("path")), fmt(comment.get("line"))),
            }),
            stage: PrMutStage::Inputs,
            opened_updated_at,
            opened_head_sha,
            fresh_reviews: None,
            fresh_head_sha: None,
            fresh_updated_at: None,
            pr_title,
            pr_state,
            pr_is_draft,
            pr_merge_state,
            pr_review_decision,
            owner_screen_id: Some(owner_id),
            pending_confirm: None,
        }));
    }

    fn list_freshness(&self, screen_id: u64) -> (Option<String>, Option<String>) {
        for s in self.stack.iter() {
            if let FScreen::List(l) = s {
                if l.id == screen_id {
                    return (l.opened_updated_at.clone(), l.opened_head_sha.clone());
                }
            }
        }
        (None, None)
    }

    /// PR meta for reply flow: reply originates from the comments list,
    /// not a detail screen — look up the parent PR detail if present.
    fn detail_pr_meta_for_reply(&self, _number: i64) -> (String, String, bool, String, String) {
        // Find the nearest PR detail screen below the comments list.
        for s in self.stack.iter().rev() {
            if let FScreen::Detail(d) = s {
                if !d.is_issue && d.load_kind == Some(LoadKind::PrDetail) {
                    return self.detail_pr_meta(d.id);
                }
            }
        }
        (
            String::new(),
            String::new(),
            false,
            String::new(),
            String::new(),
        )
    }

    // ---- Issue mutation starters ----------------------------------------------

    fn start_issue_mut(
        &mut self,
        number: i64,
        spec: Option<IssueMutSpec>,
        steps: Vec<InputStep>,
        owner_id: u64,
        edit_current_title: Option<String>,
    ) {
        let (opened_updated_at, _) = self.detail_freshness(owner_id);
        self.start_flow(Flow::IssueMut(IssueMutFlow {
            number,
            inputs: BTreeMap::new(),
            steps,
            step_idx: 0,
            spec,
            stage: IssueMutStage::Inputs,
            opened_updated_at,
            fresh_updated_at: None,
            edit_current_title,
            owner_screen_id: Some(owner_id),
            pending_confirm: None,
        }));
    }

    fn start_issue_comment_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("issue") {
            return;
        }
        self.start_issue_mut(
            number,
            Some(IssueMutSpec::Comment {
                body: String::new(),
            }),
            issue_comment_steps(),
            owner_id,
            None,
        );
    }

    fn start_issue_edit_flow(&mut self, number: i64, owner_id: u64) {
        if !self.gate("issue") {
            return;
        }
        let (current_title, current_state) = self.issue_meta(owner_id);
        self.start_issue_mut(
            number,
            None, // spec resolved from inputs at confirm time
            issue_edit_steps(&current_title, &current_state),
            owner_id,
            Some(current_title),
        );
    }

    fn issue_meta(&self, owner_id: u64) -> (String, String) {
        if let Some((issue, true)) = self.detail_snapshot(owner_id) {
            (
                jstr(issue.get("title")).unwrap_or_default(),
                fmt(issue.get("state")),
            )
        } else {
            (String::new(), "?".to_string())
        }
    }

    fn start_issue_close_reopen(&mut self, number: i64, owner_id: u64) {
        if !self.gate("issue") {
            return;
        }
        let state = self
            .detail_snapshot(owner_id)
            .and_then(|(i, is_issue)| {
                if is_issue {
                    Some(fmt(i.get("state")).to_lowercase())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        if state == "closed" {
            self.start_issue_mut(
                number,
                Some(IssueMutSpec::Reopen),
                Vec::new(),
                owner_id,
                None,
            );
            return;
        }
        self.start_issue_mut(
            number,
            Some(IssueMutSpec::Close {
                reason: String::new(),
            }),
            issue_close_steps(),
            owner_id,
            None,
        );
    }

    // ---- Create starters --------------------------------------------------------

    fn start_pr_create(&mut self) {
        self.start_flow(Flow::PrCreate(PrCreateFlow {
            inputs: BTreeMap::new(),
            steps: pr_create_steps(),
            step_idx: 0,
            stage: PrCreateStage::Inputs,
            draft: false,
        }));
    }

    fn start_issue_create(&mut self) {
        self.start_flow(Flow::IssueCreate(IssueCreateFlow {
            inputs: BTreeMap::new(),
            steps: issue_create_steps(),
            step_idx: 0,
            stage: IssueCreateStage::Inputs,
        }));
    }

    // ===========================================================================
    // Key handling
    // ===========================================================================

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match self.current_mut() {
            FScreen::Prompt(_) => self.handle_prompt_key(key),
            FScreen::Confirm(_) => self.handle_confirm_key(key),
            FScreen::Select(_) => self.handle_select_key(key),
            FScreen::Detail(_) => self.handle_detail_key(key),
            FScreen::List(_) => self.handle_list_key(key),
            FScreen::Home { .. } => self.handle_home_key(key),
        }
    }

    fn handle_home_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let FScreen::Home { cursor } = self.current_mut() {
                    *cursor = cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let FScreen::Home { cursor } = self.current_mut() {
                    *cursor = (*cursor + 1).min(2);
                }
            }
            KeyCode::Enter => {
                let idx = match self.current() {
                    FScreen::Home { cursor } => *cursor,
                    _ => 0,
                };
                match idx {
                    0 => self.open_pr_list(),
                    1 => self.open_issue_list(),
                    _ => self.open_pr_status(),
                }
            }
            KeyCode::Char('p') => self.open_global_prompt(GlobalPrompt::PaneId),
            KeyCode::Char('o') => self.open_global_prompt(GlobalPrompt::RepoOverride),
            KeyCode::Esc | KeyCode::Char('q') => return false,
            _ => {}
        }
        true
    }

    fn open_global_prompt(&mut self, which: GlobalPrompt) {
        self.global_prompt = Some(which);
        let (label, initial) = match which {
            GlobalPrompt::PaneId => (
                "Pane id for the forge target (repo resolves from its cwd)".to_string(),
                self.forge.pane_id.clone().unwrap_or_default(),
            ),
            GlobalPrompt::RepoOverride => (
                "Forge repo override (owner/name — empty clears)".to_string(),
                self.forge.repo_override.clone().unwrap_or_default(),
            ),
        };
        self.push(FScreen::Prompt(FPromptScreen {
            label,
            draft: initial,
        }));
    }

    fn apply_global_prompt(&mut self, which: GlobalPrompt, text: Option<String>) {
        let Some(text) = text else { return };
        match which {
            GlobalPrompt::PaneId => {
                let value = text.trim().to_string();
                if value.is_empty() {
                    self.set_status("empty pane id — cancelled");
                    return;
                }
                if value.starts_with('-') {
                    self.set_status("pane id must not start with '-'");
                    return;
                }
                self.forge.pane_id = Some(value.clone());
                self.set_status(format!("forge pane → {value} — press R to reload"));
            }
            GlobalPrompt::RepoOverride => {
                let value = text.trim().to_string();
                if value.is_empty() {
                    self.forge.repo_override = None;
                    self.set_status("repo override cleared — press R to reload");
                    return;
                }
                if let Some(problem) = validate_repo_override(&value) {
                    self.set_status(problem);
                    return;
                }
                self.forge.repo_override = Some(value.clone());
                self.set_status(format!("repo override → {value} — press R to reload"));
            }
        }
    }

    fn apply_label_filter(&mut self, text: String) {
        let value = text.trim().to_string();
        // Find the issues list screen and update its filter.
        let id = {
            let mut found = None;
            for s in self.stack.iter_mut().rev() {
                if let FScreen::List(l) = s {
                    if l.kind == ListKind::Issues {
                        l.label_filter = value.clone();
                        l.phase = LoadPhase::Loading;
                        found = Some(l.id);
                        break;
                    }
                }
            }
            found
        };
        if let Some(id) = id {
            self.set_status(if value.is_empty() {
                "label filter cleared".to_string()
            } else {
                format!("label filter → {value}")
            });
            self.spawn_load(id, LoadKind::IssueList, "issue list");
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.pop();
                self.route_modal_result(ModalResult::Cancel);
                true
            }
            KeyCode::Enter => {
                let draft = match self.current() {
                    FScreen::Prompt(p) => p.draft.clone(),
                    _ => return true,
                };
                self.pop();
                self.route_modal_result(ModalResult::Text(Some(draft)));
                true
            }
            KeyCode::Backspace => {
                if let FScreen::Prompt(p) = self.current_mut() {
                    p.draft.pop();
                }
                true
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let FScreen::Prompt(p) = self.current_mut() {
                    p.draft.push(c);
                }
                true
            }
            _ => true,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.pop();
                self.route_modal_result(ModalResult::Bool(false));
                true
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                self.pop();
                self.route_modal_result(ModalResult::Bool(true));
                true
            }
            _ => true,
        }
    }

    fn handle_select_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pop();
                self.route_modal_result(ModalResult::Cancel);
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let FScreen::Select(s) = self.current_mut() {
                    if !s.items.is_empty() {
                        s.cursor = (s.cursor + s.items.len() - 1) % s.items.len();
                    }
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let FScreen::Select(s) = self.current_mut() {
                    if !s.items.is_empty() {
                        s.cursor = (s.cursor + 1) % s.items.len();
                    }
                }
                true
            }
            KeyCode::Enter => {
                let value = match self.current() {
                    FScreen::Select(s) => s.items.get(s.cursor).map(|i| i.value.clone()),
                    _ => None,
                };
                self.pop();
                self.route_modal_result(ModalResult::Choice(value));
                true
            }
            _ => true,
        }
    }

    // --- detail screens ---------------------------------------------------------

    fn detail_context(&self) -> Option<(i64, bool, u64)> {
        if let FScreen::Detail(d) = self.current() {
            Some((d.number, d.is_issue, d.id))
        } else {
            None
        }
    }

    fn handle_detail_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        // Scroll keys first (j/k/up/down/pgup/pgdn/space/home/end).
        let scrolled = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll = d.scroll.saturating_sub(1);
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll += 1;
                }
                true
            }
            KeyCode::PageUp => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll = d.scroll.saturating_sub(10);
                }
                true
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll += 10;
                }
                true
            }
            KeyCode::Home => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll = 0;
                }
                true
            }
            KeyCode::End => {
                if let FScreen::Detail(d) = self.current_mut() {
                    d.scroll = usize::MAX;
                }
                true
            }
            _ => false,
        };
        if scrolled {
            return true;
        }
        // Target keys work on every detail screen.
        if self.handle_target_key(key) {
            return true;
        }
        let Some((number, is_issue, owner_id)) = self.detail_context() else {
            return false;
        };
        // Phase gate: only actionable detail views (pr/issue view loads)
        // respond to action keys; payload viewers only scroll + R + p/o.
        let actionable = matches!(self.current(), FScreen::Detail(d) if d.load_kind == Some(LoadKind::PrDetail) || d.load_kind == Some(LoadKind::IssueDetail));
        let phase_ready =
            matches!(self.current(), FScreen::Detail(d) if d.phase == DetailPhase::Ready);
        if !actionable {
            // Payload viewers: R does nothing meaningful; esc/q back.
            return match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.pop();
                    true
                }
                _ => false,
            };
        }
        match key.code {
            KeyCode::Char('R') => {
                self.reload_current_detail(owner_id);
                true
            }
            KeyCode::Char('r') if phase_ready && !is_issue => {
                // PR detail: 'r' = review flow (lowercase), 'R' = refresh.
                self.start_pr_review_flow(number, owner_id);
                true
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pop();
                true
            }
            _ if phase_ready => self.detail_action_key(key, number, is_issue, owner_id),
            _ => false,
        }
    }

    fn reload_current_detail(&mut self, owner_id: u64) {
        self.reload_owner_screen(owner_id);
        // PR detail also refreshes the head-sha feed.
        if let Some(n) = self.detail_number(owner_id) {
            if !self.detail_is_issue(owner_id) {
                if let Some(args) = forge_args(&self.forge, "pr", "reviews", &[n.to_string()], &[])
                {
                    self.spawn_task(BgKind::Payload, "pr reviews", move || BgResult::Payload {
                        tag: PayloadTag::HeadSha(n),
                        result: run_cli(&args, OP_TIMEOUT_MS),
                    });
                }
            }
        }
    }

    fn detail_number(&self, id: u64) -> Option<i64> {
        self.stack.iter().find_map(|s| match s {
            FScreen::Detail(d) if d.id == id => Some(d.number),
            _ => None,
        })
    }

    fn detail_is_issue(&self, id: u64) -> bool {
        self.stack.iter().any(|s| match s {
            FScreen::Detail(d) => d.id == id && d.is_issue,
            _ => false,
        })
    }

    fn handle_target_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Char('p') => {
                self.open_global_prompt(GlobalPrompt::PaneId);
                true
            }
            KeyCode::Char('o') => {
                self.open_global_prompt(GlobalPrompt::RepoOverride);
                true
            }
            _ => false,
        }
    }

    fn detail_action_key(
        &mut self,
        key: KeyEvent,
        number: i64,
        is_issue: bool,
        owner_id: u64,
    ) -> bool {
        if key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return self.handle_target_key(key);
        }
        let KeyCode::Char(c) = key.code else {
            return self.handle_target_key(key);
        };
        if !is_issue {
            match c {
                'c' => {
                    let (u, h) = self.detail_freshness(owner_id);
                    self.open_pr_comments(number, u, h);
                    return true;
                }
                'v' => {
                    self.push_pr_payload(number, "reviews", PayloadTag::Reviews);
                    return true;
                }
                't' => {
                    self.push_pr_payload(number, "checks", PayloadTag::Checks);
                    return true;
                }
                'l' => {
                    self.open_pr_logs(number);
                    return true;
                }
                'm' => {
                    self.start_pr_comment_flow(number, owner_id);
                    return true;
                }
                'e' => {
                    self.start_pr_reviewers_flow(number, owner_id);
                    return true;
                }
                'g' => {
                    self.start_pr_merge_flow(number, owner_id);
                    return true;
                }
                'u' => {
                    self.open_pr_url(owner_id);
                    return true;
                }
                _ => {}
            }
        } else {
            match c {
                'c' => {
                    self.start_issue_comment_flow(number, owner_id);
                    return true;
                }
                'e' => {
                    self.start_issue_edit_flow(number, owner_id);
                    return true;
                }
                'x' => {
                    self.start_issue_close_reopen(number, owner_id);
                    return true;
                }
                'u' => {
                    self.open_issue_url(owner_id);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    fn push_pr_payload(&mut self, number: i64, action: &str, tag_fn: fn(i64) -> PayloadTag) {
        let Some(args) = forge_args(&self.forge, "pr", action, &[number.to_string()], &[]) else {
            return;
        };
        let label = format!("pr {action}");
        self.spawn_task(BgKind::Payload, &label, move || BgResult::Payload {
            tag: tag_fn(number),
            result: run_cli(&args, OP_TIMEOUT_MS),
        });
    }

    // ---- URL opening --------------------------------------------------------------

    fn open_pr_url(&mut self, owner_id: u64) {
        let url = self.stack.iter().find_map(|s| match s {
            FScreen::Detail(d) if d.id == owner_id => {
                d.pr_snapshot.as_ref().and_then(|p| jstr(p.get("url")))
            }
            _ => None,
        });
        self.open_url(url, "PR");
    }

    fn open_issue_url(&mut self, owner_id: u64) {
        let url = self.stack.iter().find_map(|s| match s {
            FScreen::Detail(d) if d.id == owner_id => {
                d.issue_snapshot.as_ref().and_then(|p| jstr(p.get("url")))
            }
            _ => None,
        });
        self.open_url(url, "issue");
    }

    fn open_url(&mut self, url: Option<String>, kind: &str) {
        match url {
            Some(u) => {
                let u2 = u.clone();
                std::thread::spawn(move || {
                    let _ = open_external(&u2);
                });
                self.set_status(format!("opened {u}"));
            }
            None => self.set_status(format!("no {kind} url in payload")),
        }
    }

    // ===========================================================================
    // List screen key handling
    // ===========================================================================

    fn handle_list_key(&mut self, key: KeyEvent) -> bool {
        let filtering = matches!(self.current(), FScreen::List(l) if l.filter_active);
        if filtering {
            if let FScreen::List(list) = self.current_mut() {
                match key.code {
                    KeyCode::Esc => {
                        list.filter_active = false;
                        list.filter_draft.clear();
                    }
                    KeyCode::Enter => {
                        list.committed_filter = list.filter_draft.clone();
                        list.filter_active = false;
                    }
                    KeyCode::Backspace => {
                        list.filter_draft.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        list.filter_draft.push(c);
                        list.cursor = 0;
                        list.scroll_top = 0;
                    }
                    _ => {}
                }
            }
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        let (kind, item, list_id) = match self.current() {
            FScreen::List(l) => (l.kind, l.current().cloned(), l.id),
            _ => return true,
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let FScreen::List(l) = self.current_mut() {
                    l.move_cursor(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let FScreen::List(l) = self.current_mut() {
                    l.move_cursor(1);
                }
            }
            KeyCode::PageUp => {
                if let FScreen::List(l) = self.current_mut() {
                    for _ in 0..10 {
                        l.move_cursor(-1);
                    }
                }
            }
            KeyCode::PageDown => {
                if let FScreen::List(l) = self.current_mut() {
                    for _ in 0..10 {
                        l.move_cursor(1);
                    }
                }
            }
            KeyCode::Home => {
                if let FScreen::List(l) = self.current_mut() {
                    l.cursor = 0;
                }
            }
            KeyCode::End => {
                if let FScreen::List(l) = self.current_mut() {
                    let n = l.filtered_indices().len();
                    l.cursor = n.saturating_sub(1);
                }
            }
            KeyCode::Char('/') => {
                if let FScreen::List(l) = self.current_mut() {
                    l.filter_active = true;
                    l.filter_draft = l.committed_filter.clone();
                }
            }
            KeyCode::Enter => {
                if let Some(item) = item {
                    if let Some(n) = num(item.value.get("number")).map(|x| x as i64) {
                        match kind {
                            ListKind::Prs => self.open_pr_detail(n),
                            ListKind::Issues => self.open_issue_detail(n),
                            ListKind::Comments => self.push_comment_detail(&item.value),
                            ListKind::Logs => self.on_logs_select(kind, item.value),
                        }
                    }
                }
            }
            KeyCode::Esc => {
                // Logs in jobs mode: Esc returns to runs (TS onCancel).
                if kind == ListKind::Logs {
                    if let FScreen::List(l) = self.current_mut() {
                        if l.log_mode == LogMode::Jobs {
                            l.log_mode = LogMode::Runs;
                            l.current_run_id = None;
                            l.phase = LoadPhase::Loading;
                            l.set_items(Vec::new());
                            self.spawn_load(list_id, LoadKind::PrLogsRuns, "pr logs (runs)");
                            return true;
                        }
                    }
                }
                if !self.at_root() {
                    self.pop();
                } else {
                    return false;
                }
            }
            KeyCode::Char('q') => {
                if !self.at_root() {
                    self.pop();
                } else {
                    return false;
                }
            }
            KeyCode::Char('s') => match kind {
                ListKind::Prs => self.cycle_pr_state(list_id),
                ListKind::Issues => self.cycle_issue_state(list_id),
                _ => {}
            },
            KeyCode::Char('n') => match kind {
                ListKind::Prs if self.gate("pr") => self.start_pr_create(),
                ListKind::Issues if self.gate("issue") => self.start_issue_create(),
                _ => {}
            },
            KeyCode::Char('l') => {
                if kind == ListKind::Issues {
                    self.pending_label_prompt = true;
                    let current = match self.current() {
                        FScreen::List(l) => l.label_filter.clone(),
                        _ => String::new(),
                    };
                    self.push(FScreen::Prompt(FPromptScreen {
                        label: "Label filter (comma-separated — empty clears)".to_string(),
                        draft: current,
                    }));
                }
            }
            KeyCode::Char('r') => {
                // Comments list: 'r' replies to the selected line comment.
                if kind == ListKind::Comments {
                    self.on_comments_reply(list_id, item.as_ref());
                } else {
                    self.reload_list_screen(list_id);
                }
            }
            KeyCode::Char('R') => self.reload_list_screen(list_id),
            KeyCode::Char('p') => self.open_global_prompt(GlobalPrompt::PaneId),
            KeyCode::Char('o') => self.open_global_prompt(GlobalPrompt::RepoOverride),
            _ => {}
        }
        true
    }

    fn on_comments_reply(&mut self, list_id: u64, item: Option<&FItem>) {
        let Some(item) = item else {
            self.set_status("select a comment first");
            return;
        };
        let is_line = item
            .value
            .get("_kind")
            .and_then(|k| k.as_str())
            .map(|k| k == "line")
            .unwrap_or(false);
        if !is_line {
            self.set_status(
                "replies attach to line comments — select one (L…) or use 'm' on the PR for a conversation comment",
            );
            return;
        }
        // PR number comes from the list screen.
        let number = self.list_pr_number(list_id);
        let Some(n) = number else { return };
        self.start_pr_reply_flow(n, &item.value, list_id);
    }

    fn list_pr_number(&self, screen_id: u64) -> Option<i64> {
        self.stack.iter().find_map(|s| match s {
            FScreen::List(l) if l.id == screen_id => l.pr_number,
            _ => None,
        })
    }

    fn push_comment_detail(&mut self, comment: &Value) {
        let is_line = comment.get("_kind").and_then(|k| k.as_str()) == Some("line");
        let id = fmt(comment.get("id"));
        let author_raw = comment.get("author");
        let author = match author_raw {
            Some(Value::String(s)) => s.clone(),
            Some(o) => fmt(o.get("login").or_else(|| o.get("name"))),
            None => "unknown".to_string(),
        };
        let created = fmt(comment.get("created_at"));
        let head = if is_line {
            format!("{}:{}", fmt(comment.get("path")), fmt(comment.get("line")))
        } else {
            "conversation".to_string()
        };
        let mut lines = vec![format!("author: {author}"), format!("created: {created}")];
        if let Some(u) = jstr(comment.get("url")) {
            lines.push(format!("url: {u}"));
        }
        lines.push(String::new());
        let body = jstr(comment.get("body")).unwrap_or_default();
        lines.extend(body.split('\n').map(|s| s.to_string()));
        if bool_at(comment, "body_truncated") {
            lines.push("… (truncated)".to_string());
        }
        let screen_id = self.next_id();
        self.push(FScreen::Detail(FDetailScreen {
            id: screen_id,
            title: format!("comment #{id} — {head}"),
            lines,
            phase: DetailPhase::Ready,
            error: None,
            scroll: 0,
            footer: "esc — back".to_string(),
            opened_updated_at: None,
            opened_head_sha: None,
            number: 0,
            is_issue: false,
            load_kind: None,
            pr_snapshot: None,
            issue_snapshot: None,
        }));
    }

    fn on_logs_select(&mut self, kind: ListKind, value: Value) {
        if kind != ListKind::Logs {
            return;
        }
        let (mode, pr_number, list_id) = match self.current() {
            FScreen::List(l) if l.kind == ListKind::Logs => (l.log_mode, l.pr_number, l.id),
            _ => return,
        };
        let Some(n) = pr_number else { return };
        let id = jstr(value.get("id"));
        match mode {
            LogMode::Runs => {
                if let Some(run_id) = id {
                    if let FScreen::List(l) = self.current_mut() {
                        l.log_mode = LogMode::Jobs;
                        l.current_run_id = Some(run_id);
                        l.phase = LoadPhase::Loading;
                        l.set_items(Vec::new());
                    }
                    self.spawn_load(list_id, LoadKind::PrLogsJobs, "pr logs (run)");
                }
            }
            LogMode::Jobs => {
                if let Some(job_id) = id {
                    let Some(args) = forge_args(
                        &self.forge,
                        "pr",
                        "logs",
                        &[n.to_string()],
                        &[
                            "--job".into(),
                            job_id.clone(),
                            "--lines".into(),
                            "500".into(),
                        ],
                    ) else {
                        return;
                    };
                    let tag = PayloadTag::JobLog(job_id);
                    self.spawn_task(BgKind::Payload, "pr logs (job)", move || {
                        BgResult::Payload {
                            tag,
                            result: run_cli(&args, JOB_LOG_TIMEOUT_MS),
                        }
                    });
                }
            }
        }
    }

    fn cycle_pr_state(&mut self, list_id: u64) {
        const STATES: [&str; 4] = ["open", "closed", "merged", "all"];
        let new_state = {
            let mut found = None;
            for s in self.stack.iter_mut() {
                if let FScreen::List(l) = s {
                    if l.id == list_id && l.kind == ListKind::Prs {
                        let i = STATES.iter().position(|x| *x == l.state).unwrap_or(0);
                        l.state = STATES[(i + 1) % STATES.len()].to_string();
                        l.phase = LoadPhase::Loading;
                        found = Some(l.state.clone());
                        break;
                    }
                }
            }
            found
        };
        if let Some(state) = new_state {
            self.set_status(format!("pr state filter → {state}"));
            self.spawn_load(list_id, LoadKind::PrList, "pr list");
        }
    }

    fn cycle_issue_state(&mut self, list_id: u64) {
        let new_state = {
            let mut found = None;
            for s in self.stack.iter_mut() {
                if let FScreen::List(l) = s {
                    if l.id == list_id && l.kind == ListKind::Issues {
                        l.state = match l.state.as_str() {
                            "open" => "closed".to_string(),
                            "closed" => "all".to_string(),
                            _ => "open".to_string(),
                        };
                        l.phase = LoadPhase::Loading;
                        found = Some(l.state.clone());
                        break;
                    }
                }
            }
            found
        };
        if let Some(state) = new_state {
            self.set_status(format!("issue state filter → {state}"));
            self.spawn_load(list_id, LoadKind::IssueList, "issue list");
        }
    }

    fn reload_list_screen(&mut self, list_id: u64) {
        let kind = {
            let mut found = None;
            for s in self.stack.iter() {
                if let FScreen::List(l) = s {
                    if l.id == list_id {
                        found = Some(match l.kind {
                            ListKind::Prs => LoadKind::PrList,
                            ListKind::Issues => LoadKind::IssueList,
                            ListKind::Comments => LoadKind::PrComments,
                            ListKind::Logs => {
                                if l.log_mode == LogMode::Jobs {
                                    LoadKind::PrLogsJobs
                                } else {
                                    LoadKind::PrLogsRuns
                                }
                            }
                        });
                        break;
                    }
                }
            }
            found
        };
        if let Some(k) = kind {
            for s in self.stack.iter_mut() {
                if let FScreen::List(l) = s {
                    if l.id == list_id {
                        l.phase = LoadPhase::Loading;
                    }
                }
            }
            self.spawn_load(list_id, k, "refresh");
        }
    }
}

// free helpers -----------------------------------------------------------------

fn apply_list_payload(list: &mut FListScreen, kind: LoadKind, payload: &Value) {
    // Mirrors ForgeTui::apply_list_payload without needing &mut self.
    match kind {
        LoadKind::PrList => {
            let prs = as_records(payload.get("prs"));
            list.set_items(
                prs.iter()
                    .map(|pr| {
                        let v = Value::Object((*pr).clone());
                        let (label, detail) = pr_select_item(&v);
                        FItem {
                            label,
                            detail,
                            value: v,
                        }
                    })
                    .collect(),
            );
        }
        LoadKind::IssueList => {
            let issues = as_records(payload.get("issues"));
            list.set_items(
                issues
                    .iter()
                    .map(|i| {
                        let v = Value::Object((*i).clone());
                        let (label, detail) = issue_select_item(&v);
                        FItem {
                            label,
                            detail,
                            value: v,
                        }
                    })
                    .collect(),
            );
        }
        LoadKind::PrComments => {
            let mut items: Vec<FItem> = Vec::new();
            for c in as_records(payload.get("issue_comments")) {
                items.push(comment_item(c, false));
            }
            for c in as_records(payload.get("line_comments")) {
                items.push(comment_item(c, true));
            }
            list.set_items(items);
        }
        LoadKind::PrLogsRuns => {
            let runs = as_records(payload.get("runs"));
            list.set_items(
                runs.iter()
                    .map(|r| {
                        let v = Value::Object((*r).clone());
                        FItem {
                            label: format!(
                                "run #{} {} — {}/{}",
                                fmt(r.get("id")),
                                fmt(r.get("name")),
                                fmt(r.get("status")),
                                fmt(r.get("conclusion"))
                            ),
                            detail: [
                                format!("workflow: {}", fmt(r.get("workflow"))),
                                format!("created: {}", fmt(r.get("created_at"))),
                                format!("title: {}", fmt(r.get("display_title"))),
                                format!("url: {}", fmt(r.get("url"))),
                            ]
                            .join("\n"),
                            value: v,
                        }
                    })
                    .collect(),
            );
        }
        LoadKind::PrLogsJobs => {
            let jobs = as_records(payload.get("jobs"));
            list.set_items(
                jobs.iter()
                    .map(|j| {
                        let v = Value::Object((*j).clone());
                        FItem {
                            label: format!(
                                "job #{} {} — {}/{}",
                                fmt(j.get("id")),
                                fmt(j.get("name")),
                                fmt(j.get("status")),
                                fmt(j.get("conclusion"))
                            ),
                            detail: format!("url: {}", fmt(j.get("url"))),
                            value: v,
                        }
                    })
                    .collect(),
            );
        }
        _ => {}
    }
    list.phase = LoadPhase::Ready;
    list.error = None;
}

fn apply_detail_payload(detail: &mut FDetailScreen, kind: LoadKind, payload: &Value) {
    match kind {
        LoadKind::PrStatus => {
            if let Some(pr) = payload.get("pr").and_then(|p| p.as_object()) {
                let pr_v = Value::Object(pr.clone());
                detail.lines = format_pr_view(&pr_v);
                detail.pr_snapshot = Some(pr_v.clone());
                detail.opened_updated_at =
                    jstr(pr_v.get("updatedAt").or_else(|| pr_v.get("updated_at")));
            } else if bool_at(payload, "detached") {
                let branch = jstr(payload.get("branch")).unwrap_or_else(|| "HEAD".into());
                detail.lines = vec![
                    format!("branch: {branch} (detached)"),
                    String::new(),
                    "(detached HEAD — no branch PR lookup)".to_string(),
                ];
            } else {
                let branch = jstr(payload.get("branch")).unwrap_or_else(|| "unknown".into());
                detail.lines = vec![
                    format!("branch: {branch}"),
                    String::new(),
                    "(no open PR for the current branch)".to_string(),
                ];
            }
        }
        LoadKind::PrDetail => {
            if let Some(pr) = payload.get("pr").and_then(|p| p.as_object()) {
                let pr_v = Value::Object(pr.clone());
                let mut lines = format_pr_view(&pr_v);
                detail.opened_updated_at =
                    jstr(pr_v.get("updatedAt").or_else(|| pr_v.get("updated_at")));
                detail.pr_snapshot = Some(pr_v);
                if let Some(sha) = &detail.opened_head_sha {
                    lines.push(format!("head sha (expect-head): {sha}"));
                } else {
                    lines.push(
                        "head sha: unavailable (reviews read failed — mutations will revalidate)"
                            .to_string(),
                    );
                }
                detail.lines = lines;
            } else {
                detail.phase = DetailPhase::Error;
                detail.error = Some((None, "pr view returned no PR payload".to_string()));
                return;
            }
        }
        LoadKind::IssueDetail => {
            if let Some(issue) = payload.get("issue").and_then(|p| p.as_object()) {
                let issue_v = Value::Object(issue.clone());
                detail.lines = format_issue_view(&issue_v);
                detail.opened_updated_at = jstr(
                    issue_v
                        .get("updated_at")
                        .or_else(|| issue_v.get("updatedAt")),
                );
                detail.issue_snapshot = Some(issue_v);
            } else {
                detail.phase = DetailPhase::Error;
                detail.error = Some((None, "issue view returned no issue payload".to_string()));
                return;
            }
        }
        _ => {}
    }
    detail.phase = DetailPhase::Ready;
    detail.error = None;
}

fn comment_item(c: &Map<String, Value>, is_line: bool) -> FItem {
    let mut obj = c.clone();
    obj.insert(
        "_kind".to_string(),
        Value::String(if is_line { "line" } else { "issue" }.to_string()),
    );
    let v = Value::Object(obj);
    let id = fmt(v.get("id"));
    let author = author_of(v.get("author"));
    let when = fmt(v.get("created_at"));
    let body = jstr(v.get("body")).unwrap_or_else(|| "(no body)".into());
    let label = if is_line {
        format!(
            "L{} {} — #{} {} {}",
            fmt(v.get("line")),
            fmt(v.get("path")),
            id,
            author,
            when
        )
    } else {
        format!("#{id} {author} (conversation) {when}")
    };
    FItem {
        label,
        detail: body.chars().take(400).collect(),
        value: v,
    }
}

fn forge_error_lines(kind: Option<&str>, message: &str) -> Vec<String> {
    let mut lines = vec![format!(
        "forge error{}: {}",
        kind.map(|k| format!(" ({k})")).unwrap_or_default(),
        message
    )];
    if kind == Some("gh_missing") {
        lines.push(
            "the host `gh` CLI is required for the forge surface (gh >= 2.81.0 + gh auth login)"
                .to_string(),
        );
        lines.push("CLI fallback: git pr list <pane_id>".to_string());
    } else if kind.is_some() {
        lines.push("CLI fallback: git <cmd> — see plugin README".to_string());
    }
    lines
}

// ===========================================================================
// Rendering
// ===========================================================================

fn clip_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut w = 0usize;
        for word in raw.split_whitespace() {
            let ww = UnicodeWidthStr::width(word);
            if w > 0 && w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                w += 1;
            }
            if ww > width {
                // Hard-split words longer than width.
                let mut rest = word;
                while UnicodeWidthStr::width(rest) > width {
                    let mut cut = 0usize;
                    let mut cw = 0usize;
                    for (i, ch) in rest.char_indices() {
                        let cwid = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if cw + cwid > width {
                            break;
                        }
                        cw += cwid;
                        cut = i + ch.len_utf8();
                    }
                    if cut == 0 {
                        cut = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                    }
                    out.push(rest[..cut].to_string());
                    rest = &rest[cut..];
                }
                line = rest.to_string();
                w = UnicodeWidthStr::width(rest);
            } else {
                line.push_str(word);
                w += ww;
            }
        }
        out.push(line);
    }
    out
}

fn render_scrollable(
    title: &str,
    source: &[String],
    scroll: &mut usize,
    width: usize,
    height: usize,
    footer: &str,
) -> Vec<Line<'static>> {
    let has_title = !title.is_empty();
    let body_h = height.saturating_sub(if has_title { 3 } else { 1 }).max(1);
    let wrap_w = width.saturating_sub(2).max(1);
    let mut wrapped: Vec<String> = Vec::new();
    for l in source {
        wrapped.extend(wrap_text(l, wrap_w));
    }
    let max_scroll = wrapped.len().saturating_sub(body_h);
    *scroll = (*scroll).min(max_scroll);
    let view: Vec<&String> = wrapped.iter().skip(*scroll).take(body_h).collect();
    let mut lines: Vec<Line> = Vec::new();
    if has_title {
        lines.push(Line::from(Span::styled(
            clip_text(&sanitize_display(title), width),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(width),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for v in view {
        lines.push(Line::from(clip_text(&sanitize_display(v), width)));
    }
    while lines.len() < height.saturating_sub(1) {
        lines.push(Line::from(""));
    }
    let pos = if wrapped.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", (*scroll + 1).min(wrapped.len()), wrapped.len())
    };
    let footer_clip = clip_text(
        &sanitize_display(footer),
        width.saturating_sub(UnicodeWidthStr::width(pos.as_str()) + 2),
    );
    let pad = width.saturating_sub(
        UnicodeWidthStr::width(footer_clip.as_str()) + UnicodeWidthStr::width(pos.as_str()),
    );
    lines.push(Line::from(vec![
        Span::styled(footer_clip, Style::default().add_modifier(Modifier::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(pos, Style::default().add_modifier(Modifier::DIM)),
    ]));
    lines.truncate(height);
    lines
}

fn render_forge(app: &mut ForgeTui, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(sanitize_display(&format!(
            "Git Forge v{PLUGIN_VERSION} — {}",
            app.target_label
        )))
        .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let width = chunks[1].width as usize;
    let height = chunks[1].height as usize;

    match app.current_mut() {
        FScreen::Home { cursor } => {
            let items = [
                (
                    "Pull requests",
                    "list / view / review / comment / merge (gh-backed)",
                ),
                (
                    "Issues",
                    "list / view / create / edit / comment / close (gh-backed)",
                ),
                (
                    "PR for current branch",
                    "open PR of the pinned pane's current branch, or honest none",
                ),
            ];
            let mut lines: Vec<Line> = Vec::new();
            for (i, (label, detail)) in items.iter().enumerate() {
                let sel = i == *cursor;
                let style = if sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(if sel { "❯ " } else { "  " }, style),
                    Span::styled(sanitize_display(label), style),
                    Span::styled(
                        format!("  {detail}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
            while lines.len() < height.saturating_sub(1) {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                "j/k move · enter open · p pane · o repo · q quit",
                Style::default().add_modifier(Modifier::DIM),
            )));
            frame.render_widget(Paragraph::new(lines), chunks[1]);
        }
        FScreen::List(list) => render_forge_list(list, frame, chunks[1], width, height),
        FScreen::Detail(d) => {
            let source: Vec<String> = match d.phase {
                DetailPhase::Loading => vec!["loading…".to_string()],
                DetailPhase::Error => {
                    let (kind, msg) = d
                        .error
                        .clone()
                        .unwrap_or((None, "unknown error".to_string()));
                    forge_error_lines(kind.as_deref(), &msg)
                }
                DetailPhase::Ready => d.lines.clone(),
            };
            let footer = if d.phase == DetailPhase::Error {
                "R — retry · esc — back".to_string()
            } else {
                d.footer.clone()
            };
            let lines = render_scrollable(&d.title, &source, &mut d.scroll, width, height, &footer);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
        }
        FScreen::Prompt(p) => {
            let lines = vec![
                Line::from(Span::styled(
                    sanitize_display(&p.label),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("> {}█", sanitize_display(&p.draft))),
                Line::from(Span::styled(
                    "enter submit · esc cancel",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title("git forge")),
                chunks[1],
            );
        }
        FScreen::Confirm(c) => {
            let mut text = vec![
                Line::from(Span::styled(
                    sanitize_display(&c.title),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            for l in &c.target {
                text.push(Line::from(sanitize_display(l)));
            }
            text.push(Line::from(""));
            text.push(Line::from(sanitize_display(&c.consequence)));
            let hint_style = if c.danger {
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            text.push(Line::from(Span::styled(
                format!(
                    "enter/y {} · esc/{} cancel",
                    c.confirm_label, c.cancel_label
                ),
                hint_style,
            )));
            let area = Rect {
                x: chunks[1].x + chunks[1].width / 4,
                y: chunks[1].y + chunks[1].height / 4,
                width: chunks[1].width / 2,
                height: chunks[1].height / 2,
            };
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("confirm")),
                area,
            );
        }
        FScreen::Select(s) => {
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                sanitize_display(&s.title),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            for (i, item) in s.items.iter().enumerate() {
                let sel = i == s.cursor;
                let style = if sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(if sel { "❯ " } else { "  " }, style),
                    Span::styled(sanitize_display(&item.label), style),
                    Span::styled(
                        format!("  {}", sanitize_display(&item.detail)),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "j/k move · enter choose · esc cancel",
                Style::default().add_modifier(Modifier::DIM),
            )));
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("select")),
                chunks[1],
            );
        }
    }

    let busy = app
        .busy_label()
        .map(|l| format!(" … {l}"))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            " {}{}",
            sanitize_display(&app.status_left),
            sanitize_display(&busy)
        ))
        .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_forge_list(
    list: &mut FListScreen,
    frame: &mut ratatui::Frame,
    area: Rect,
    width: usize,
    height: usize,
) {
    match list.phase {
        LoadPhase::Loading => {
            let label = match list.kind {
                ListKind::Prs => format!("loading pull requests ({})…", list.state),
                ListKind::Issues => format!("loading issues ({})…", list.state),
                ListKind::Comments => {
                    format!("loading comments for pr #{}…", list.pr_number.unwrap_or(0))
                }
                ListKind::Logs => {
                    if list.log_mode == LogMode::Jobs {
                        format!(
                            "loading jobs for run #{}…",
                            list.current_run_id.clone().unwrap_or_else(|| "?".into())
                        )
                    } else {
                        format!("loading runs for pr #{}…", list.pr_number.unwrap_or(0))
                    }
                }
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default().add_modifier(Modifier::DIM),
                ))),
                area,
            );
            return;
        }
        LoadPhase::Error => {
            let (kind, msg) = list
                .error
                .clone()
                .unwrap_or((None, "unknown error".to_string()));
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                "failed to load",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))];
            for l in forge_error_lines(kind.as_deref(), &msg) {
                lines.push(Line::from(Span::styled(
                    clip_text(&sanitize_display(&l), width),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(Span::styled(
                "R — retry · p — set pane · esc — back",
                Style::default().add_modifier(Modifier::DIM),
            )));
            while lines.len() < height {
                lines.push(Line::from(""));
            }
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
        LoadPhase::Ready => {}
    }

    let filtered = list.filtered_indices();
    if filtered.is_empty() {
        let empty = match list.kind {
            ListKind::Prs => "no pull requests",
            ListKind::Issues => "no issues",
            ListKind::Comments => "no comments",
            ListKind::Logs => "no entries",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "  (empty)",
                    Style::default().add_modifier(Modifier::DIM),
                )),
                Line::from(empty),
            ]),
            area,
        );
        return;
    }
    let chrome = if list.filter_active || !list.committed_filter.is_empty() {
        2
    } else {
        1
    };
    let visible = height.saturating_sub(chrome).max(1);
    let n = filtered.len();
    if list.cursor < list.scroll_top {
        list.scroll_top = list.cursor;
    }
    if list.cursor >= list.scroll_top + visible {
        list.scroll_top = list.cursor + 1 - visible;
    }
    list.scroll_top = list.scroll_top.min(n.saturating_sub(visible));

    let mut lines: Vec<Line> = Vec::new();
    if list.filter_active || !list.committed_filter.is_empty() {
        let q = if list.filter_active {
            list.filter_draft.clone()
        } else {
            list.committed_filter.clone()
        };
        lines.push(Line::from(Span::styled(
            format!(" /{}", sanitize_display(&q)),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for i in 0..visible.min(n.saturating_sub(list.scroll_top)) {
        let idx = filtered[list.scroll_top + i];
        let item = &list.items[idx];
        let is_sel = list.scroll_top + i == list.cursor;
        let style = if is_sel {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(if is_sel { "❯ " } else { "  " }, style),
            Span::styled(
                clip_text(&sanitize_display(&item.label), width.saturating_sub(2)),
                style,
            ),
        ]));
    }
    if n > visible {
        lines.push(Line::from(Span::styled(
            format!("  {}/{}", list.cursor + 1, n),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    while lines.len() < height.saturating_sub(1) {
        lines.push(Line::from(""));
    }
    let hint = match list.kind {
        ListKind::Prs => "enter open · s state · n create · / filter · R refresh · esc back",
        ListKind::Issues => {
            "enter open · s state · l labels · n create · / filter · R refresh · esc back"
        }
        ListKind::Comments => "enter view · r reply (line) · / filter · R refresh · esc back",
        ListKind::Logs => "enter drill down · R refresh · esc back",
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::DIM),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

// ===========================================================================
// openExternal port (argv array + scrubbed env, safety.ts semantics)
// ===========================================================================

fn open_external(target: &str) -> Result<(), String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err("invalid-target".to_string());
    }
    let is_url = trimmed.contains("://");
    let is_path = Path::new(trimmed).is_absolute();
    if !is_url && !is_path {
        return Err("invalid-target".to_string());
    }
    #[cfg(target_os = "macos")]
    let argv: Vec<String> = vec!["open".to_string(), trimmed.to_string()];
    #[cfg(target_os = "windows")]
    let argv: Vec<String> = vec![
        "cmd.exe".to_string(),
        "/c".to_string(),
        "start".to_string(),
        String::new(),
        trimmed.to_string(),
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let argv: Vec<String> = vec!["xdg-open".to_string(), trimmed.to_string()];

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    for key in ["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env(
        "LANG",
        std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
    );
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn-failed: {e}"))
}

// ===========================================================================
// Entry
// ===========================================================================

const FORGE_USAGE: &str = r#"rebuildup.git forge TUI — GitHub PR/issue browser (herdr-plugins.tui/v1)

usage:
  git tui forge [main|open] [--cwd DIR]
  git tui forge --help

commands:
  main (default)  interactive PR/issue browser pane entrypoint
  open            open-forge action helper: plugin.pane.open (tab) for the forge TUI
"#;

/// `git tui forge ...` entrypoint; returns the process exit code.
pub fn run_forge_tui(args: &[String]) -> i32 {
    let mut command = "main".to_string();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "open" | "main" => command = a.clone(),
            "help" | "--help" | "-h" => {
                print!("{FORGE_USAGE}");
                return 0;
            }
            "--cwd" => {
                i += 1; // value ignored: target comes from pinned context
            }
            _ if a.starts_with("--cwd=") => {}
            _ => {
                eprintln!("git-tui-forge: unknown argument: {a}");
                return 2;
            }
        }
        i += 1;
    }
    if command == "open" {
        return run_open();
    }
    forge_main()
}

fn forge_main() -> i32 {
    let pinned = resolve_pinned_target();

    if !io::stdin().is_terminal() {
        return render_plain_list(&pinned);
    }

    // Capability document for mutation gating (fail closed on error).
    let capability = run_cli(&["capability".to_string()], CAPABILITY_TIMEOUT_MS).ok();

    let mut tui = ForgeTui::new(pinned);
    tui.capability = capability;
    if tui.forge.pane_id.is_none() {
        tui.set_status("no pane pinned — press p to set a pane id (its cwd resolves the repo)");
    }

    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("git-tui-forge: could not enable raw mode");
        return 1;
    }
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("git-tui-forge: {err}");
            return 1;
        }
    };

    let result = (|| -> io::Result<()> {
        loop {
            tui.poll_tasks();
            terminal.draw(|frame| render_forge(&mut tui, frame))?;
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == event::KeyEventKind::Press && !tui.handle_key(key) {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();

    let _ = terminal.show_cursor();
    crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen).ok();
    crossterm::terminal::disable_raw_mode().ok();

    match result {
        Ok(()) => {
            let _ = writeln!(io::stderr(), "git forge tui exited");
            0
        }
        Err(err) => {
            eprintln!("git-tui-forge: {err}");
            1
        }
    }
}

/// Plain-text fallback for non-TTY stdin (pipes/CI): honest, read-only.
fn render_plain_list(target: &ForgePinned) -> i32 {
    let Some(pane) = &target.origin_pane_id else {
        println!(
            "git forge tui: no pane pinned — launch from a pane context (or set HERDR_TUI_PANE_ID)\n\
             hint: run inside a herdr pane (or a TTY) for the interactive TUI; CLI: git pr list <pane_id>"
        );
        return 0;
    };
    let mut failures = 0;
    match run_cli(
        &[
            "pr".into(),
            "list".into(),
            pane.clone(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            "30".into(),
        ],
        CLI_TIMEOUT_MS,
    ) {
        Ok(payload) => {
            let repo = fmt(payload.get("repo"));
            println!("prs ({repo}):");
            let prs = as_records(payload.get("prs"));
            if prs.is_empty() {
                println!("  (none)");
            }
            for pr in &prs {
                let v = Value::Object((*pr).clone());
                let draft = if bool_at(&v, "isDraft") || bool_at(&v, "is_draft") {
                    " [draft]"
                } else {
                    ""
                };
                println!(
                    "  #{} {} ({}){}",
                    fmt(v.get("number")),
                    fmt(v.get("title")),
                    fmt(v.get("state")).to_lowercase(),
                    draft
                );
            }
        }
        Err(err) => {
            failures += 1;
            eprintln!("pr list failed ({}): {}", err.kind, err.message);
        }
    }
    match run_cli(
        &[
            "issue".into(),
            "list".into(),
            pane.clone(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            "30".into(),
        ],
        CLI_TIMEOUT_MS,
    ) {
        Ok(payload) => {
            let repo = fmt(payload.get("repo"));
            println!("issues ({repo}):");
            let issues = as_records(payload.get("issues"));
            if issues.is_empty() {
                println!("  (none)");
            }
            for issue in &issues {
                let v = Value::Object((*issue).clone());
                println!(
                    "  #{} {} ({})",
                    fmt(v.get("number")),
                    fmt(v.get("title")),
                    fmt(v.get("state")).to_lowercase()
                );
            }
        }
        Err(err) => {
            failures += 1;
            eprintln!("issue list failed ({}): {}", err.kind, err.message);
        }
    }
    println!("hint: run inside a herdr pane (or a TTY) for the interactive TUI; CLI: git <cmd>");
    if failures > 0 {
        1
    } else {
        0
    }
}
