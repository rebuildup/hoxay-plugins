//! `rebuildup.git` CLI — Git status/diff plus local mutation contract
//! (`herdr-plugins.git/v1`) shared by the herdr TUI (via manifest
//! actions), agents, and external clients such as herdr-t3. Rust port of
//! `plugins/git/src/cli.ts`.
//!
//! All commands emit a single JSON document on stdout; diagnostics go to
//! stderr. Exit code 0 on success, 1 on handled errors, 2 on usage errors.
//!
//! Pane selection: explicit <pane_id> argument, else HERDR_PANE_ID, else
//! HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id (manifest action context).
//! Ambient pane ids are verified against the resolved session's snapshot;
//! session/socket targeting follows `resolveTarget` — explicit flags beat
//! ambient HERDR_* and contradictions fail closed.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::Utc;
use hoxay_herdr_client::commands::{self, CallOptions, CommandError};
use hoxay_herdr_client::dirs::plugin_dirs;
use hoxay_herdr_client::snapshot::{PaneInfo, SessionSnapshot};
use hoxay_herdr_client::target::{
    ambient_identity, assert_ambient_pane_identity, AmbientIdentity, TargetResolutionError,
};
use serde_json::{json, Value};

use crate::gh::{self, GhError, GhOptions};
use crate::git::{self, DiffFormat, DiffRequest, GitError, RunOptions};
use crate::inboxlink::{self, InboxCliCallOptions, InboxRootResolution, INBOX_PLUGIN_ID};

const CONTRACT: &str = "herdr-plugins.git/v1";
const PLUGIN_ID: &str = "rebuildup.git";
const PLUGIN_VERSION: &str = "0.7.0";
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_REMOTE_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_DIFF_MAX_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Options / arg parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CliOptions {
    pub command: String,
    pub args: Vec<String>,
    pub session: Option<String>,
    pub socket_path: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Mutating operation: target resolution fails closed with
    /// `target_required` on the implicit default session socket.
    pub mutation: bool,
}

fn take_global_value(args: &[String], i: usize, flag: &str) -> Result<String, RunError> {
    match args.get(i + 1) {
        Some(v) if !v.is_empty() && !v.starts_with('-') => Ok(v.clone()),
        _ => Err(RunError::usage(format!("option {flag} requires a value"))),
    }
}

fn parse_global_timeout(raw: &str) -> Result<u64, RunError> {
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v as u64),
        _ => Err(RunError::usage(format!(
            "invalid --timeout: {raw} (expected a positive number of milliseconds)"
        ))),
    }
}

pub fn parse_args(argv: &[String]) -> Result<CliOptions, RunError> {
    let mut args: Vec<String> = argv.to_vec();
    let command = if args.is_empty() {
        "help".to_string()
    } else {
        args.remove(0)
    };
    let mut options = CliOptions {
        command,
        ..Default::default()
    };
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--session" {
            options.session = Some(take_global_value(&args, i, "--session")?);
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--session=") {
            if v.is_empty() {
                return Err(RunError::usage("option --session requires a value"));
            }
            options.session = Some(v.to_string());
            i += 1;
        } else if arg == "--socket" {
            options.socket_path = Some(take_global_value(&args, i, "--socket")?);
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--socket=") {
            if v.is_empty() {
                return Err(RunError::usage("option --socket requires a value"));
            }
            options.socket_path = Some(v.to_string());
            i += 1;
        } else if arg == "--timeout" {
            options.timeout_ms = Some(parse_global_timeout(&take_global_value(
                &args,
                i,
                "--timeout",
            )?)?);
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--timeout=") {
            options.timeout_ms = Some(parse_global_timeout(v)?);
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    options.args = rest;
    Ok(options)
}

impl CliOptions {
    fn call_options(&self) -> CallOptions {
        CallOptions {
            timeout_ms: self.timeout_ms,
            session: self.session.clone(),
            socket_path: self.socket_path.clone(),
            mutation: self.mutation,
        }
    }

    fn run_options(&self, timeout_ms: u64) -> RunOptions {
        RunOptions {
            timeout_ms: Some(timeout_ms),
            ..Default::default()
        }
    }

    fn gh_options(&self, timeout_ms: u64) -> GhOptions {
        GhOptions {
            timeout_ms: Some(timeout_ms),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Output helpers / error surface
// ---------------------------------------------------------------------------

pub fn emit(payload: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(payload).unwrap_or_default()
    );
}

/// Error surface for command cores: usable both from the process-level
/// CLI dispatch (mapped to stderr doc + exit code) and in-process from
/// the TUI (returned as a value — never exits the host process).
#[derive(Debug)]
pub enum RunError {
    /// Target-resolution failure with its structured code/exit mapping.
    Target(TargetResolutionError),
    /// Usage error (exit 2 at the CLI boundary).
    Usage(String),
    /// Handled runtime error (exit 1); `kind` carries the git/gh error
    /// taxonomy (git_error, stale_head, …) when present.
    Other {
        message: String,
        kind: Option<String>,
    },
}

impl RunError {
    pub fn usage(message: impl Into<String>) -> Self {
        RunError::Usage(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        RunError::Other {
            message: message.into(),
            kind: None,
        }
    }

    pub fn other_kind(kind: &str, message: impl Into<String>) -> Self {
        RunError::Other {
            message: message.into(),
            kind: Some(kind.to_string()),
        }
    }

    /// Exit code the CLI boundary would use for this error.
    pub fn code(&self) -> i32 {
        match self {
            RunError::Target(t) => {
                if t.usage {
                    2
                } else {
                    1
                }
            }
            RunError::Usage(_) => 2,
            RunError::Other { .. } => 1,
        }
    }

    /// The same JSON error document `exit()` writes to stderr — lets
    /// in-process callers (the TUI) surface identical diagnostics.
    pub fn stderr_doc(&self) -> String {
        let error = match self {
            RunError::Target(t) => json!({
                "contract": CONTRACT,
                "ok": false,
                "error": { "code": t.code.as_str(), "message": t.message },
            }),
            RunError::Usage(m) => json!({
                "contract": CONTRACT,
                "ok": false,
                "error": { "code": "usage", "message": m },
            }),
            RunError::Other { message, kind } => {
                let mut e = json!({
                    "contract": CONTRACT,
                    "ok": false,
                    "error": { "code": "error", "message": message },
                });
                if let Some(k) = kind {
                    e["error"]["kind"] = json!(k);
                }
                e
            }
        };
        serde_json::to_string(&error).unwrap_or_default()
    }

    /// CLI boundary: emit the error document and exit with the mapped code.
    pub fn exit(self) -> ! {
        eprintln!("{}", self.stderr_doc());
        std::process::exit(self.code());
    }
}

impl From<CommandError> for RunError {
    fn from(e: CommandError) -> Self {
        match e {
            CommandError::Target(t) => RunError::Target(t),
            CommandError::Client(c) => RunError::other(c.to_string()),
        }
    }
}

impl From<TargetResolutionError> for RunError {
    fn from(e: TargetResolutionError) -> Self {
        RunError::Target(e)
    }
}

impl From<GitError> for RunError {
    fn from(e: GitError) -> Self {
        RunError::other_kind(e.kind.as_str(), e.message)
    }
}

impl From<GhError> for RunError {
    fn from(e: GhError) -> Self {
        RunError::other_kind(e.kind.as_str(), e.message)
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Target(t) => write!(f, "{}", t),
            RunError::Usage(m) => write!(f, "{m}"),
            RunError::Other { message, .. } => write!(f, "{message}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Flag helpers (mirror cli.ts)
// ---------------------------------------------------------------------------

/// First value of a flag, in either `--name value` or `--name=value` form.
/// Tokens after a `--` separator are positional and never match.
fn flag_value(args: &[String], name: &str) -> Option<String> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if arg == name {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

/// All values of a repeatable value flag (`--path a --path=b`).
fn flag_values(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if arg == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
        } else if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
            out.push(v.to_string());
        }
    }
    out
}

fn has_flag(args: &[String], name: &str) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == name {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Boolean,
    Value,
}

/// Strict option check: every `-`-prefixed token before a `--` separator
/// must be a whitelisted flag. Unknown options, `--flag=value` on boolean
/// flags, and value flags with a missing/option-like value are usage
/// errors (exit 2). A lone `-` is a positional, not an option.
fn validate_flags(args: &[String], spec: &HashMap<String, FlagKind>) -> Result<(), RunError> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            break;
        }
        if arg == "-" || !arg.starts_with('-') {
            i += 1;
            continue;
        }
        let eq = arg.find('=');
        let name = match eq {
            Some(e) => &arg[..e],
            None => arg.as_str(),
        };
        let Some(kind) = spec.get(name) else {
            return Err(RunError::usage(format!("unknown option: {arg}")));
        };
        if *kind == FlagKind::Boolean {
            if eq.is_some() {
                return Err(RunError::usage(format!(
                    "option {name} does not take a value"
                )));
            }
            i += 1;
            continue;
        }
        if eq.is_some() {
            i += 1;
            continue;
        }
        let next = args.get(i + 1);
        let bad = match next {
            None => true,
            Some(n) => {
                n == "--"
                    || n.starts_with("--")
                    || (n.len() > 1 && n.starts_with('-') && spec.contains_key(n.as_str()))
            }
        };
        if bad {
            return Err(RunError::usage(format!("option {name} requires a value")));
        }
        i += 2;
    }
    Ok(())
}

fn flags_spec(pairs: &[(&str, FlagKind)]) -> HashMap<String, FlagKind> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// Positional args, skipping flag values. Everything after `--` counts.
fn positionals(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut after_separator = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if after_separator {
            out.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            after_separator = true;
            i += 1;
            continue;
        }
        if value_flags.contains(&arg.as_str()) {
            i += 2;
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            i += 1;
            continue;
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

/// More positionals than the command accepts is a usage error.
fn assert_max_positionals(
    args: &[String],
    value_flags: &[&str],
    max: usize,
    usage: &str,
) -> Result<(), RunError> {
    let pos = positionals(args, value_flags);
    if pos.len() > max {
        return Err(RunError::usage(format!(
            "{usage}: unexpected argument {}",
            serde_json::to_string(&pos[max]).unwrap_or_default()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config / pane selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct PluginConfig {
    timeout_ms: Option<u64>,
    remote_timeout_ms: Option<u64>,
    diff_max_bytes: Option<usize>,
    /// Explicit rebuildup.inbox plugin root for `issue link|unlink|links`.
    inbox_plugin_root: Option<String>,
}

fn load_config() -> PluginConfig {
    let file = plugin_dirs(PLUGIN_ID, None, None)
        .config_dir
        .join("config.json");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return PluginConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return PluginConfig::default();
    };
    let positive_u64 =
        |key: &str| -> Option<u64> { parsed.get(key).and_then(|v| v.as_u64()).filter(|v| *v > 0) };
    PluginConfig {
        timeout_ms: positive_u64("timeoutMs"),
        remote_timeout_ms: positive_u64("remoteTimeoutMs"),
        diff_max_bytes: positive_u64("diffMaxBytes").map(|v| v as usize),
        inbox_plugin_root: parsed
            .get("inboxPluginRoot")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    }
}

fn effective_timeout_ms(options: &CliOptions, config: &PluginConfig) -> u64 {
    options
        .timeout_ms
        .or(config.timeout_ms)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

/// Network-facing git ops (fetch/push) get a longer default budget.
fn effective_remote_timeout_ms(options: &CliOptions, config: &PluginConfig) -> u64 {
    options
        .timeout_ms
        .or(config.remote_timeout_ms)
        .unwrap_or(DEFAULT_REMOTE_TIMEOUT_MS)
}

struct PaneSelection {
    pane_id: String,
    /// Non-null when the pane id came from ambient env/context.
    ambient: Option<AmbientIdentity>,
}

fn ambient() -> Result<AmbientIdentity, RunError> {
    Ok(ambient_identity()?)
}

/// Resolve the target pane. An explicit positional wins; otherwise the
/// ambient identity bundle is used and kept for the post-snapshot
/// consistency check.
fn resolve_pane_selection(
    args: &[String],
    value_flags: &[&str],
) -> Result<PaneSelection, RunError> {
    if let Some(positional) = positionals(args, value_flags).into_iter().next() {
        return Ok(PaneSelection {
            pane_id: positional,
            ambient: None,
        });
    }
    let identity = ambient()?;
    match &identity.pane_id {
        Some(pane_id) => Ok(PaneSelection {
            pane_id: pane_id.clone(),
            ambient: Some(identity),
        }),
        None => Err(RunError::usage(
            "pane id required (argument, HERDR_PANE_ID, or plugin context)",
        )),
    }
}

/// findPane + ambient identity verification for ambient-sourced pane ids.
fn find_pane_checked<'a>(
    snapshot: &'a SessionSnapshot,
    selection: &PaneSelection,
) -> Result<&'a PaneInfo, RunError> {
    let pane = snapshot
        .panes
        .iter()
        .find(|p| p.pane_id == selection.pane_id)
        .ok_or_else(|| {
            RunError::other_kind(
                "pane_not_found",
                format!("pane not found in session snapshot: {}", selection.pane_id),
            )
        })?;
    if let Some(ambient) = &selection.ambient {
        assert_ambient_pane_identity(pane, ambient)?;
    }
    Ok(pane)
}

fn pane_cwd(pane: &PaneInfo) -> Result<String, RunError> {
    // `foreground_cwd` lives in the flattened extra map.
    let cwd = pane
        .cwd
        .clone()
        .or_else(|| {
            pane.extra
                .get("foreground_cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty());
    match cwd {
        Some(cwd) => Ok(cwd),
        None => Err(RunError::other_kind(
            "no_pane_cwd",
            format!("pane has no cwd in session snapshot: {}", pane.pane_id),
        )),
    }
}

fn generated_at() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn status_payload(pane_id: &str, report: &git::StatusReport) -> Value {
    json!({
        "contract": CONTRACT,
        "command": "status",
        "pane_id": pane_id,
        "generated_at": generated_at(),
        "repo": {
            "root": report.repo.root,
            "git_dir": report.repo.git_dir,
            "git_common_dir": report.repo.git_common_dir,
            "bare": report.repo.bare,
        },
        "branch": {
            "name": report.branch.name,
            "detached": report.branch.detached,
            "upstream": report.branch.upstream,
            "ahead": report.branch.ahead,
            "behind": report.branch.behind,
        },
        "changes": {
            "staged": report.changes.staged,
            "unstaged": report.changes.unstaged,
            "untracked": report.changes.untracked,
            "conflicted": report.changes.conflicted,
        },
        "warnings": [],
    })
}

// ---------------------------------------------------------------------------
// Read commands
// ---------------------------------------------------------------------------

pub fn cmd_status(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 1, "status <pane_id>")?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, &[])?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;
    let timeout = effective_timeout_ms(options, &config);
    let report = git::collect_status(Path::new(&cwd), &options.run_options(timeout))?;
    Ok(status_payload(&pane_id, &report))
}

const DIFF_VALUE_FLAGS: &[&str] = &["--path", "--max-bytes"];

fn diff_flags_spec() -> HashMap<String, FlagKind> {
    flags_spec(&[
        ("--path", FlagKind::Value),
        ("--max-bytes", FlagKind::Value),
        ("--staged", FlagKind::Boolean),
        ("--stat", FlagKind::Boolean),
        ("--name-status", FlagKind::Boolean),
        ("--numstat", FlagKind::Boolean),
    ])
}

pub fn cmd_files(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &flags_spec(&[("--path", FlagKind::Value)]))?;
    assert_max_positionals(&options.args, &["--path"], 1, "files <pane_id>")?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, &["--path"])?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;

    let pathspec = flag_values(&options.args, "--path");
    for p in &pathspec {
        if p.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid --path value (option-like rejected): {p}"
            )));
        }
    }

    let timeout = effective_timeout_ms(options, &config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let result = git::collect_files(
        Path::new(&repo.root),
        &pathspec,
        &options.run_options(timeout),
    )?;
    Ok(json!({
        "contract": CONTRACT,
        "command": "files",
        "pane_id": pane_id,
        "repo_root": repo.root,
        "pathspec": pathspec,
        "files": result.files.iter().map(|f| {
            let mut v = serde_json::Map::new();
            v.insert("path".into(), json!(f.path));
            v.insert("oldPath".into(), f.old_path.clone().map(Value::String).unwrap_or(Value::Null));
            v.insert("index".into(), json!(f.index));
            v.insert("worktree".into(), json!(f.worktree));
            v.insert("kind".into(), json!(f.kind.as_str()));
            v.insert("staged".into(), json!(f.staged));
            v.insert("unstaged".into(), json!(f.unstaged));
            v.insert("binary".into(), f.binary.map(Value::Bool).unwrap_or(Value::Null));
            Value::Object(v)
        }).collect::<Vec<_>>(),
        "generated_at": generated_at(),
    }))
}

pub fn cmd_diff(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &diff_flags_spec())?;
    assert_max_positionals(
        &options.args,
        DIFF_VALUE_FLAGS,
        3,
        "diff <pane_id> [rev [rev]]",
    )?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, DIFF_VALUE_FLAGS)?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;

    let pos = positionals(&options.args, DIFF_VALUE_FLAGS);
    let revisions: Vec<String> = pos.iter().skip(1).cloned().collect();
    for rev in &revisions {
        if rev.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid revision (option-like value rejected): {rev}"
            )));
        }
    }
    if revisions.len() > 2 {
        return Err(RunError::usage("diff accepts at most two revisions"));
    }

    let format_flags: Vec<&str> = ["--stat", "--name-status", "--numstat"]
        .iter()
        .filter(|f| has_flag(&options.args, f))
        .copied()
        .collect();
    if format_flags.len() > 1 {
        return Err(RunError::usage(format!(
            "diff accepts only one output format flag (got {})",
            format_flags.join(", ")
        )));
    }
    let format: Option<DiffFormat> = match format_flags.first() {
        Some(&"--stat") => Some(DiffFormat::Stat),
        Some(&"--name-status") => Some(DiffFormat::NameStatus),
        Some(&"--numstat") => Some(DiffFormat::Numstat),
        _ => None,
    };

    let paths = flag_values(&options.args, "--path");
    for p in &paths {
        if p.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid --path value (option-like rejected): {p}"
            )));
        }
    }

    let max_bytes_raw = flag_value(&options.args, "--max-bytes");
    let mut max_bytes: Option<usize> = None;
    if let Some(raw) = &max_bytes_raw {
        match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => max_bytes = Some(v as usize),
            _ => return Err(RunError::usage(format!("invalid --max-bytes: {raw}"))),
        }
    }
    let max_bytes = max_bytes
        .or(config.diff_max_bytes)
        .unwrap_or(DEFAULT_DIFF_MAX_BYTES);

    let timeout = effective_timeout_ms(options, &config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let request = DiffRequest {
        staged: has_flag(&options.args, "--staged"),
        stat: has_flag(&options.args, "--stat"),
        format,
        revisions: revisions.clone(),
        paths: paths.clone(),
        max_bytes: Some(max_bytes),
    };
    let result = git::collect_diff(
        Path::new(&repo.root),
        &request,
        &options.run_options(timeout),
    )?;
    let staged = request.staged;
    let stat_flag = request.stat;
    let format_str = format.map(|f| f.as_str()).unwrap_or("patch");
    Ok(json!({
        "contract": CONTRACT,
        "command": "diff",
        "pane_id": pane_id,
        "repo_root": repo.root,
        "staged": staged,
        "stat": stat_flag,
        "revisions": revisions,
        "format": format_str,
        "output": if format.is_none() { json!(result.output) } else { Value::Null },
        "stat_output": if format == Some(DiffFormat::Stat) { json!(result.output) } else { Value::Null },
        "name_status_output": if format == Some(DiffFormat::NameStatus) { json!(result.output) } else { Value::Null },
        "numstat_output": if format == Some(DiffFormat::Numstat) { json!(result.output) } else { Value::Null },
        "paths": paths,
        "truncated": result.truncated,
        "bytes": result.bytes,
        "generated_at": generated_at(),
    }))
}

const LOG_VALUE_FLAGS: &[&str] = &[
    "--limit", "--skip", "--author", "--grep", "--since", "--until", "--path",
];

fn log_flags_spec() -> HashMap<String, FlagKind> {
    flags_spec(&[
        ("--limit", FlagKind::Value),
        ("--skip", FlagKind::Value),
        ("--author", FlagKind::Value),
        ("--grep", FlagKind::Value),
        ("--since", FlagKind::Value),
        ("--until", FlagKind::Value),
        ("--path", FlagKind::Value),
        ("--all", FlagKind::Boolean),
        ("--first-parent", FlagKind::Boolean),
    ])
}

fn positive_int_flag(args: &[String], name: &str) -> Result<Option<i64>, RunError> {
    let Some(raw) = flag_value(args, name) else {
        return Ok(None);
    };
    match raw.parse::<i64>() {
        Ok(v) if v >= 0 => Ok(Some(v)),
        _ => Err(RunError::usage(format!("invalid {name}: {raw}"))),
    }
}

pub fn cmd_log(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &log_flags_spec())?;
    assert_max_positionals(&options.args, LOG_VALUE_FLAGS, 1, "log <pane_id>")?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, LOG_VALUE_FLAGS)?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;

    let pathspec = flag_values(&options.args, "--path");
    for p in &pathspec {
        if p.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid --path value (option-like rejected): {p}"
            )));
        }
    }
    let limit = positive_int_flag(&options.args, "--limit")?;
    if limit == Some(0) {
        return Err(RunError::usage("--limit must be >= 1"));
    }
    let skip = positive_int_flag(&options.args, "--skip")?;

    let timeout = effective_timeout_ms(options, &config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let request = git::LogRequest {
        limit,
        skip,
        all: has_flag(&options.args, "--all"),
        first_parent: has_flag(&options.args, "--first-parent"),
        author: flag_value(&options.args, "--author"),
        grep: flag_value(&options.args, "--grep"),
        since: flag_value(&options.args, "--since"),
        until: flag_value(&options.args, "--until"),
        pathspec: pathspec.clone(),
    };
    let result = git::collect_log(
        Path::new(&repo.root),
        &request,
        &options.run_options(timeout),
    )?;
    Ok(json!({
        "contract": CONTRACT,
        "command": "log",
        "pane_id": pane_id,
        "repo_root": repo.root,
        "limit": limit.unwrap_or(50),
        "skip": skip.unwrap_or(0),
        "commits": result.commits.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
        "has_more": result.has_more,
        "generated_at": generated_at(),
    }))
}

pub fn cmd_compare(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(
        &options.args,
        &flags_spec(&[("--max-bytes", FlagKind::Value)]),
    )?;
    assert_max_positionals(
        &options.args,
        &["--max-bytes"],
        3,
        "compare <pane_id> [base [head]]",
    )?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, &["--max-bytes"])?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;

    let pos = positionals(&options.args, &["--max-bytes"]);
    let revs: Vec<String> = pos.iter().skip(1).cloned().collect();
    if revs.len() > 2 {
        return Err(RunError::usage(
            "compare accepts at most two revisions (base head)",
        ));
    }
    for rev in &revs {
        if rev.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid revision (option-like value rejected): {rev}"
            )));
        }
    }
    let max_bytes_raw = flag_value(&options.args, "--max-bytes");
    let mut max_bytes: Option<usize> = None;
    if let Some(raw) = &max_bytes_raw {
        match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => max_bytes = Some(v as usize),
            _ => return Err(RunError::usage(format!("invalid --max-bytes: {raw}"))),
        }
    }
    let max_bytes = max_bytes
        .or(config.diff_max_bytes)
        .unwrap_or(DEFAULT_DIFF_MAX_BYTES);

    let timeout = effective_timeout_ms(options, &config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let result = git::collect_compare(
        Path::new(&repo.root),
        revs.first().map(|s| s.as_str()),
        revs.get(1).map(|s| s.as_str()),
        Some(max_bytes),
        &options.run_options(timeout),
    )?;
    Ok(json!({
        "contract": CONTRACT,
        "command": "compare",
        "pane_id": pane_id,
        "repo_root": repo.root,
        "base": result.base,
        "head": result.head,
        "merge_base": result.merge_base,
        "ahead": result.ahead,
        "behind": result.behind,
        "files": result.files.iter().map(|f| json!({
            "status": f.status,
            "path": f.path,
            "oldPath": f.old_path,
        })).collect::<Vec<_>>(),
        "stat": result.stat,
        "patch": result.patch.output,
        "patch_truncated": result.patch.truncated,
        "patch_bytes": result.patch.bytes,
        "generated_at": generated_at(),
    }))
}

/// Resolve pane → snapshot → repo root; shared by all repo commands.
fn repo_root_for_pane(
    options: &mut CliOptions,
    config: &PluginConfig,
    value_flags: &[&str],
) -> Result<(String, String), RunError> {
    let selection = resolve_pane_selection(&options.args, value_flags)?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;
    let timeout = effective_timeout_ms(options, config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    Ok((pane_id, repo.root))
}

fn mutation_payload(command: &str, pane_id: &str, repo_root: &str, fields: Value) -> Value {
    let mut payload = json!({
        "contract": CONTRACT,
        "command": command,
        "pane_id": pane_id,
        "repo_root": repo_root,
        "ok": true,
        "generated_at": generated_at(),
    });
    if let (Some(obj), Some(f)) = (payload.as_object_mut(), fields.as_object()) {
        for (k, v) in f {
            obj.insert(k.clone(), v.clone());
        }
    }
    payload
}

fn pathspec_args(args: &[String]) -> Result<Vec<String>, RunError> {
    let paths = flag_values(args, "--path");
    for p in &paths {
        if p.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid --path value (option-like rejected): {p}"
            )));
        }
    }
    Ok(paths)
}

fn stage_flags_spec() -> HashMap<String, FlagKind> {
    flags_spec(&[("--all", FlagKind::Boolean), ("--path", FlagKind::Value)])
}

pub fn cmd_stage_or_unstage(options: &mut CliOptions, unstaging: bool) -> Result<Value, RunError> {
    options.mutation = true;
    let command = if unstaging { "unstage" } else { "stage" };
    validate_flags(&options.args, &stage_flags_spec())?;
    assert_max_positionals(
        &options.args,
        &["--path"],
        1,
        &format!("{command} <pane_id>"),
    )?;
    let paths = pathspec_args(&options.args)?;
    let all = has_flag(&options.args, "--all");
    if all && !paths.is_empty() {
        return Err(RunError::usage(format!(
            "{command}: --all and --path are mutually exclusive"
        )));
    }
    if !all && paths.is_empty() {
        return Err(RunError::usage(format!(
            "{command} requires --all or at least one --path"
        )));
    }
    let config = load_config();
    let (pane_id, repo_root) = repo_root_for_pane(options, &config, &["--path"])?;
    let timeout = effective_timeout_ms(options, &config);
    let run_opts = options.run_options(timeout);
    let output = if unstaging {
        git::collect_unstage(Path::new(&repo_root), all, &paths, &run_opts)?
    } else {
        git::collect_stage(Path::new(&repo_root), all, &paths, &run_opts)?
    };
    Ok(mutation_payload(
        command,
        &pane_id,
        &repo_root,
        json!({ "all": all, "paths": paths, "output": output }),
    ))
}

pub fn cmd_commit(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    validate_flags(
        &options.args,
        &flags_spec(&[
            ("-m", FlagKind::Value),
            ("--allow-empty", FlagKind::Boolean),
        ]),
    )?;
    assert_max_positionals(&options.args, &["-m"], 1, "commit <pane_id>")?;
    let messages = flag_values(&options.args, "-m");
    if messages.is_empty() {
        return Err(RunError::usage("commit requires at least one -m <message>"));
    }
    if messages.iter().any(|m| m.is_empty()) {
        return Err(RunError::usage("commit -m requires a non-empty message"));
    }
    let allow_empty = has_flag(&options.args, "--allow-empty");
    let config = load_config();
    let (pane_id, repo_root) = repo_root_for_pane(options, &config, &["-m"])?;
    let timeout = effective_timeout_ms(options, &config);
    let (sha, summary) = git::collect_commit(
        Path::new(&repo_root),
        &messages,
        allow_empty,
        &options.run_options(timeout),
    )?;
    Ok(mutation_payload(
        "commit",
        &pane_id,
        &repo_root,
        json!({ "sha": sha, "summary": summary }),
    ))
}

/// `branch create|switch <pane> <name>` and `upstream <pane> <ref>` share
/// the "optional pane positional + required trailing value" shape.
fn resolve_pane_and_value(
    options: &mut CliOptions,
    config: &PluginConfig,
    what: &str,
) -> Result<(String, String, String), RunError> {
    let identity = ambient()?;
    let pos = positionals(&options.args, &[]);
    let (pane_id, value, ambient_sel) = if identity.pane_id.is_some() {
        (
            identity.pane_id.clone().unwrap_or_default(),
            pos.first().cloned(),
            Some(identity),
        )
    } else {
        (
            pos.first().cloned().unwrap_or_default(),
            pos.get(1).cloned(),
            None,
        )
    };
    if pane_id.is_empty() {
        return Err(RunError::usage(format!("pane id required for {what}")));
    }
    let Some(value) = value else {
        return Err(RunError::usage(format!(
            "{what} requires a {} argument",
            if what == "upstream" { "ref" } else { "name" }
        )));
    };
    let what_arg = if what == "upstream" {
        "upstream ref"
    } else {
        "branch name"
    };
    if let Err(e) = git::assert_safe_git_arg(&value, what_arg) {
        return Err(RunError::usage(e.message));
    }
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let selection = PaneSelection {
        pane_id: pane_id.clone(),
        ambient: ambient_sel,
    };
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;
    let timeout = effective_timeout_ms(options, config);
    let repo = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    Ok((pane_id, repo.root, value))
}

pub fn cmd_branch(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    let sub = options.args.first().cloned();
    let sub = match sub.as_deref() {
        Some("create") | Some("switch") => sub.unwrap(),
        _ => return Err(RunError::usage(
            "branch requires a subcommand: create <pane> <name> [--switch] | switch <pane> <name>",
        )),
    };
    let mut rest = options.clone();
    rest.args = options.args[1..].to_vec();
    let spec = if sub == "create" {
        flags_spec(&[("--switch", FlagKind::Boolean)])
    } else {
        HashMap::new()
    };
    validate_flags(&rest.args, &spec)?;
    assert_max_positionals(&rest.args, &[], 2, &format!("branch {sub}"))?;
    let config = load_config();
    let (pane_id, repo_root, name) = resolve_pane_and_value(&mut rest, &config, "branch")?;
    let timeout = effective_timeout_ms(options, &config);
    let run_opts = options.run_options(timeout);
    let output = if sub == "create" {
        git::collect_branch_create(
            Path::new(&repo_root),
            &name,
            has_flag(&rest.args, "--switch"),
            &run_opts,
        )?
    } else {
        git::collect_branch_switch(Path::new(&repo_root), &name, &run_opts)?
    };
    Ok(mutation_payload(
        "branch",
        &pane_id,
        &repo_root,
        json!({ "action": sub, "name": name, "output": output }),
    ))
}

pub fn cmd_upstream(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 2, "upstream <pane_id> <upstream-ref>")?;
    let config = load_config();
    let (pane_id, repo_root, value) = resolve_pane_and_value(options, &config, "upstream")?;
    let timeout = effective_timeout_ms(options, &config);
    let output =
        git::collect_upstream(Path::new(&repo_root), &value, &options.run_options(timeout))?;
    Ok(mutation_payload(
        "upstream",
        &pane_id,
        &repo_root,
        json!({ "upstream": value, "output": output }),
    ))
}

/// Optional second/third positionals after the pane id (remote/refspec).
fn extra_positionals(options: &CliOptions, value_flags: &[&str]) -> Vec<String> {
    positionals(&options.args, value_flags)
        .into_iter()
        .skip(1)
        .collect()
}

pub fn cmd_fetch(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    validate_flags(
        &options.args,
        &flags_spec(&[("--prune", FlagKind::Boolean)]),
    )?;
    assert_max_positionals(&options.args, &[], 2, "fetch <pane_id> [remote]")?;
    let config = load_config();
    let (pane_id, repo_root) = repo_root_for_pane(options, &config, &[])?;
    let extra = extra_positionals(options, &[]);
    if extra.len() > 1 {
        return Err(RunError::usage("fetch accepts at most one remote argument"));
    }
    let remote = extra.first().cloned();
    if let Some(r) = &remote {
        if let Err(e) = git::assert_safe_git_arg(r, "remote") {
            return Err(RunError::usage(e.message));
        }
    }
    let prune = has_flag(&options.args, "--prune");
    let timeout = effective_remote_timeout_ms(options, &config);
    let output = git::collect_fetch(
        Path::new(&repo_root),
        remote.as_deref(),
        prune,
        &options.run_options(timeout),
    )?;
    Ok(mutation_payload(
        "fetch",
        &pane_id,
        &repo_root,
        json!({ "remote": remote, "prune": prune, "output": output }),
    ))
}

pub fn cmd_push(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    validate_flags(
        &options.args,
        &flags_spec(&[
            ("--set-upstream", FlagKind::Boolean),
            ("--force-with-lease", FlagKind::Boolean),
        ]),
    )?;
    assert_max_positionals(&options.args, &[], 3, "push <pane_id> [remote] [refspec]")?;
    let config = load_config();
    let (pane_id, repo_root) = repo_root_for_pane(options, &config, &[])?;
    let extra = extra_positionals(options, &[]);
    if extra.len() > 2 {
        return Err(RunError::usage(
            "push accepts at most remote and refspec arguments",
        ));
    }
    let remote = extra.first().cloned();
    let refspec = extra.get(1).cloned();
    for (value, what) in [(&remote, "remote"), (&refspec, "refspec")] {
        if let Some(v) = value {
            if let Err(e) = git::assert_safe_git_arg(v, what) {
                return Err(RunError::usage(e.message));
            }
        }
    }
    let set_upstream = has_flag(&options.args, "--set-upstream");
    let force_with_lease = has_flag(&options.args, "--force-with-lease");
    let timeout = effective_remote_timeout_ms(options, &config);
    let output = git::collect_push(
        Path::new(&repo_root),
        remote.as_deref(),
        refspec.as_deref(),
        set_upstream,
        force_with_lease,
        &options.run_options(timeout),
    )?;
    Ok(mutation_payload(
        "push",
        &pane_id,
        &repo_root,
        json!({
            "remote": remote,
            "refspec": refspec,
            "set_upstream": set_upstream,
            "force_with_lease": force_with_lease,
            "output": output,
        }),
    ))
}

pub fn cmd_remote(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 1, "remote <pane_id>")?;
    let config = load_config();
    let (pane_id, repo_root) = repo_root_for_pane(options, &config, &[])?;
    let timeout = effective_timeout_ms(options, &config);
    let remotes = git::collect_remotes(Path::new(&repo_root), &options.run_options(timeout))?;
    Ok(json!({
        "contract": CONTRACT,
        "command": "remote",
        "pane_id": pane_id,
        "repo_root": repo_root,
        "remotes": remotes.iter().map(|r| json!({
            "name": r.name,
            "url": r.url,
            "pushUrl": r.push_url,
        })).collect::<Vec<_>>(),
        "generated_at": generated_at(),
    }))
}

// ---------------------------------------------------------------------------
// `pr` — GitHub pull request surface via the host `gh` CLI.
// ---------------------------------------------------------------------------

const PR_VALUE_FLAGS: &[&str] = &[
    "--repo",
    "--limit",
    "--state",
    "--head",
    "--title",
    "--body",
    "--body-file",
    "--base",
    "--run",
    "--job",
    "--lines",
    "--expect-head",
    "--comments-file",
    "--add-reviewer",
    "--remove-reviewer",
];

fn pr_flag_specs() -> HashMap<String, HashMap<String, FlagKind>> {
    let v = FlagKind::Value;
    let b = FlagKind::Boolean;
    let mut m = HashMap::new();
    m.insert(
        "list".to_string(),
        flags_spec(&[("--repo", v), ("--limit", v), ("--state", v), ("--head", v)]),
    );
    m.insert("status".to_string(), flags_spec(&[("--repo", v)]));
    m.insert("view".to_string(), flags_spec(&[("--repo", v)]));
    m.insert(
        "create".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--title", v),
            ("--body", v),
            ("--body-file", v),
            ("--base", v),
            ("--head", v),
            ("--draft", b),
        ]),
    );
    m.insert("reviews".to_string(), flags_spec(&[("--repo", v)]));
    m.insert("comments".to_string(), flags_spec(&[("--repo", v)]));
    m.insert("checks".to_string(), flags_spec(&[("--repo", v)]));
    m.insert(
        "logs".to_string(),
        flags_spec(&[("--repo", v), ("--run", v), ("--job", v), ("--lines", v)]),
    );
    m.insert(
        "review".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--expect-head", v),
            ("--approve", b),
            ("--request-changes", b),
            ("--comment", b),
            ("--body", v),
            ("--body-file", v),
            ("--comments-file", v),
        ]),
    );
    m.insert(
        "comment".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--expect-head", v),
            ("--body", v),
            ("--body-file", v),
        ]),
    );
    m.insert(
        "reply".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--expect-head", v),
            ("--body", v),
            ("--body-file", v),
        ]),
    );
    m.insert(
        "reviewers".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--expect-head", v),
            ("--add-reviewer", v),
            ("--remove-reviewer", v),
        ]),
    );
    m.insert(
        "merge".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--expect-head", v),
            ("--squash", b),
            ("--merge", b),
            ("--rebase", b),
            ("--auto", b),
            ("--disable-auto", b),
        ]),
    );
    m
}

struct PrBodyInput {
    body: Option<String>,
    /// `"--body-file -"` effective value: `-` when the body is piped.
    body_file: Option<String>,
    /// Body text resolved from `--body-file` (file contents or stdin).
    body_text: Option<String>,
}

/// Shared `--body`/`--body-file` resolution for `pr create` and the forge
/// mutations.
fn resolve_pr_body(args: &[String]) -> Result<PrBodyInput, RunError> {
    let body = flag_value(args, "--body");
    let body_file = flag_value(args, "--body-file");
    if body.is_some() && body_file.is_some() {
        return Err(RunError::usage(
            "--body and --body-file are mutually exclusive",
        ));
    }
    let mut body_text: Option<String> = None;
    let mut effective_body_file = body_file.clone();
    if let Some(bf) = &body_file {
        if bf != "-" {
            match std::fs::read_to_string(bf) {
                Ok(text) => {
                    body_text = Some(text);
                    effective_body_file = Some("-".to_string());
                }
                Err(_) => {
                    return Err(RunError::usage(format!("cannot read --body-file: {bf}")));
                }
            }
        } else {
            body_text = Some(read_stdin());
        }
    }
    Ok(PrBodyInput {
        body,
        body_file: effective_body_file,
        body_text,
    })
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

/// Effective body text for `gh api` payloads.
fn pr_body_text(input: &PrBodyInput) -> Option<String> {
    input.body.clone().or_else(|| input.body_text.clone())
}

fn is_owner_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None) => {
            !a.is_empty()
                && !b.is_empty()
                && !a.chars().any(|c| c.is_whitespace())
                && !b.chars().any(|c| c.is_whitespace())
        }
        _ => false,
    }
}

fn valid_repo_flag(repo_flag: Option<&str>) -> Result<(), RunError> {
    if let Some(repo) = repo_flag {
        if repo.starts_with('-') || !is_owner_repo(repo) {
            return Err(RunError::usage(format!(
                "invalid --repo value: {}",
                serde_json::to_string(repo).unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// `pr <action> <pane> [args]` — GitHub PR surface via the host `gh` CLI.
pub fn cmd_pr(options: &mut CliOptions) -> Result<Value, RunError> {
    // This fn is async in TS only because gh calls are async; the Rust
    // domain is synchronous, so the whole body stays sync.
    cmd_pr_inner(options)
}

fn cmd_pr_inner(options: &mut CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let timeout_ms = effective_remote_timeout_ms(options, &config);
    let pos = positionals(&options.args, PR_VALUE_FLAGS);
    let action = pos.first().cloned().unwrap_or_default();
    let specs = pr_flag_specs();
    let Some(spec) = specs.get(&action) else {
        return Err(RunError::usage(
            "pr requires an action: list | status | create | view | reviews | comments | checks | logs | review | comment | reply | reviewers | merge",
        ));
    };
    validate_flags(&options.args, spec)?;
    // Forge-mutating actions need an explicit or consistent ambient
    // target, never the implicit default socket.
    if ["create", "review", "comment", "reply", "reviewers", "merge"].contains(&action.as_str()) {
        options.mutation = true;
    }
    let identity = ambient()?;
    let explicit_pane = pos.get(1).cloned();
    let pane_id = explicit_pane.clone().or_else(|| identity.pane_id.clone());
    let ambient_sel = if explicit_pane.is_none() {
        Some(identity)
    } else {
        None
    };
    let Some(pane_id) = pane_id else {
        return Err(RunError::usage(
            "pane id required (argument, HERDR_PANE_ID, or plugin context)",
        ));
    };
    let rest: Vec<String> = pos.iter().skip(2).cloned().collect();

    // --- per-action argument validation (usage errors, exit 2) ---
    match action.as_str() {
        "list" | "status" => {
            if !rest.is_empty() {
                return Err(RunError::usage(format!(
                    "pr {action} takes no positional arguments"
                )));
            }
        }
        "view" => {
            let number = rest.first().and_then(|s| s.parse::<i64>().ok());
            match number {
                Some(n) if n >= 1 => {}
                _ => return Err(RunError::usage("pr view requires a positive PR number")),
            }
            if rest.len() > 1 {
                return Err(RunError::usage("pr view accepts at most one PR number"));
            }
        }
        "create" => {
            let title = flag_value(&options.args, "--title");
            if title.is_none() {
                return Err(RunError::usage("pr create requires --title"));
            }
            if flag_value(&options.args, "--body").is_some()
                && flag_value(&options.args, "--body-file").is_some()
            {
                return Err(RunError::usage(
                    "--body and --body-file are mutually exclusive",
                ));
            }
            if !rest.is_empty() {
                return Err(RunError::usage("pr create takes no positional arguments"));
            }
            for what in ["base", "head"] {
                if let Some(value) = flag_value(&options.args, &format!("--{what}")) {
                    if value.starts_with('-') {
                        return Err(RunError::usage(format!("invalid --{what} value")));
                    }
                }
            }
        }
        _ => {}
    }
    if action == "list" {
        let state_raw = flag_value(&options.args, "--state").unwrap_or_else(|| "open".into());
        if !["open", "closed", "merged", "all"].contains(&state_raw.as_str()) {
            return Err(RunError::usage(format!("invalid --state: {state_raw}")));
        }
        if let Some(limit_raw) = flag_value(&options.args, "--limit") {
            match limit_raw.parse::<i64>() {
                Ok(l) if (1..=1000).contains(&l) => {}
                _ => return Err(RunError::usage(format!("invalid --limit: {limit_raw}"))),
            }
        }
        if let Some(head) = flag_value(&options.args, "--head") {
            if head.starts_with('-') {
                return Err(RunError::usage("invalid --head value"));
            }
        }
    }

    let repo_flag = flag_value(&options.args, "--repo");
    valid_repo_flag(repo_flag.as_deref())?;

    let snapshot = commands::session_snapshot(&options.call_options())?;
    let selection = PaneSelection {
        pane_id: pane_id.clone(),
        ambient: ambient_sel,
    };
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;
    let timeout = effective_timeout_ms(options, &config);
    let repo_info = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let repo_root = repo_info.root;

    gh::ensure_gh(Path::new(&repo_root), &options.gh_options(timeout_ms))?;
    let repo = gh::resolve_forge_repo(
        Path::new(&repo_root),
        repo_flag.as_deref(),
        &options.gh_options(timeout_ms),
    )?;
    let gh_opts = options.gh_options(timeout_ms);

    let forge_base = |action_name: &str| {
        json!({
            "contract": CONTRACT,
            "command": "pr",
            "pane_id": pane_id,
            "repo_root": repo_root,
            "generated_at": generated_at(),
            "action": action_name,
            "repo": repo,
        })
    };

    match action.as_str() {
        "list" => {
            let state_raw = flag_value(&options.args, "--state").unwrap_or_else(|| "open".into());
            let limit = flag_value(&options.args, "--limit").and_then(|s| s.parse::<i64>().ok());
            let head = flag_value(&options.args, "--head");
            let prs = gh::collect_pr_list(
                Path::new(&repo_root),
                &repo,
                limit,
                Some(&state_raw),
                head.as_deref(),
                &gh_opts,
            )?;
            let mut payload = forge_base("list");
            payload["prs"] = json!(prs);
            Ok(payload)
        }
        "status" => {
            let branch = git::git_ok(
                Path::new(&repo_root),
                &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
                &options.run_options(timeout),
            )?
            .trim()
            .to_string();
            if branch == "HEAD" {
                let mut payload = forge_base("status");
                payload["branch"] = json!(branch);
                payload["detached"] = json!(true);
                payload["pr"] = Value::Null;
                return Ok(payload);
            }
            let pr = gh::collect_pr_status(Path::new(&repo_root), &repo, &branch, &gh_opts)?;
            let mut payload = forge_base("status");
            payload["branch"] = json!(branch);
            payload["pr"] = pr.unwrap_or(Value::Null);
            Ok(payload)
        }
        "view" => {
            let number = rest[0].parse::<i64>().unwrap_or(0);
            let pr = gh::collect_pr_view(Path::new(&repo_root), &repo, number, &gh_opts)?;
            let mut payload = forge_base("view");
            payload["pr"] = pr;
            Ok(payload)
        }
        "reviews" | "comments" | "checks" | "logs" => {
            let number = match rest.first().and_then(|s| s.parse::<i64>().ok()) {
                Some(n) if n >= 1 => n,
                _ => {
                    return Err(RunError::usage(format!(
                        "pr {action} requires a positive PR number"
                    )))
                }
            };
            if rest.len() > 1 {
                return Err(RunError::usage(format!(
                    "pr {action} accepts at most one PR number"
                )));
            }
            match action.as_str() {
                "reviews" => {
                    let data =
                        gh::collect_pr_reviews(Path::new(&repo_root), &repo, number, &gh_opts);
                    let mut payload = forge_base("reviews");
                    merge_json(&mut payload, &data?);
                    Ok(payload)
                }
                "comments" => {
                    let data =
                        gh::collect_pr_comments(Path::new(&repo_root), &repo, number, &gh_opts);
                    let mut payload = forge_base("comments");
                    merge_json(&mut payload, &data?);
                    Ok(payload)
                }
                "checks" => {
                    let data =
                        gh::collect_pr_checks(Path::new(&repo_root), &repo, number, &gh_opts);
                    let mut payload = forge_base("checks");
                    merge_json(&mut payload, &data?);
                    Ok(payload)
                }
                _ => {
                    // logs
                    let run_raw = flag_value(&options.args, "--run");
                    let job_raw = flag_value(&options.args, "--job");
                    for (raw, what) in [(&run_raw, "--run"), (&job_raw, "--job")] {
                        if let Some(r) = raw {
                            if !r.chars().all(|c| c.is_ascii_digit()) {
                                return Err(RunError::usage(format!(
                                    "invalid {what}: {r} (positive integer required)"
                                )));
                            }
                        }
                    }
                    if run_raw.is_some() && job_raw.is_some() {
                        return Err(RunError::usage("--run and --job are mutually exclusive"));
                    }
                    let lines_raw = flag_value(&options.args, "--lines");
                    let mut lines: Option<i64> = None;
                    if let Some(raw) = &lines_raw {
                        match raw.parse::<i64>() {
                            Ok(l) if (1..=2000).contains(&l) => lines = Some(l),
                            _ => return Err(RunError::usage(format!("invalid --lines: {raw}"))),
                        }
                        if job_raw.is_none() {
                            return Err(RunError::usage("--lines requires --job"));
                        }
                    }
                    let req = gh::PrLogsRequest {
                        repo: repo.clone(),
                        number,
                        run_id: run_raw,
                        job_id: job_raw,
                        lines,
                    };
                    let data = gh::collect_pr_logs(Path::new(&repo_root), &req, &gh_opts);
                    let mut payload = forge_base("logs");
                    merge_json(&mut payload, &data?);
                    Ok(payload)
                }
            }
        }
        "review" | "comment" | "reply" | "reviewers" => {
            let number = match rest.first().and_then(|s| s.parse::<i64>().ok()) {
                Some(n) if n >= 1 => n,
                _ => {
                    return Err(RunError::usage(format!(
                        "pr {action} requires a positive PR number"
                    )))
                }
            };
            let expect_head_raw = flag_value(&options.args, "--expect-head");
            let expect_head = match &expect_head_raw {
                Some(s) if gh::is_valid_head_sha(s) => s.clone(),
                _ => {
                    return Err(RunError::usage(format!(
                        "pr {action} requires --expect-head <full head sha> (head.sha from a prior pr read)"
                    )))
                }
            };
            match action.as_str() {
                "review" => {
                    if rest.len() > 1 {
                        return Err(RunError::usage("pr review accepts at most one PR number"));
                    }
                    let events: Vec<&str> = [
                        ("--approve", "APPROVE"),
                        ("--request-changes", "REQUEST_CHANGES"),
                        ("--comment", "COMMENT"),
                    ]
                    .iter()
                    .filter(|(flag, _)| has_flag(&options.args, flag))
                    .map(|(_, e)| *e)
                    .collect();
                    if events.len() != 1 {
                        return Err(RunError::usage(
                            "pr review requires exactly one of --approve, --request-changes, --comment",
                        ));
                    }
                    let comments_file = flag_value(&options.args, "--comments-file");
                    let body_input = resolve_pr_body(&options.args)?;
                    if comments_file.as_deref() == Some("-")
                        && body_input.body_file.as_deref() == Some("-")
                    {
                        return Err(RunError::usage(
                            "--body-file - and --comments-file - cannot both read stdin",
                        ));
                    }
                    let mut comments: Vec<gh::PrLineComment> = Vec::new();
                    if let Some(cf) = &comments_file {
                        let raw = if cf == "-" {
                            read_stdin()
                        } else {
                            match std::fs::read_to_string(cf) {
                                Ok(t) => t,
                                Err(_) => {
                                    return Err(RunError::usage(format!(
                                        "cannot read --comments-file: {cf}"
                                    )))
                                }
                            }
                        };
                        let parsed: Value = match serde_json::from_str(&raw) {
                            Ok(v) => v,
                            Err(_) => {
                                return Err(RunError::usage("--comments-file is not valid JSON"))
                            }
                        };
                        let Some(arr) = parsed.as_array() else {
                            return Err(RunError::usage(
                                "--comments-file must contain a JSON array of line comments",
                            ));
                        };
                        for entry in arr {
                            comments.push(gh::PrLineComment {
                                path: entry
                                    .get("path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                line: entry.get("line").and_then(|v| v.as_i64()).unwrap_or(0),
                                side: entry
                                    .get("side")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                                body: entry
                                    .get("body")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            });
                        }
                    }
                    let event = match events[0] {
                        "APPROVE" => gh::PrReviewEvent::Approve,
                        "REQUEST_CHANGES" => gh::PrReviewEvent::RequestChanges,
                        _ => gh::PrReviewEvent::Comment,
                    };
                    let data = gh::submit_pr_review(
                        Path::new(&repo_root),
                        &repo,
                        number,
                        &expect_head,
                        event,
                        pr_body_text(&body_input).as_deref(),
                        &comments,
                        &gh_opts,
                    )?;
                    let mut payload = forge_base("review");
                    payload["ok"] = json!(true);
                    merge_json(&mut payload, &data);
                    Ok(payload)
                }
                "comment" => {
                    if rest.len() > 1 {
                        return Err(RunError::usage("pr comment accepts at most one PR number"));
                    }
                    let body_input = resolve_pr_body(&options.args)?;
                    let body = pr_body_text(&body_input);
                    match &body {
                        Some(b) if !b.trim().is_empty() => {}
                        _ => {
                            return Err(RunError::usage(
                                "pr comment requires --body or --body-file",
                            ))
                        }
                    }
                    let data = gh::submit_pr_comment(
                        Path::new(&repo_root),
                        &repo,
                        number,
                        &expect_head,
                        body.as_deref().unwrap_or(""),
                        &gh_opts,
                    )?;
                    let mut payload = forge_base("comment");
                    payload["ok"] = json!(true);
                    merge_json(&mut payload, &data);
                    Ok(payload)
                }
                "reply" => {
                    if rest.len() != 2 {
                        return Err(RunError::usage(
                            "pr reply requires a PR number and a review-comment id",
                        ));
                    }
                    let comment_id = match rest[1].parse::<i64>() {
                        Ok(n) if n >= 1 => n,
                        _ => {
                            return Err(RunError::usage(
                                "pr reply requires a positive review-comment id",
                            ))
                        }
                    };
                    let body_input = resolve_pr_body(&options.args)?;
                    let body = pr_body_text(&body_input);
                    match &body {
                        Some(b) if !b.trim().is_empty() => {}
                        _ => {
                            return Err(RunError::usage("pr reply requires --body or --body-file"))
                        }
                    }
                    let data = gh::submit_pr_reply(
                        Path::new(&repo_root),
                        &repo,
                        number,
                        &expect_head,
                        comment_id,
                        body.as_deref().unwrap_or(""),
                        &gh_opts,
                    )?;
                    let mut payload = forge_base("reply");
                    payload["ok"] = json!(true);
                    merge_json(&mut payload, &data);
                    Ok(payload)
                }
                _ => {
                    // reviewers
                    if rest.len() > 1 {
                        return Err(RunError::usage(
                            "pr reviewers accepts at most one PR number",
                        ));
                    }
                    let split_logins = |flag: &str| -> Vec<String> {
                        flag_values(&options.args, flag)
                            .into_iter()
                            .flat_map(|v| {
                                v.split(',')
                                    .map(|s| s.trim().to_string())
                                    .collect::<Vec<_>>()
                            })
                            .filter(|s| !s.is_empty())
                            .collect()
                    };
                    let add = split_logins("--add-reviewer");
                    let remove = split_logins("--remove-reviewer");
                    if add.is_empty() && remove.is_empty() {
                        return Err(RunError::usage(
                            "pr reviewers requires --add-reviewer and/or --remove-reviewer",
                        ));
                    }
                    let data = gh::update_pr_reviewers(
                        Path::new(&repo_root),
                        &repo,
                        number,
                        &expect_head,
                        &add,
                        &remove,
                        &gh_opts,
                    )?;
                    let mut payload = forge_base("reviewers");
                    payload["ok"] = json!(true);
                    merge_json(&mut payload, &data);
                    Ok(payload)
                }
            }
        }
        "merge" => {
            let number = match rest.first().and_then(|s| s.parse::<i64>().ok()) {
                Some(n) if n >= 1 => n,
                _ => return Err(RunError::usage("pr merge requires a positive PR number")),
            };
            if rest.len() > 1 {
                return Err(RunError::usage("pr merge accepts at most one PR number"));
            }
            let expect_head_raw = flag_value(&options.args, "--expect-head");
            let expect_head = match &expect_head_raw {
                Some(s) if gh::is_valid_head_sha(s) => s.clone(),
                _ => {
                    return Err(RunError::usage(
                        "pr merge requires --expect-head <full head sha> (head.sha from a prior pr read)",
                    ))
                }
            };
            let methods: Vec<&str> = [
                ("--squash", "squash"),
                ("--merge", "merge"),
                ("--rebase", "rebase"),
            ]
            .iter()
            .filter(|(flag, _)| has_flag(&options.args, flag))
            .map(|(_, m)| *m)
            .collect();
            if methods.len() > 1 {
                return Err(RunError::usage(
                    "pr merge accepts at most one of --squash, --merge, --rebase",
                ));
            }
            let method = match methods.first() {
                Some(&"squash") => Some(gh::PrMergeMethod::Squash),
                Some(&"merge") => Some(gh::PrMergeMethod::Merge),
                Some(&"rebase") => Some(gh::PrMergeMethod::Rebase),
                _ => None,
            };
            let auto = has_flag(&options.args, "--auto");
            let disable_auto = has_flag(&options.args, "--disable-auto");
            if auto && disable_auto {
                return Err(RunError::usage(
                    "--auto and --disable-auto are mutually exclusive",
                ));
            }
            if auto && method.is_none() {
                return Err(RunError::usage(
                    "pr merge --auto requires --squash, --merge, or --rebase",
                ));
            }
            if disable_auto && method.is_some() {
                return Err(RunError::usage(
                    "merge method flags are not valid with --disable-auto",
                ));
            }
            let req = gh::PrMergeRequest {
                repo: &repo,
                number,
                expect_head: &expect_head,
                method,
                auto,
                disable_auto,
            };
            let data = gh::merge_pr(Path::new(&repo_root), &req, &gh_opts)?;
            let mut payload = forge_base("merge");
            payload["ok"] = json!(true);
            merge_json(&mut payload, &data);
            Ok(payload)
        }
        "create" => {
            let title = flag_value(&options.args, "--title").unwrap_or_default();
            let body_input = resolve_pr_body(&options.args)?;
            let base = flag_value(&options.args, "--base");
            let head = flag_value(&options.args, "--head");
            let mut call_opts = gh_opts.clone();
            call_opts.body_text = body_input.body_text.clone();
            let created = gh::collect_pr_create(
                Path::new(&repo_root),
                &repo,
                &title,
                body_input.body.as_deref(),
                body_input.body_file.as_deref(),
                has_flag(&options.args, "--draft"),
                base.as_deref(),
                head.as_deref(),
                &call_opts,
            )?;
            let mut payload = forge_base("create");
            payload["ok"] = json!(true);
            payload["url"] = json!(created.0);
            payload["number"] = created.1.map(Value::from).unwrap_or(Value::Null);
            Ok(payload)
        }
        _ => unreachable!(),
    }
}

fn merge_json(target: &mut Value, source: &Value) {
    if let (Some(t), Some(s)) = (target.as_object_mut(), source.as_object()) {
        for (k, v) in s {
            t.insert(k.clone(), v.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// `issue` — GitHub issue surface via `gh`, plus pane↔issue links.
// ---------------------------------------------------------------------------

const ISSUE_VALUE_FLAGS: &[&str] = &[
    "--repo",
    "--limit",
    "--state",
    "--label",
    "--assignee",
    "--author",
    "--title",
    "--body",
    "--body-file",
    "--add-label",
    "--remove-label",
    "--add-assignee",
    "--remove-assignee",
    "--reason",
];

fn issue_flag_specs() -> HashMap<String, HashMap<String, FlagKind>> {
    let v = FlagKind::Value;
    let b = FlagKind::Boolean;
    let mut m = HashMap::new();
    m.insert(
        "list".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--limit", v),
            ("--state", v),
            ("--label", v),
            ("--assignee", v),
            ("--author", v),
        ]),
    );
    m.insert("view".to_string(), flags_spec(&[("--repo", v)]));
    m.insert(
        "create".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--title", v),
            ("--body", v),
            ("--body-file", v),
            ("--label", v),
            ("--assignee", v),
        ]),
    );
    m.insert(
        "edit".to_string(),
        flags_spec(&[
            ("--repo", v),
            ("--title", v),
            ("--body", v),
            ("--body-file", v),
            ("--add-label", v),
            ("--remove-label", v),
            ("--add-assignee", v),
            ("--remove-assignee", v),
            ("--state", v),
        ]),
    );
    m.insert(
        "comment".to_string(),
        flags_spec(&[("--repo", v), ("--body", v), ("--body-file", v)]),
    );
    m.insert(
        "close".to_string(),
        flags_spec(&[("--repo", v), ("--reason", v)]),
    );
    m.insert("link".to_string(), flags_spec(&[("--verify", b)]));
    m.insert("unlink".to_string(), HashMap::new());
    m.insert("links".to_string(), HashMap::new());
    m
}

const ISSUE_MUTATION_ACTIONS: &[&str] = &["create", "edit", "comment", "close", "link", "unlink"];

/// Same `<owner>/<repo>#<n>` grammar as inbox-v1 tracker refs.
fn parse_issue_tracker_ref(spec: &str) -> Result<(String, i64), RunError> {
    let re = regex::Regex::new(
        r"^([A-Za-z0-9][A-Za-z0-9_.-]*(?:/[A-Za-z0-9][A-Za-z0-9_.-]*)+)#([1-9][0-9]*)$",
    )
    .unwrap();
    match re.captures(spec) {
        Some(caps) => Ok((caps[1].to_string(), caps[2].parse::<i64>().unwrap_or(0))),
        None => Err(RunError::usage(format!(
            "invalid issue ref (expected <owner>/<repo>#<number>): {spec}"
        ))),
    }
}

/// Comma-split + trim a repeatable value flag into items.
fn flag_items(args: &[String], flag: &str) -> Vec<String> {
    flag_values(args, flag)
        .into_iter()
        .flat_map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Reject option-like label/assignee values before they reach gh argv.
fn assert_forge_items(items: &[String], flag: &str) -> Result<(), RunError> {
    for item in items {
        if item.starts_with('-') {
            return Err(RunError::usage(format!(
                "invalid {flag} value (option-like rejected): {item}"
            )));
        }
    }
    Ok(())
}

fn positive_issue_number(action: &str, rest: &[String]) -> Result<i64, RunError> {
    let number = rest.first().and_then(|s| s.parse::<i64>().ok());
    match number {
        Some(n) if n >= 1 => {}
        _ => {
            return Err(RunError::usage(format!(
                "issue {action} requires a positive issue number"
            )))
        }
    }
    if rest.len() > 1 {
        return Err(RunError::usage(format!(
            "issue {action} accepts at most one issue number"
        )));
    }
    Ok(number.unwrap())
}

/// `issue link|unlink|links` — delegate to rebuildup.inbox's public CLI.
fn cmd_issue_link(
    options: &CliOptions,
    config: &PluginConfig,
    action: &str,
    pane_id: &str,
    rest: &[String],
) -> Result<Value, RunError> {
    let mut inbox_args: Vec<String> = Vec::new();
    if action == "links" {
        inbox_args.push("links".to_string());
        inbox_args.push(pane_id.to_string());
    } else {
        let (repo, number) = parse_issue_tracker_ref(&rest[0])?;
        inbox_args.push(action.to_string());
        inbox_args.push(pane_id.to_string());
        inbox_args.push("--issue".to_string());
        inbox_args.push(format!("{repo}#{number}"));
        if action == "link" && has_flag(&options.args, "--verify") {
            inbox_args.push("--verify".to_string());
        }
    }

    let resolution = inboxlink::resolve_inbox_plugin_root(config.inbox_plugin_root.as_deref());
    let InboxRootResolution::Ok { root, .. } = resolution else {
        let error = match resolution {
            InboxRootResolution::Err { error } => error,
            _ => unreachable!(),
        };
        return Err(RunError::other_kind(
            "inbox_unavailable",
            format!("inbox plugin unavailable: {error}"),
        ));
    };
    let result = inboxlink::run_inbox_cli(
        &root,
        &inbox_args,
        &InboxCliCallOptions {
            session: options.session.clone(),
            socket_path: options.socket_path.clone(),
            timeout_ms: Some(effective_timeout_ms(options, config)),
        },
    )
    .map_err(RunError::other)?;
    if result.status != 0 {
        let message = result
            .error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                let t = result.stderr.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .unwrap_or_default();
        let error_code = result
            .error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if result.status == 2 || error_code.as_deref() == Some("usage") {
            return Err(RunError::usage(if message.is_empty() {
                format!("inbox {action} usage error")
            } else {
                message
            }));
        }
        let kind = match error_code.as_deref() {
            Some(c) if c != "error" => c.to_string(),
            _ => "inbox_error".to_string(),
        };
        return Err(RunError::other_kind(
            &kind,
            if message.is_empty() {
                format!("inbox {action} failed (exit {})", result.status)
            } else {
                message
            },
        ));
    }

    let data = result.json.unwrap_or(Value::Null);
    let mut payload = json!({
        "contract": CONTRACT,
        "command": "issue",
        "action": action,
        "pane_id": pane_id,
        "delegated": INBOX_PLUGIN_ID,
        "ok": true,
        "generated_at": generated_at(),
    });
    for key in ["link", "removed", "links", "warnings"] {
        if let Some(v) = data.get(key) {
            payload[key] = v.clone();
        }
    }
    Ok(payload)
}

/// `issue <action> <pane_id> [args]` — GitHub issue surface via `gh`,
/// plus pane link delegation to rebuildup.inbox.
pub fn cmd_issue(options: &mut CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let timeout_ms = effective_remote_timeout_ms(options, &config);
    let pos = positionals(&options.args, ISSUE_VALUE_FLAGS);
    let action = pos.first().cloned().unwrap_or_default();
    let specs = issue_flag_specs();
    let Some(spec) = specs.get(&action) else {
        return Err(RunError::usage(
            "issue requires an action: list | view | create | edit | comment | close | link | unlink | links",
        ));
    };
    validate_flags(&options.args, spec)?;
    // Forge/inbox mutations need an explicit or consistent ambient
    // target, never the implicit default socket.
    if ISSUE_MUTATION_ACTIONS.contains(&action.as_str()) {
        options.mutation = true;
    }
    let identity = ambient()?;
    let explicit_pane = pos.get(1).cloned();
    let pane_id = explicit_pane.clone().or_else(|| identity.pane_id.clone());
    let ambient_sel = if explicit_pane.is_none() {
        Some(identity)
    } else {
        None
    };
    let Some(pane_id) = pane_id else {
        return Err(RunError::usage(
            "pane id required (argument, HERDR_PANE_ID, or plugin context)",
        ));
    };
    let rest: Vec<String> = pos.iter().skip(2).cloned().collect();

    // --- link surface: pure delegation, no repo/gh resolution needed ---
    if action == "link" || action == "unlink" || action == "links" {
        if action == "links" {
            if !rest.is_empty() {
                return Err(RunError::usage("issue links takes no positional arguments"));
            }
        } else {
            if rest.len() != 1 {
                return Err(RunError::usage(format!(
                    "issue {action} requires exactly one <owner>/<repo>#<number> argument"
                )));
            }
            parse_issue_tracker_ref(&rest[0])?;
        }
        return cmd_issue_link(options, &config, &action, &pane_id, &rest);
    }

    // --- per-action argument validation (usage errors, exit 2) ---
    match action.as_str() {
        "list" => {
            if !rest.is_empty() {
                return Err(RunError::usage("issue list takes no positional arguments"));
            }
            let state_raw = flag_value(&options.args, "--state").unwrap_or_else(|| "open".into());
            if !["open", "closed", "all"].contains(&state_raw.as_str()) {
                return Err(RunError::usage(format!("invalid --state: {state_raw}")));
            }
            if let Some(limit_raw) = flag_value(&options.args, "--limit") {
                match limit_raw.parse::<i64>() {
                    Ok(l) if (1..=1000).contains(&l) => {}
                    _ => return Err(RunError::usage(format!("invalid --limit: {limit_raw}"))),
                }
            }
            for flag in ["--label", "--assignee", "--author"] {
                assert_forge_items(&flag_items(&options.args, flag), flag)?;
            }
        }
        "view" => {
            positive_issue_number("view", &rest)?;
        }
        "create" => {
            let title = flag_value(&options.args, "--title");
            match &title {
                Some(t) if !t.trim().is_empty() => {}
                _ => return Err(RunError::usage("issue create requires --title")),
            }
            if flag_value(&options.args, "--body").is_some()
                && flag_value(&options.args, "--body-file").is_some()
            {
                return Err(RunError::usage(
                    "--body and --body-file are mutually exclusive",
                ));
            }
            if !rest.is_empty() {
                return Err(RunError::usage(
                    "issue create takes no positional arguments",
                ));
            }
            for flag in ["--label", "--assignee"] {
                assert_forge_items(&flag_items(&options.args, flag), flag)?;
            }
        }
        "edit" => {
            positive_issue_number("edit", &rest)?;
            let has_field = flag_value(&options.args, "--title").is_some()
                || flag_value(&options.args, "--body").is_some()
                || flag_value(&options.args, "--body-file").is_some()
                || !flag_items(&options.args, "--add-label").is_empty()
                || !flag_items(&options.args, "--remove-label").is_empty()
                || !flag_items(&options.args, "--add-assignee").is_empty()
                || !flag_items(&options.args, "--remove-assignee").is_empty()
                || flag_value(&options.args, "--state").is_some();
            if !has_field {
                return Err(RunError::usage(
                    "issue edit requires at least one of --title/--body/--body-file/--add-label/--remove-label/--add-assignee/--remove-assignee/--state",
                ));
            }
            if let Some(state_raw) = flag_value(&options.args, "--state") {
                if !["open", "closed"].contains(&state_raw.as_str()) {
                    return Err(RunError::usage(format!("invalid --state: {state_raw}")));
                }
            }
            if flag_value(&options.args, "--body").is_some()
                && flag_value(&options.args, "--body-file").is_some()
            {
                return Err(RunError::usage(
                    "--body and --body-file are mutually exclusive",
                ));
            }
            for flag in [
                "--add-label",
                "--remove-label",
                "--add-assignee",
                "--remove-assignee",
            ] {
                assert_forge_items(&flag_items(&options.args, flag), flag)?;
            }
        }
        "comment" => {
            positive_issue_number("comment", &rest)?;
        }
        "close" => {
            positive_issue_number("close", &rest)?;
            if let Some(reason) = flag_value(&options.args, "--reason") {
                if !["completed", "not planned"].contains(&reason.as_str()) {
                    return Err(RunError::usage(format!("invalid --reason: {reason}")));
                }
            }
        }
        _ => {}
    }

    let repo_flag = flag_value(&options.args, "--repo");
    valid_repo_flag(repo_flag.as_deref())?;

    let snapshot = commands::session_snapshot(&options.call_options())?;
    let selection = PaneSelection {
        pane_id: pane_id.clone(),
        ambient: ambient_sel,
    };
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;
    let timeout = effective_timeout_ms(options, &config);
    let repo_info = git::resolve_repo(Path::new(&cwd), &options.run_options(timeout))?;
    let repo_root = repo_info.root;

    gh::ensure_gh(Path::new(&repo_root), &options.gh_options(timeout_ms))?;
    let repo = gh::resolve_forge_repo(
        Path::new(&repo_root),
        repo_flag.as_deref(),
        &options.gh_options(timeout_ms),
    )?;
    let gh_opts = options.gh_options(timeout_ms);

    let forge_base = |action_name: &str| {
        json!({
            "contract": CONTRACT,
            "command": "issue",
            "pane_id": pane_id,
            "repo_root": repo_root,
            "generated_at": generated_at(),
            "action": action_name,
            "repo": repo,
        })
    };

    match action.as_str() {
        "list" => {
            let state_raw = flag_value(&options.args, "--state").unwrap_or_else(|| "open".into());
            let limit = flag_value(&options.args, "--limit").and_then(|s| s.parse::<i64>().ok());
            let author_flag = flag_value(&options.args, "--author");
            let req = gh::IssueListRequest {
                repo: &repo,
                limit,
                state: Some(&state_raw),
                label: &flag_items(&options.args, "--label"),
                assignee: &flag_items(&options.args, "--assignee"),
                author: author_flag.as_deref(),
            };
            let issues = gh::collect_issue_list(Path::new(&repo_root), &req, &gh_opts)?;
            let mut payload = forge_base("list");
            payload["issues"] = json!(issues);
            Ok(payload)
        }
        "view" => {
            let number = rest[0].parse::<i64>().unwrap_or(0);
            let issue = gh::collect_issue_view(Path::new(&repo_root), &repo, number, &gh_opts)?;
            let mut payload = forge_base("view");
            payload["issue"] = issue;
            Ok(payload)
        }
        "create" => {
            let title = flag_value(&options.args, "--title").unwrap_or_default();
            let body_input = resolve_pr_body(&options.args)?;
            let mut call_opts = gh_opts.clone();
            call_opts.body_text = body_input.body_text.clone();
            let req = gh::IssueCreateRequest {
                repo: &repo,
                title: &title,
                body: body_input.body.as_deref(),
                body_file: body_input.body_file.as_deref(),
                labels: &flag_items(&options.args, "--label"),
                assignees: &flag_items(&options.args, "--assignee"),
            };
            let created = gh::collect_issue_create(Path::new(&repo_root), &req, &call_opts)?;
            let mut payload = forge_base("create");
            payload["ok"] = json!(true);
            payload["url"] = json!(created.0);
            payload["number"] = created.1.map(Value::from).unwrap_or(Value::Null);
            Ok(payload)
        }
        "edit" => {
            let number = rest[0].parse::<i64>().unwrap_or(0);
            let body_input = resolve_pr_body(&options.args)?;
            let state_raw = flag_value(&options.args, "--state");
            let title_flag = flag_value(&options.args, "--title");
            let mut call_opts = gh_opts.clone();
            call_opts.body_text = body_input.body_text.clone();
            let req = gh::IssueEditRequest {
                repo: &repo,
                number,
                title: title_flag.as_deref(),
                body: body_input.body.as_deref(),
                body_file: body_input.body_file.as_deref(),
                add_labels: &flag_items(&options.args, "--add-label"),
                remove_labels: &flag_items(&options.args, "--remove-label"),
                add_assignees: &flag_items(&options.args, "--add-assignee"),
                remove_assignees: &flag_items(&options.args, "--remove-assignee"),
                state: state_raw.as_deref(),
                reason: None,
            };
            let data = gh::update_issue(Path::new(&repo_root), &req, &call_opts)?;
            let mut payload = forge_base("edit");
            payload["ok"] = json!(true);
            merge_json(&mut payload, &data);
            Ok(payload)
        }
        "comment" => {
            let number = rest[0].parse::<i64>().unwrap_or(0);
            let body_input = resolve_pr_body(&options.args)?;
            let body = pr_body_text(&body_input);
            match &body {
                Some(b) if !b.trim().is_empty() => {}
                _ => {
                    return Err(RunError::usage(
                        "issue comment requires --body or --body-file",
                    ))
                }
            }
            let data = gh::submit_issue_comment(
                Path::new(&repo_root),
                &repo,
                number,
                body.as_deref().unwrap_or(""),
                &gh_opts,
            )?;
            let mut payload = forge_base("comment");
            payload["ok"] = json!(true);
            merge_json(&mut payload, &data);
            Ok(payload)
        }
        _ => {
            // close
            let number = rest[0].parse::<i64>().unwrap_or(0);
            let reason_flag = flag_value(&options.args, "--reason");
            let req = gh::IssueEditRequest {
                repo: &repo,
                number,
                title: None,
                body: None,
                body_file: None,
                add_labels: &[],
                remove_labels: &[],
                add_assignees: &[],
                remove_assignees: &[],
                state: Some("closed"),
                reason: reason_flag.as_deref(),
            };
            let data = gh::update_issue(Path::new(&repo_root), &req, &gh_opts)?;
            let mut payload = forge_base("close");
            payload["ok"] = json!(true);
            merge_json(&mut payload, &data);
            Ok(payload)
        }
    }
}

// ---------------------------------------------------------------------------
// `project` — pane metadata projection.
// ---------------------------------------------------------------------------

pub fn cmd_project(options: &mut CliOptions) -> Result<Value, RunError> {
    // project writes pane metadata tokens — a mutation of herdr state.
    options.mutation = true;
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 1, "project <pane_id>")?;
    let config = load_config();
    let selection = resolve_pane_selection(&options.args, &[])?;
    let pane_id = selection.pane_id.clone();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let pane = find_pane_checked(&snapshot, &selection)?;
    let cwd = pane_cwd(pane)?;

    let mut warnings: Vec<String> = Vec::new();
    let mut tokens: BTreeMap<String, Option<String>> = BTreeMap::new();
    tokens.insert("git_branch".into(), None);
    tokens.insert("git_changes".into(), None);
    tokens.insert("git_ahead_behind".into(), None);

    let timeout = effective_timeout_ms(options, &config);
    let status = match git::collect_status(Path::new(&cwd), &options.run_options(timeout)) {
        Ok(report) => {
            let total = report.changes.staged
                + report.changes.unstaged
                + report.changes.untracked
                + report.changes.conflicted;
            tokens.insert("git_branch".into(), Some(report.branch.name.clone()));
            tokens.insert("git_changes".into(), Some(total.to_string()));
            tokens.insert(
                "git_ahead_behind".into(),
                if report.branch.upstream.is_some()
                    && report.branch.ahead.is_some()
                    && report.branch.behind.is_some()
                {
                    Some(format!(
                        "+{}/-{}",
                        report.branch.ahead.unwrap_or(0),
                        report.branch.behind.unwrap_or(0)
                    ))
                } else {
                    None
                },
            );
            status_payload(&pane_id, &report)
        }
        // Not a repo / bare / missing git: clear tokens and report the
        // error as data — the pane legitimately has no git projection.
        Err(e) => json!({
            "contract": CONTRACT,
            "command": "status",
            "pane_id": pane_id,
            "error": { "kind": e.kind.as_str(), "message": e.message },
        }),
    };

    let applied = match commands::report_pane_metadata_as(
        &pane_id,
        &tokens,
        "rebuildup.inbox",
        None,
        &options.call_options(),
    ) {
        Ok(_) => true,
        Err(e) => {
            warnings.push(format!("metadata projection failed: {e}"));
            false
        }
    };

    Ok(json!({
        "contract": CONTRACT,
        "command": "project",
        "pane_id": pane_id,
        "status": status,
        "metadata": {
            "applied": applied,
            "tokens": tokens,
            "warnings": warnings,
        },
    }))
}

// ---------------------------------------------------------------------------
// `capability`
// ---------------------------------------------------------------------------

pub fn cmd_capability(options: &mut CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 0, "capability")?;
    let config = load_config();
    Ok(json!({
        "contract": CONTRACT,
        "plugin": PLUGIN_ID,
        "version": PLUGIN_VERSION,
        "commands": [
            "capability", "status", "files", "diff", "log", "compare",
            "stage", "unstage", "commit", "branch", "upstream", "fetch",
            "push", "remote", "pr", "issue", "project",
        ],
        "read_only": false,
        "mutation_commands": [
            "stage", "unstage", "commit", "branch", "upstream", "fetch",
            "push", "pr", "issue",
        ],
        "requires": { "git": true },
        "defaults": {
            "timeoutMs": config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            "remoteTimeoutMs": config.remote_timeout_ms.unwrap_or(DEFAULT_REMOTE_TIMEOUT_MS),
            "diffMaxBytes": config.diff_max_bytes.unwrap_or(DEFAULT_DIFF_MAX_BYTES),
        },
    }))
}

// ---------------------------------------------------------------------------
// Dispatch / process entry
// ---------------------------------------------------------------------------

/// Run one CLI command in-process and return its payload (or the error
/// the CLI boundary would emit). Shared by the process entrypoint and
/// the TUI's in-process domain calls — no self-respawn.
pub fn dispatch(mut options: CliOptions) -> Result<Value, RunError> {
    match options.command.as_str() {
        "status" => cmd_status(&mut options),
        "diff" => cmd_diff(&mut options),
        "files" => cmd_files(&mut options),
        "log" => cmd_log(&mut options),
        "compare" => cmd_compare(&mut options),
        "stage" => cmd_stage_or_unstage(&mut options, false),
        "unstage" => cmd_stage_or_unstage(&mut options, true),
        "commit" => cmd_commit(&mut options),
        "branch" => cmd_branch(&mut options),
        "upstream" => cmd_upstream(&mut options),
        "fetch" => cmd_fetch(&mut options),
        "push" => cmd_push(&mut options),
        "remote" => cmd_remote(&mut options),
        "pr" => cmd_pr(&mut options),
        "issue" => cmd_issue(&mut options),
        "project" => cmd_project(&mut options),
        "capability" => cmd_capability(&mut options),
        other => Err(RunError::usage(format!(
            "unknown command: {other}\n\n{USAGE}"
        ))),
    }
}

/// Process-level CLI entry: parse argv, dispatch, emit JSON, exit.
pub fn run_cli(argv: &[String]) -> ! {
    let options = match parse_args(argv) {
        Ok(o) => o,
        Err(e) => e.exit(),
    };
    if matches!(options.command.as_str(), "help" | "--help" | "-h") {
        print!("{USAGE}");
        std::process::exit(0);
    }
    match dispatch(options) {
        Ok(payload) => {
            emit(&payload);
            std::process::exit(0);
        }
        Err(e) => e.exit(),
    }
}

const USAGE: &str = "rebuildup.git — git status/diff/mutations for agent panes (contract herdr-plugins.git/v1)\n\nUsage:\n  git status <pane_id> [--session NAME] [--timeout MS]\n  git diff <pane_id> [--staged] [--stat|--name-status|--numstat]\n      [--path PATH]... [--max-bytes N] [rev [rev]]\n  git files <pane_id> [--path PATH]... [--session NAME]\n  git log <pane_id> [--limit N] [--skip N] [--all] [--first-parent]\n      [--author X] [--grep X] [--since X] [--until X] [--path PATH]...\n  git compare <pane_id> [base [head]] [--max-bytes N]\n  git stage <pane_id> [--all | --path PATH]...\n  git unstage <pane_id> [--all | --path PATH]...\n  git commit <pane_id> -m MSG [-m MSG]... [--allow-empty]\n  git branch create <pane_id> <name> [--switch]\n  git branch switch <pane_id> <name>\n  git upstream <pane_id> <upstream-ref>\n  git fetch <pane_id> [remote] [--prune]\n  git push <pane_id> [--set-upstream] [--force-with-lease] [remote] [refspec]\n  git remote <pane_id>\n  git pr list|status|create|view|reviews|comments|checks|logs|merge\n      <pane_id> [args] [--repo owner/name]\n      pr create: --title T [--body B | --body-file PATH|-] [--draft]\n          [--base B] [--head B]\n      pr reviews|comments|checks <pane> <number>\n      pr logs <pane> <number> [--run ID | --job ID [--lines N]]\n  git pr review|comment|reply|reviewers|merge <pane_id> [args]\n      [--repo owner/name] --expect-head <full-sha>   (required)\n      pr review <pane> <number> --approve|--request-changes|--comment\n          [--body B | --body-file PATH|-]\n          [--comments-file PATH|-]   (JSON array: path/line/side/body)\n      pr comment <pane> <number> --body B|--body-file PATH|-\n      pr reply <pane> <number> <comment-id> --body B|--body-file PATH|-\n      pr reviewers <pane> <number>\n          [--add-reviewer LOGIN[,LOGIN]...] [--remove-reviewer LOGIN...]\n      pr merge <pane> <number> [--squash|--merge|--rebase]\n          [--auto | --disable-auto] --expect-head <full-sha>   (required)\n      --expect-head is the stale-head guard: pass head.sha (or\n      headRefOid/head.sha from pr view) from a prior pr read; the\n      mutation is refused (kind \"stale_head\") if the PR head moved.\n      pr merge re-verifies merge state (mergeStateStatus/\n      reviewDecision/isDraft) right before the write; non-mergeable\n      states fail with kind \"merge_blocked\". Release-train head\n      branches (release-x-y-z) are refused with kind\n      \"release_boundary\" — release approval stays a maintainer\n      decision. --auto arms GitHub auto-merge (method flag required);\n      --disable-auto disarms it.\n  git issue list|view|create|edit|comment|close <pane_id> [args]\n      [--repo owner/name]\n      issue list: [--limit N] [--state open|closed|all]\n          [--label L]... [--assignee U]... [--author U]\n      issue view <pane> <number>\n      issue create <pane> --title T [--body B | --body-file PATH|-]\n          [--label L]... [--assignee U]...\n      issue edit <pane> <number> [--title T] [--body B|--body-file PATH|-]\n          [--add-label L]... [--remove-label L]...\n          [--add-assignee U]... [--remove-assignee U]...\n          [--state open|closed]\n      issue comment <pane> <number> --body B|--body-file PATH|-\n      issue close <pane> <number> [--reason completed|\"not planned\"]\n      (no --expect-head: issues have no head sha; mutations are gated by\n      target resolution + explicit invocation only)\n  git issue link <pane_id> <owner>/<repo>#<n> [--verify]\n  git issue unlink <pane_id> <owner>/<repo>#<n>\n  git issue links <pane_id>\n      Pane↔issue links are stored by rebuildup.inbox (delegated via its\n      public CLI); requires the inbox plugin installed/resolvable.\n  git project <pane_id> [--session NAME]\n  git capability\n\nPane id may be omitted inside a plugin action context (HERDR_PANE_ID or\nHERDR_PLUGIN_CONTEXT_JSON.focused_pane_id).\n";
