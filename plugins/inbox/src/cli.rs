//! `rebuildup.inbox` CLI — the versioned structured interface shared by
//! the herdr TUI (via manifest actions), agents, and external clients such
//! as herdr-t3. Rust port of `plugins/inbox/src/cli.ts`.
//!
//! All commands emit a single JSON document on stdout; diagnostics go to
//! stderr. Exit code 0 on success, 1 on handled errors, 2 on usage errors.
//!
//! Pane selection: explicit <pane_id> argument, else HERDR_PANE_ID, else
//! HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id (manifest action context).
//! Ambient pane ids are verified against the resolved session's snapshot;
//! session/socket targeting follows `resolveTarget` — explicit flags beat
//! ambient HERDR_* and contradictions fail closed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hoxay_herdr_client::commands::{self, CallOptions, CommandError};
use hoxay_herdr_client::dirs::plugin_dirs;
use hoxay_herdr_client::snapshot::{PaneInfo, SessionSnapshot};
use hoxay_herdr_client::target::{
    ambient_identity, assert_ambient_pane_identity, resolve_target, session_key_for,
    AmbientIdentity, ResolveTargetOptions, TargetResolutionError,
};
use serde_json::{json, Value};

use crate::model::{
    self, apply_inbox_filters, archive_pane, build_inbox, compute_order_ranks, link_pane, pin_pane,
    prune_state, resolve_state, restore_pane, settle_pane, snooze_pane, to_public_link,
    unarchive_pane, unlink_pane, unpin_pane, upsert_entry, InboxFilters, InboxItem, OrderPlacement,
};
use crate::pluginroot::{
    gh_child_env, resolve_plugin_root, run_plugin_cli, PluginCliCallOptions, PluginRootResolution,
};
use crate::state::{
    self, load_state, remove_watch_link, session_entries, session_watch_read, update_state,
    InboxEntry, LinkKind, LinkWatch, PaneLink, PLUGIN_ID,
};
use crate::transitions::{
    append_cleared_transitions, append_organize_transition, append_transition, transition_context,
    TransitionContext, MAX_TRANSITIONS_PER_SESSION,
};
use crate::watchpr::{
    build_observation, collect_watch_targets, diff_observation, poll_backoff_ms,
    prune_watch_session, WatchDiff, WatchTarget,
};

const CONTRACT: &str = "herdr-plugins.inbox/v1";
const DEFAULT_SNOOZE_CONFIG: &str = "config.json";
const GIT_PLUGIN_ID: &str = "rebuildup.git";
const WATCH_PR_DEFAULT_INTERVAL_SEC: i64 = 120;
/// Per git-plugin CLI call budget (covers ensureGh + the gh request).
const WATCH_PR_CALL_TIMEOUT_MS: u64 = 30_000;
const GH_VERIFY_TIMEOUT_MS: u64 = 15_000;
const WAKE_MAX_SLEEP_MS: u64 = 2_000_000_000;

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

fn take_global_value(args: &[String], i: usize, flag: &str) -> String {
    let value = args.get(i + 1);
    match value {
        Some(v) if !v.is_empty() && !v.starts_with('-') => v.clone(),
        _ => fail(&format!("option {} requires a value", flag), 2),
    }
}

fn parse_global_timeout(raw: &str) -> u64 {
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => v as u64,
        _ => fail(
            &format!(
                "invalid --timeout: {} (expected a positive number of milliseconds)",
                raw
            ),
            2,
        ),
    }
}

pub fn parse_args(argv: &[String]) -> CliOptions {
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
            options.session = Some(take_global_value(&args, i, "--session"));
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--session=") {
            if v.is_empty() {
                fail("option --session requires a value", 2);
            }
            options.session = Some(v.to_string());
            i += 1;
        } else if arg == "--socket" {
            options.socket_path = Some(take_global_value(&args, i, "--socket"));
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--socket=") {
            if v.is_empty() {
                fail("option --socket requires a value", 2);
            }
            options.socket_path = Some(v.to_string());
            i += 1;
        } else if arg == "--timeout" {
            options.timeout_ms = Some(parse_global_timeout(&take_global_value(
                &args,
                i,
                "--timeout",
            )));
            i += 2;
        } else if let Some(v) = arg.strip_prefix("--timeout=") {
            options.timeout_ms = Some(parse_global_timeout(v));
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    options.args = rest;
    options
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

    fn target_options(&self) -> ResolveTargetOptions {
        ResolveTargetOptions {
            session: self.session.clone(),
            socket_path: self.socket_path.clone(),
            mutation: self.mutation,
        }
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn emit(payload: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(payload).unwrap_or_default()
    );
}

fn emit_line(payload: &Value) {
    println!("{}", serde_json::to_string(payload).unwrap_or_default());
}

/// Error document on stderr + exit. Mirrors TS `emitError`/`fail`.
pub fn fail(message: &str, code: i32) -> ! {
    let error = json!({
        "contract": CONTRACT,
        "ok": false,
        "error": {
            "code": if code == 2 { "usage" } else { "error" },
            "message": message,
        },
    });
    eprintln!("{}", serde_json::to_string(&error).unwrap_or_default());
    std::process::exit(code);
}

/// Emit a TargetResolutionError document and exit with its mapped code.
pub fn fail_target(err: &TargetResolutionError) -> ! {
    let doc = json!({
        "contract": CONTRACT,
        "ok": false,
        "error": { "code": err.code.as_str(), "message": err.message },
    });
    eprintln!("{}", serde_json::to_string(&doc).unwrap_or_default());
    std::process::exit(if err.usage { 2 } else { 1 });
}

fn fail_command(err: &CommandError) -> ! {
    match err {
        CommandError::Target(t) => fail_target(t),
        CommandError::Client(e) => fail(&e.to_string(), 1),
    }
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
    /// Handled runtime error (exit 1 at the CLI boundary).
    Other(String),
}

impl RunError {
    pub fn usage(message: impl Into<String>) -> Self {
        RunError::Usage(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        RunError::Other(message.into())
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
            RunError::Other(_) => 1,
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
            RunError::Other(m) => json!({
                "contract": CONTRACT,
                "ok": false,
                "error": { "code": "error", "message": m },
            }),
        };
        serde_json::to_string(&error).unwrap_or_default()
    }

    /// CLI boundary: emit the error document and exit with the mapped code.
    pub fn exit(self) -> ! {
        match self {
            RunError::Target(t) => fail_target(&t),
            RunError::Usage(m) => fail(&m, 2),
            RunError::Other(m) => fail(&m, 1),
        }
    }
}

impl From<CommandError> for RunError {
    fn from(e: CommandError) -> Self {
        match e {
            CommandError::Target(t) => RunError::Target(t),
            CommandError::Client(c) => RunError::Other(c.to_string()),
        }
    }
}

impl From<TargetResolutionError> for RunError {
    fn from(e: TargetResolutionError) -> Self {
        RunError::Target(e)
    }
}

impl From<String> for RunError {
    fn from(e: String) -> Self {
        RunError::Other(e)
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Target(t) => write!(f, "{}", t),
            RunError::Usage(m) | RunError::Other(m) => write!(f, "{}", m),
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
        if let Some(v) = arg.strip_prefix(&format!("{}=", name)) {
            return Some(v.to_string());
        }
    }
    None
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
            return Err(RunError::usage(format!("unknown option: {}", arg)));
        };
        if *kind == FlagKind::Boolean {
            if eq.is_some() {
                return Err(RunError::usage(format!(
                    "option {} does not take a value",
                    name
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
            return Err(RunError::usage(format!("option {} requires a value", name)));
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
    let mut out: Vec<String> = Vec::new();
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
            "{}: unexpected argument {}",
            usage,
            serde_json::to_string(&pos[max]).unwrap_or_default()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pane selection / config / snapshot helpers
// ---------------------------------------------------------------------------

struct PaneSelection {
    pane_id: String,
    /// Non-None when the pane id came from ambient env/context.
    ambient: Option<AmbientIdentity>,
}

/// Resolve the target pane. An explicit positional wins; otherwise the
/// ambient identity bundle is used and kept for the post-snapshot
/// consistency check.
fn resolve_pane_selection(
    args: &[String],
    value_flags: &[&str],
) -> Result<PaneSelection, RunError> {
    let positional = positionals(args, value_flags).into_iter().next();
    if let Some(p) = positional {
        if !p.is_empty() {
            return Ok(PaneSelection {
                pane_id: p,
                ambient: None,
            });
        }
    }
    let identity = ambient_identity().map_err(RunError::Target)?;
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

#[derive(Debug, Default)]
struct InboxConfig {
    default_snooze: Option<String>,
    git_plugin_root: Option<String>,
}

fn load_config() -> InboxConfig {
    let file = plugin_dirs(PLUGIN_ID, None, None)
        .config_dir
        .join(DEFAULT_SNOOZE_CONFIG);
    let Ok(raw) = std::fs::read_to_string(&file) else {
        return InboxConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return InboxConfig::default();
    };
    InboxConfig {
        default_snooze: parsed
            .get("defaultSnooze")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        git_plugin_root: parsed
            .get("gitPluginRoot")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

/// Parse deadline specs: ISO 8601, `+30m`/`+2h`/`+1d` (or bare `30m`).
pub fn parse_until_spec(spec: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, RunError> {
    let normalized = if is_bare_duration(spec) {
        format!("+{}", spec)
    } else {
        spec.to_string()
    };
    if let Some(dt) = parse_duration_spec(&normalized, now) {
        return Ok(dt);
    }
    if let Some(dt) = model::parse_instant(&normalized) {
        return Ok(dt);
    }
    Err(RunError::usage(format!(
        "cannot parse snooze deadline: {}",
        spec
    )))
}

fn is_bare_duration(spec: &str) -> bool {
    // /^\d+[mhd]$/
    let bytes = spec.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    let (num, unit) = bytes.split_at(bytes.len() - 1);
    !num.is_empty()
        && num.iter().all(|b| b.is_ascii_digit())
        && matches!(unit[0], b'm' | b'h' | b'd')
}

fn parse_duration_spec(spec: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // /^\+(\d+)([mhd])$/
    let rest = spec.strip_prefix('+')?;
    let split = rest.find(|c: char| !c.is_ascii_digit())?;
    let (amount, unit) = rest.split_at(split);
    if amount.is_empty() || amount.len() != split {
        // unit must be exactly one char m/h/d
    }
    if unit.len() != 1 || !matches!(unit, "m" | "h" | "d") {
        return None;
    }
    let amount: i64 = amount.parse().ok()?;
    let ms = match unit {
        "m" => amount.checked_mul(60_000)?,
        "h" => amount.checked_mul(3_600_000)?,
        _ => amount.checked_mul(86_400_000)?,
    };
    Some(now + chrono::Duration::milliseconds(ms))
}

fn snapshot_or_err(options: &CliOptions) -> Result<SessionSnapshot, RunError> {
    commands::session_snapshot(&options.call_options()).map_err(RunError::from)
}

fn find_pane<'a>(snapshot: &'a SessionSnapshot, pane_id: &str) -> Result<&'a PaneInfo, RunError> {
    snapshot
        .pane(pane_id)
        .ok_or_else(|| RunError::other(format!("pane not found in session snapshot: {}", pane_id)))
}

/// Project organize state onto pane metadata tokens. Returns warnings.
fn project_metadata(
    pane_id: &str,
    entry: Option<&InboxEntry>,
    options: &CliOptions,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    // Token keys allow only [A-Za-z0-9_-]; a null value clears the token.
    let mut tokens: BTreeMap<String, Option<String>> = BTreeMap::new();
    if let Some(snoozed) = entry.and_then(|e| e.snoozed.as_ref()) {
        tokens.insert("inbox_state".to_string(), Some("snoozed".to_string()));
        tokens.insert(
            "inbox_snoozed_until".to_string(),
            Some(snoozed.until.clone()),
        );
        tokens.insert(
            "inbox_pinned".to_string(),
            entry
                .and_then(|e| e.pinned.as_ref())
                .map(|_| "true".to_string()),
        );
    } else if entry.and_then(|e| e.settled.as_ref()).is_some() {
        tokens.insert("inbox_state".to_string(), Some("settled".to_string()));
        tokens.insert("inbox_snoozed_until".to_string(), None);
        tokens.insert(
            "inbox_pinned".to_string(),
            entry
                .and_then(|e| e.pinned.as_ref())
                .map(|_| "true".to_string()),
        );
    } else if entry.and_then(|e| e.archived.as_ref()).is_some() {
        tokens.insert("inbox_state".to_string(), Some("archived".to_string()));
        tokens.insert("inbox_snoozed_until".to_string(), None);
        tokens.insert(
            "inbox_pinned".to_string(),
            entry
                .and_then(|e| e.pinned.as_ref())
                .map(|_| "true".to_string()),
        );
    } else {
        tokens.insert("inbox_state".to_string(), None);
        tokens.insert("inbox_snoozed_until".to_string(), None);
        tokens.insert("inbox_pinned".to_string(), None);
    }
    if let Err(err) = commands::report_pane_metadata(pane_id, &tokens, &options.call_options()) {
        warnings.push(format!("metadata projection failed: {}", err));
    }
    warnings
}

const ORGANIZE_FILTERS: &[&str] = &["settled", "snoozed", "archived", "none"];

/// Parse read-side `list` filters; unknown --organize values are usage errors.
fn parse_list_filters(args: &[String]) -> Result<InboxFilters, RunError> {
    let mut filters = InboxFilters::default();
    if args.iter().any(|a| a == "--attention") {
        filters.attention = Some(true);
    }
    if let Some(organize) = flag_value(args, "--organize") {
        if !ORGANIZE_FILTERS.contains(&organize.as_str()) {
            return Err(RunError::usage(format!(
                "invalid --organize value (expected settled|snoozed|archived|none): {}",
                organize
            )));
        }
        filters.organize = Some(organize);
    }
    if let Some(search) = flag_value(args, "--search") {
        filters.search = Some(search);
    }
    if let Some(workspace) = flag_value(args, "--workspace") {
        filters.workspace = Some(workspace);
    }
    Ok(filters)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `inbox list` core — returns the payload document. Shared by the CLI
/// dispatch (emits) and the in-process TUI.
pub fn cmd_list(options: &CliOptions) -> Result<Value, RunError> {
    validate_flags(
        &options.args,
        &flags_spec(&[
            ("--attention", FlagKind::Boolean),
            ("--organize", FlagKind::Value),
            ("--search", FlagKind::Value),
            ("--workspace", FlagKind::Value),
        ]),
    )?;
    assert_max_positionals(
        &options.args,
        &["--organize", "--search", "--workspace"],
        0,
        "list",
    )?;
    let now = Utc::now();
    let target = resolve_target(&options.target_options()).map_err(RunError::Target)?;
    let snapshot = snapshot_or_err(options)?;
    let state = load_state()?;
    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let mut entries = state
        .sessions
        .get(&session_key)
        .cloned()
        .unwrap_or_default();
    resolve_state(&mut entries, &snapshot.agents, now);
    let filters = parse_list_filters(&options.args)?;
    let items = apply_inbox_filters(
        &build_inbox(&snapshot.panes, &snapshot.agents, &mut entries, now),
        &filters,
    );
    let inbox: Vec<&InboxItem> = items.iter().filter(|i| i.in_inbox).collect();

    Ok(json!({
        "contract": CONTRACT,
        "generated_at": model::iso_millis(now),
        "session": {
            "name": target.session,
            "socket": options.socket_path.clone().or_else(|| std::env::var("HERDR_SOCKET_PATH").ok()),
        },
        "state_file": state::state_file_path().to_string_lossy(),
        "filters": {
            "attention": filters.attention == Some(true),
            "organize": filters.organize,
            "search": filters.search,
            "workspace": filters.workspace,
        },
        "counts": {
            "total": items.len(),
            "inbox": inbox.len(),
            "attention": items.iter().filter(|i| i.attention).count(),
            "organized": items.iter().filter(|i| i.organize.is_some()).count(),
            "pinned": items.iter().filter(|i| i.pinned).count(),
            "archived": items.iter().filter(|i| i.organize.as_ref().map(|o| o.kind == "archived").unwrap_or(false)).count(),
            "linked": items.iter().filter(|i| !i.links.is_empty()).count(),
        },
        "items": items,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizeCommand {
    Settle,
    Snooze,
    Restore,
    Pin,
    Unpin,
    Archive,
    Unarchive,
}

impl OrganizeCommand {
    fn as_str(&self) -> &'static str {
        match self {
            OrganizeCommand::Settle => "settle",
            OrganizeCommand::Snooze => "snooze",
            OrganizeCommand::Restore => "restore",
            OrganizeCommand::Pin => "pin",
            OrganizeCommand::Unpin => "unpin",
            OrganizeCommand::Archive => "archive",
            OrganizeCommand::Unarchive => "unarchive",
        }
    }
}

/// Organize mutation core (settle/snooze/restore/pin/unpin/archive/
/// unarchive). Returns the payload document; shared by CLI dispatch and
/// the in-process TUI.
pub fn cmd_organize(options: &mut CliOptions, kind: OrganizeCommand) -> Result<Value, RunError> {
    // Every organize command mutates plugin state and pane metadata: the
    // target must be explicit or a consistent ambient context, never the
    // implicit default socket.
    options.mutation = true;
    let value_flags: &[&str] = if kind == OrganizeCommand::Snooze {
        &["--until", "--for"]
    } else {
        &[]
    };
    let spec = if kind == OrganizeCommand::Snooze {
        flags_spec(&[("--until", FlagKind::Value), ("--for", FlagKind::Value)])
    } else {
        HashMap::new()
    };
    validate_flags(&options.args, &spec)?;
    assert_max_positionals(
        &options.args,
        value_flags,
        1,
        &format!("{} <pane_id>", kind.as_str()),
    )?;
    let selection = resolve_pane_selection(&options.args, value_flags)?;
    let pane_id = selection.pane_id.clone();
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    let pane = find_pane(&snapshot, &pane_id)?.clone();
    if let Some(ambient) = &selection.ambient {
        assert_ambient_pane_identity(&pane, ambient).map_err(RunError::Target)?;
    }
    let agent = snapshot.agents.get(&pane_id).cloned();

    // Resolve the snooze deadline before taking the state lock.
    let mut until: Option<DateTime<Utc>> = None;
    if kind == OrganizeCommand::Snooze {
        let until_spec =
            flag_value(&options.args, "--until").or_else(|| flag_value(&options.args, "--for"));
        if let Some(spec) = until_spec {
            until = Some(parse_until_spec(&spec, now)?);
        } else {
            let config = load_config();
            match &config.default_snooze {
                Some(spec) => until = Some(parse_until_spec(spec, now)?),
                None => {
                    return Err(RunError::usage(
                        "snooze requires --until <iso|+duration> or config.json defaultSnooze",
                    ))
                }
            }
        }
    }

    let mut warnings: Vec<String> = Vec::new();
    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let ctx = transition_context(Some(pane.clone()), agent.clone(), None, now);

    // Transition kind to append after the organize mutation (the
    // session-entries borrow must end before `state` can be borrowed
    // again for the transition log).
    enum PendingTransition {
        Settled,
        Snoozed,
        Restored(Option<String>),
        None,
    }

    let entry: Option<InboxEntry> = update_state(|state| {
        let (entry, pending) = {
            let entries = session_entries(state, &session_key);
            match kind {
                OrganizeCommand::Settle => {
                    settle_pane(entries, &pane, agent.as_ref(), now);
                    (entries.get(&pane_id).cloned(), PendingTransition::Settled)
                }
                OrganizeCommand::Snooze => {
                    snooze_pane(
                        entries,
                        &pane,
                        agent.as_ref(),
                        until.expect("snooze until pre-resolved"),
                        now,
                    );
                    (entries.get(&pane_id).cloned(), PendingTransition::Snoozed)
                }
                OrganizeCommand::Restore => {
                    // Record `restored` only when the command actually removed
                    // organize state — a no-op restore is not a transition.
                    let before = entries.get(&pane_id).cloned();
                    let had_state = before
                        .as_ref()
                        .map(|b| {
                            b.settled.is_some()
                                || b.snoozed.is_some()
                                || b.archived.is_some()
                                || b.pinned.is_some()
                                || b.order.is_some()
                        })
                        .unwrap_or(false);
                    restore_pane(entries, &pane_id);
                    let pending = if had_state {
                        PendingTransition::Restored(
                            before.as_ref().and_then(|b| b.workspace_id.clone()),
                        )
                    } else {
                        PendingTransition::None
                    };
                    (None, pending)
                }
                OrganizeCommand::Pin => {
                    pin_pane(entries, &pane, agent.as_ref(), now);
                    (entries.get(&pane_id).cloned(), PendingTransition::None)
                }
                OrganizeCommand::Unpin => {
                    unpin_pane(entries, &pane_id, now);
                    (entries.get(&pane_id).cloned(), PendingTransition::None)
                }
                OrganizeCommand::Archive => {
                    // Archive/unarchive/pin/unpin are not organize transitions
                    // (only settle/snooze/restore record them, per cli.ts).
                    archive_pane(entries, &pane, agent.as_ref(), now);
                    (entries.get(&pane_id).cloned(), PendingTransition::None)
                }
                OrganizeCommand::Unarchive => {
                    unarchive_pane(entries, &pane_id, now);
                    (entries.get(&pane_id).cloned(), PendingTransition::None)
                }
            }
        };
        match pending {
            PendingTransition::Settled => {
                append_organize_transition(state, &session_key, "settled", &pane_id, &ctx);
            }
            PendingTransition::Snoozed => {
                append_organize_transition(state, &session_key, "snoozed", &pane_id, &ctx);
            }
            PendingTransition::Restored(workspace_id) => {
                let mut ctx2 = ctx.clone();
                ctx2.workspace_id = workspace_id;
                append_organize_transition(state, &session_key, "restored", &pane_id, &ctx2);
            }
            PendingTransition::None => {}
        }
        entry
    })?;

    warnings.extend(project_metadata(&pane_id, entry.as_ref(), options));

    if kind == OrganizeCommand::Snooze {
        if let Some(until_dt) = until {
            if let Some(w) = spawn_snooze_wake(options, until_dt) {
                warnings.push(w);
            }
        }
    }

    let fresh = load_state()?;
    let resolved_entry = fresh
        .sessions
        .get(&session_key)
        .and_then(|m| m.get(&pane_id));
    let organize = resolved_entry.and_then(|e| {
        if let Some(snoozed) = &e.snoozed {
            Some(json!({"kind": "snoozed", "since": snoozed.at, "until": snoozed.until}))
        } else if let Some(settled) = &e.settled {
            Some(json!({"kind": "settled", "since": settled.at}))
        } else {
            e.archived
                .as_ref()
                .map(|archived| json!({"kind": "archived", "since": archived.at}))
        }
    });
    Ok(json!({
        "contract": CONTRACT,
        "command": kind.as_str(),
        "pane_id": pane_id,
        "agent_status": agent.as_ref().and_then(|a| a.agent_status.clone()),
        "state_change_seq": agent.as_ref().and_then(|a| a.state_change_seq).map(|v| v as i64),
        "organize": organize,
        "pinned": resolved_entry.map(|e| e.pinned.is_some()).unwrap_or(false),
        "order": resolved_entry.and_then(|e| e.order),
        "warnings": warnings,
    }))
}

struct ReconcileResult {
    resolved_at: String,
    cleared: Vec<String>,
    pruned: Vec<String>,
    projected: Vec<String>,
    warnings: Vec<String>,
}

/// Single reconcile pass: resolve marks against the live snapshot, prune
/// stale entries, then repair pane metadata for every organized pane.
fn reconcile_once(options: &CliOptions) -> Result<ReconcileResult, RunError> {
    let now = Utc::now();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let agents = &snapshot.agents;
    let live_pane_ids: BTreeSet<String> =
        snapshot.panes.iter().map(|p| p.pane_id.clone()).collect();

    let session_key = session_key_for(&options.target_options())?;

    struct Mutation {
        cleared: Vec<String>,
        pruned: Vec<String>,
        cleared_entries: Vec<(String, Option<InboxEntry>)>,
        organized: Vec<(String, InboxEntry)>,
    }

    struct ClearedJob {
        pane_id: String,
        infos: Vec<model::ClearedMarkInfo>,
        ctx: TransitionContext,
    }
    struct PrunedJob {
        pane_id: String,
        ctx: TransitionContext,
    }

    let mutation = update_state(|state| {
        // Phase 1: session-entries borrow — resolve, prune, and collect
        // everything the transition log needs (both touch `state`).
        let (cleared, pruned, cleared_entries, organized, cleared_jobs, pruned_jobs) = {
            let entries = session_entries(state, &session_key);
            // Snapshot hide-kind/workspace info BEFORE resolution deletes
            // marks and entries — cleared/pruned transitions still need
            // honest context.
            let mut hide_kind_before: HashMap<String, bool> = HashMap::new();
            let mut workspace_before: HashMap<String, Option<String>> = HashMap::new();
            for (pane_id, e) in entries.iter() {
                hide_kind_before.insert(
                    pane_id.clone(),
                    e.settled.is_some() || e.snoozed.is_some() || e.archived.is_some(),
                );
                workspace_before.insert(pane_id.clone(), e.workspace_id.clone());
            }
            let resolved = resolve_state(entries, agents, now);
            let cleared = resolved.0;
            let pruned = prune_state(
                entries,
                &live_pane_ids,
                now,
                &model::PruneOptions::default(),
            );

            // Organize-transition log: record hide-kind clears caused by
            // expiry/generation resolution and retention pruning.
            let at = model::iso_millis(now);
            let pane_by_id: HashMap<String, PaneInfo> = snapshot
                .panes
                .iter()
                .map(|p| (p.pane_id.clone(), p.clone()))
                .collect();
            let mut cleared_by_pane: BTreeMap<String, Vec<model::ResolvedClear>> = BTreeMap::new();
            for cm in resolved.1 {
                if cm.field == model::ClearedMarkField::Pinned {
                    continue;
                }
                cleared_by_pane
                    .entry(cm.pane_id.clone())
                    .or_default()
                    .push(cm);
            }
            let mut cleared_jobs: Vec<ClearedJob> = Vec::new();
            for (pane_id, marks) in &cleared_by_pane {
                let infos: Vec<model::ClearedMarkInfo> = marks
                    .iter()
                    .map(|m| model::ClearedMarkInfo {
                        field: m.field,
                        cause: m.cause,
                    })
                    .collect();
                let ctx = TransitionContext {
                    pane: pane_by_id.get(pane_id).cloned(),
                    agent: agents.get(pane_id).cloned(),
                    workspace_id: workspace_before.get(pane_id).cloned().flatten(),
                    at: at.clone(),
                };
                cleared_jobs.push(ClearedJob {
                    pane_id: pane_id.clone(),
                    infos,
                    ctx,
                });
            }
            let mut pruned_jobs: Vec<PrunedJob> = Vec::new();
            for pane_id in &pruned {
                if !hide_kind_before.get(pane_id).copied().unwrap_or(false) {
                    continue;
                }
                let ctx = TransitionContext {
                    pane: pane_by_id.get(pane_id).cloned(),
                    agent: agents.get(pane_id).cloned(),
                    workspace_id: workspace_before.get(pane_id).cloned().flatten(),
                    at: at.clone(),
                };
                pruned_jobs.push(PrunedJob {
                    pane_id: pane_id.clone(),
                    ctx,
                });
            }
            // A pane can lose one mark while keeping others; project the
            // post-resolution entry, not a blanket clear.
            let cleared_entries: Vec<(String, Option<InboxEntry>)> = cleared
                .iter()
                .filter(|pid| live_pane_ids.contains(*pid))
                .map(|pid| (pid.clone(), entries.get(pid).cloned()))
                .collect();
            let organized: Vec<(String, InboxEntry)> = entries
                .iter()
                .filter(|(pid, e)| {
                    live_pane_ids.contains(*pid)
                        && (e.settled.is_some()
                            || e.snoozed.is_some()
                            || e.archived.is_some()
                            || e.pinned.is_some())
                })
                .map(|(pid, e)| (pid.clone(), e.clone()))
                .collect();
            (
                cleared,
                pruned,
                cleared_entries,
                organized,
                cleared_jobs,
                pruned_jobs,
            )
        };
        // Phase 2: append organize transitions now that `entries` is free.
        for job in &cleared_jobs {
            append_cleared_transitions(state, &session_key, &job.pane_id, &job.infos, &job.ctx);
        }
        for job in &pruned_jobs {
            append_transition(
                state,
                &session_key,
                "cleared",
                &job.pane_id,
                &job.ctx,
                Some("pruned"),
            );
        }
        Mutation {
            cleared,
            pruned,
            cleared_entries,
            organized,
        }
    })
    .map_err(RunError::Other)?;

    let mut warnings: Vec<String> = Vec::new();
    let mut projected: Vec<String> = Vec::new();
    // Only live panes can carry metadata tokens.
    for (pane_id, entry) in &mutation.cleared_entries {
        warnings.extend(project_metadata(pane_id, entry.as_ref(), options));
        projected.push(pane_id.clone());
    }
    for (pane_id, entry) in &mutation.organized {
        warnings.extend(project_metadata(pane_id, Some(entry), options));
        projected.push(pane_id.clone());
    }

    Ok(ReconcileResult {
        resolved_at: model::iso_millis(now),
        cleared: mutation.cleared,
        pruned: mutation.pruned,
        projected,
        warnings,
    })
}

/// Deadline-driven wake: `snooze` detaches a one-shot `_wake` process.
fn spawn_snooze_wake(options: &CliOptions, until: DateTime<Utc>) -> Option<String> {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(err) => return Some(format!("snooze wake scheduler failed: {}", err)),
    };
    let mut cmd = Command::new(exe);
    cmd.arg("_wake")
        .arg("--until")
        .arg(model::iso_millis(until));
    if let Some(socket) = &options.socket_path {
        cmd.arg("--socket").arg(socket);
    }
    if let Some(session) = &options.session {
        cmd.arg("--session").arg(session);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the forked child before exec; setsid
        // only changes session/process-group ids — no memory is touched.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        Ok(_child) => None,
        Err(err) => Some(format!("snooze wake scheduler failed: {}", err)),
    }
}

/// Unwrap-or-exit for process-level (CLI-only) commands.
fn must<T>(r: Result<T, RunError>) -> T {
    r.unwrap_or_else(|e| e.exit())
}

/// Internal `_wake` command: sleep until the deadline, then reconcile once.
fn cmd_wake(options: &mut CliOptions) {
    options.mutation = true;
    must(validate_flags(
        &options.args,
        &flags_spec(&[("--until", FlagKind::Value)]),
    ));
    must(assert_max_positionals(
        &options.args,
        &["--until"],
        0,
        "_wake",
    ));
    let until_spec = flag_value(&options.args, "--until");
    let Some(spec) = until_spec else {
        fail("_wake requires --until <iso8601>", 2);
    };
    let until = must(parse_until_spec(&spec, Utc::now()));
    loop {
        let remaining = until.timestamp_millis() - Utc::now().timestamp_millis();
        if remaining <= 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(
            (remaining as u64).min(WAKE_MAX_SLEEP_MS),
        ));
    }
    // TS cmdWake runs one reconcile pass and emits nothing.
    if let Err(e) = reconcile_once(options) {
        e.exit();
    }
}

/// `inbox reconcile` core — returns the payload document.
pub fn cmd_reconcile(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 0, "reconcile")?;
    let result = reconcile_once(options)?;
    Ok(json!({
        "contract": CONTRACT,
        "command": "reconcile",
        "resolved_at": result.resolved_at,
        "cleared": result.cleared,
        "pruned": result.pruned,
        "projected": result.projected,
        "warnings": result.warnings,
    }))
}

fn parse_order_placement(args: &[String]) -> Result<OrderPlacement, RunError> {
    let before = flag_value(args, "--before");
    let after = flag_value(args, "--after");
    let first = args.iter().any(|a| a == "--first");
    let last = args.iter().any(|a| a == "--last");
    let modes = [before.is_some(), after.is_some(), first, last]
        .iter()
        .filter(|b| **b)
        .count();
    if modes != 1 {
        return Err(RunError::usage(
            "order requires exactly one of --before <pane> | --after <pane> | --first | --last",
        ));
    }
    if let Some(b) = before {
        return Ok(OrderPlacement::Before(b));
    }
    if let Some(a) = after {
        return Ok(OrderPlacement::After(a));
    }
    if first {
        Ok(OrderPlacement::First)
    } else {
        Ok(OrderPlacement::Last)
    }
}

/// Manual ordering: rewrite dense `order` ranks for the moved pane's
/// display group. Ordering never hides or mutates execution state.
pub fn cmd_order(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    let order_flags = flags_spec(&[
        ("--before", FlagKind::Value),
        ("--after", FlagKind::Value),
        ("--first", FlagKind::Boolean),
        ("--last", FlagKind::Boolean),
    ]);
    let order_value_flags = ["--before", "--after"];
    validate_flags(&options.args, &order_flags)?;
    assert_max_positionals(&options.args, &order_value_flags, 1, "order <pane_id>")?;
    let selection = resolve_pane_selection(&options.args, &order_value_flags)?;
    let pane_id = selection.pane_id.clone();
    let placement = parse_order_placement(&options.args)?;
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    if let Some(ambient) = &selection.ambient {
        let pane = find_pane(&snapshot, &pane_id)?;
        assert_ambient_pane_identity(pane, ambient).map_err(RunError::Target)?;
    }
    let state = load_state()?;
    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let mut entries = state
        .sessions
        .get(&session_key)
        .cloned()
        .unwrap_or_default();
    resolve_state(&mut entries, &snapshot.agents, now);
    let items = build_inbox(&snapshot.panes, &snapshot.agents, &mut entries, now);

    let moved = items
        .iter()
        .find(|item| item.in_inbox && item.pane_id == pane_id)
        .cloned();
    let Some(moved) = moved else {
        return Err(RunError::other(format!(
            "pane is not in the inbox (hidden panes cannot be ordered): {}",
            pane_id
        )));
    };
    let siblings: Vec<InboxItem> = items
        .iter()
        .filter(|item| {
            item.in_inbox && item.attention == moved.attention && item.pinned == moved.pinned
        })
        .cloned()
        .collect();
    let ranks = compute_order_ranks(&siblings, &pane_id, &placement).map_err(RunError::usage)?;

    let pane_by_id: HashMap<String, PaneInfo> = snapshot
        .panes
        .iter()
        .map(|p| (p.pane_id.clone(), p.clone()))
        .collect();
    let now_iso = model::iso_millis(now);
    update_state(|state_doc| {
        let entries_map = session_entries(state_doc, &session_key);
        for (id, rank) in &ranks {
            if let Some(pane) = pane_by_id.get(id) {
                let entry = upsert_entry(entries_map, pane, &now_iso);
                entry.order = Some(*rank);
            }
        }
    })?;

    Ok(json!({
        "contract": CONTRACT,
        "command": "order",
        "pane_id": pane_id,
        "order": ranks.get(&pane_id),
        "placement": placement_json(&placement),
        "warnings": [],
    }))
}

fn placement_json(p: &OrderPlacement) -> Value {
    match p {
        OrderPlacement::First => json!({"kind": "first"}),
        OrderPlacement::Last => json!({"kind": "last"}),
        OrderPlacement::Before(id) => json!({"kind": "before", "paneId": id}),
        OrderPlacement::After(id) => json!({"kind": "after", "paneId": id}),
    }
}

const TRACKER_REF_PATTERN: &str =
    r"^([A-Za-z0-9][A-Za-z0-9_.-]*(?:/[A-Za-z0-9][A-Za-z0-9_.-]*)+)#([1-9][0-9]*)$";

/// Parse "<repo>#<number>" where repo is a forge path like "owner/name".
pub fn parse_tracker_ref(spec: &str) -> Result<(String, i64), RunError> {
    let re = regex::Regex::new(TRACKER_REF_PATTERN).expect("tracker ref regex");
    let Some(caps) = re.captures(spec) else {
        return Err(RunError::usage(format!(
            "invalid tracker ref (expected <owner>/<repo>#<number>): {}",
            spec
        )));
    };
    let repo = caps
        .get(1)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let number: i64 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    Ok((repo, number))
}

/// Exactly one of --pr/--issue must carry the tracker ref.
fn tracker_kind_and_ref(args: &[String]) -> Result<(LinkKind, String), RunError> {
    let pr = flag_value(args, "--pr");
    let issue = flag_value(args, "--issue");
    if pr.is_some() == issue.is_some() {
        return Err(RunError::usage(
            "requires exactly one of --pr <repo>#<n> or --issue <repo>#<n>",
        ));
    }
    match pr {
        Some(ref_) => Ok(("pr".to_string(), ref_)),
        None => Ok(("issue".to_string(), issue.unwrap())),
    }
}

/// Opt-in existence check via `gh`. Any failure is a warning — the
/// user-declared link is still recorded. The child gets `ghChildEnv`
/// (git-core): ambient GH_DEBUG/GH_REPO/GH_FORCE_TTY and every
/// repo-redirecting GIT_* are stripped; gh's stderr is untrusted child
/// output and is never copied into warnings verbatim.
fn verify_tracker_ref(kind: &str, repo: &str, number: i64) -> Option<String> {
    let reference = format!("{}#{}", repo, number);
    let env_map = gh_child_env();
    let mut cmd = Command::new("gh");
    cmd.args([
        kind,
        "view",
        &number.to_string(),
        "--repo",
        repo,
        "--json",
        "number",
    ])
    .env_clear()
    .envs(&env_map)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            return Some(format!(
                "link verification unavailable (gh spawn failed): {}; recorded anyway",
                err
            ))
        }
    };
    // Bounded wait: GH_VERIFY_TIMEOUT_MS, then SIGKILL.
    let deadline = std::time::Instant::now() + Duration::from_millis(GH_VERIFY_TIMEOUT_MS);
    let mut child = child;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    match status {
        Some(s) if s.success() => None,
        Some(s) => Some(format!(
            "link verification failed for {} (gh exit {}); recorded anyway",
            reference,
            s.code().unwrap_or(-1)
        )),
        None => Some(format!(
            "link verification timed out for {}; recorded without verification",
            reference
        )),
    }
}

fn link_views(links: Option<&Vec<PaneLink>>) -> Vec<Value> {
    links
        .map(|ls| {
            ls.iter()
                .map(|l| serde_json::to_value(to_public_link(l)).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default()
}

/// Declare a user-owned tracker link (PR/Issue) on a pane.
pub fn cmd_link(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    let link_flags = flags_spec(&[
        ("--pr", FlagKind::Value),
        ("--issue", FlagKind::Value),
        ("--verify", FlagKind::Boolean),
    ]);
    let link_value_flags = ["--pr", "--issue"];
    validate_flags(&options.args, &link_flags)?;
    assert_max_positionals(&options.args, &link_value_flags, 1, "link <pane_id>")?;
    let selection = resolve_pane_selection(&options.args, &link_value_flags)?;
    let pane_id = selection.pane_id.clone();
    let (kind, ref_) = tracker_kind_and_ref(&options.args)?;
    let (repo, number) = parse_tracker_ref(&ref_)?;
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    let pane = find_pane(&snapshot, &pane_id)?.clone();
    if let Some(ambient) = &selection.ambient {
        assert_ambient_pane_identity(&pane, ambient).map_err(RunError::Target)?;
    }
    let agent = snapshot.agents.get(&pane_id).cloned();

    let mut warnings: Vec<String> = Vec::new();
    if options.args.iter().any(|a| a == "--verify") {
        if let Some(problem) = verify_tracker_ref(&kind, &repo, number) {
            warnings.push(problem);
        }
    }

    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let canonical_ref = format!("{}#{}", repo, number);
    update_state(|state| {
        let entries = session_entries(state, &session_key);
        link_pane(
            entries,
            &pane,
            agent.as_ref(),
            model::NewPaneLink {
                kind: kind.clone(),
                repo: repo.clone(),
                number,
                ref_: canonical_ref.clone(),
            },
            now,
        );
    })?;

    let fresh = load_state()?;
    let links = fresh
        .sessions
        .get(&session_key)
        .and_then(|m| m.get(&pane_id))
        .and_then(|e| e.links.as_ref());
    Ok(json!({
        "contract": CONTRACT,
        "command": "link",
        "pane_id": pane_id,
        "link": {"kind": kind, "repo": repo, "number": number, "ref": canonical_ref},
        "links": link_views(links),
        "warnings": warnings,
    }))
}

/// Remove one declared link; the pane must still exist in the snapshot.
pub fn cmd_unlink(options: &mut CliOptions) -> Result<Value, RunError> {
    options.mutation = true;
    let unlink_flags = flags_spec(&[("--pr", FlagKind::Value), ("--issue", FlagKind::Value)]);
    let unlink_value_flags = ["--pr", "--issue"];
    validate_flags(&options.args, &unlink_flags)?;
    assert_max_positionals(&options.args, &unlink_value_flags, 1, "unlink <pane_id>")?;
    let selection = resolve_pane_selection(&options.args, &unlink_value_flags)?;
    let pane_id = selection.pane_id.clone();
    let (kind, ref_) = tracker_kind_and_ref(&options.args)?;
    let (repo, number) = parse_tracker_ref(&ref_)?;
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    let pane = find_pane(&snapshot, &pane_id)?.clone();
    if let Some(ambient) = &selection.ambient {
        assert_ambient_pane_identity(&pane, ambient).map_err(RunError::Target)?;
    }

    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let canonical_ref = format!("{}#{}", repo, number);
    let removed: Option<PaneLink> = update_state(|state| {
        // An unlinked PR stops being watched: drop its poll baseline too.
        // Both touch `state` but disjoint fields — sequence them.
        let removed = {
            let entries = session_entries(state, &session_key);
            unlink_pane(entries, &pane_id, &kind, &repo, number, now)
        };
        remove_watch_link(state, &session_key, &pane_id, &canonical_ref);
        removed
    })?;
    let Some(removed) = removed else {
        return Err(RunError::other(format!(
            "no matching {} link on pane {}: {}#{}",
            kind, pane_id, repo, number
        )));
    };

    let fresh = load_state()?;
    let links = fresh
        .sessions
        .get(&session_key)
        .and_then(|m| m.get(&pane_id))
        .and_then(|e| e.links.as_ref());
    Ok(json!({
        "contract": CONTRACT,
        "command": "unlink",
        "pane_id": pane_id,
        "removed": serde_json::to_value(to_public_link(&removed)).unwrap_or(Value::Null),
        "links": link_views(links),
        "warnings": [],
    }))
}

/// Read the declared links stored for a pane (staleness already resolved).
pub fn cmd_links(options: &CliOptions) -> Result<Value, RunError> {
    validate_flags(&options.args, &HashMap::new())?;
    assert_max_positionals(&options.args, &[], 1, "links <pane_id>")?;
    let selection = resolve_pane_selection(&options.args, &[])?;
    let pane_id = selection.pane_id.clone();
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    if let Some(ambient) = &selection.ambient {
        let pane = find_pane(&snapshot, &pane_id)?;
        assert_ambient_pane_identity(pane, ambient).map_err(RunError::Target)?;
    }
    let state = load_state()?;
    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let mut entries = state
        .sessions
        .get(&session_key)
        .cloned()
        .unwrap_or_default();
    resolve_state(&mut entries, &snapshot.agents, now);
    let Some(entry) = entries.get(&pane_id) else {
        return Err(RunError::other(format!(
            "no inbox state for pane: {}",
            pane_id
        )));
    };
    Ok(json!({
        "contract": CONTRACT,
        "command": "links",
        "pane_id": pane_id,
        "links": link_views(entry.links.as_ref()),
        "warnings": [],
    }))
}

// ---------------------------------------------------------------------------
// watch: long-running reconciler (JSONL)
// ---------------------------------------------------------------------------

/// Long-running reconciler. stdout is JSONL:
///   {"contract","type":"watch.started"|"watch.reconcile"|"watch.stopped",...}
/// Stops on SIGINT/SIGTERM (exit 0) or unexpected stream end (exit 1).
fn cmd_watch(options: &mut CliOptions) {
    options.mutation = true;
    must(validate_flags(
        &options.args,
        &flags_spec(&[("--interval", FlagKind::Value)]),
    ));
    must(assert_max_positionals(
        &options.args,
        &["--interval"],
        0,
        "watch",
    ));
    let interval_raw = flag_value(&options.args, "--interval");
    // Mirrors cli.ts: an explicit but unparseable/non-positive value is a
    // usage error; only an absent flag defaults to 60s.
    let interval_sec = match &interval_raw {
        Some(raw) => match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => fail(
                &format!(
                    "invalid --interval: {} (expected a positive number of seconds)",
                    raw
                ),
                2,
            ),
        },
        None => 60.0,
    };
    let interval_ms = (interval_sec * 1000.0) as u64;

    let call = options.call_options();
    let subscription = match commands::subscribe_agent_events(&call) {
        Ok(s) => s,
        Err(e) => fail_command(&e),
    };
    if let Err(e) = subscription.started() {
        fail_command(&CommandError::Client(e));
    }
    emit_line(&json!({
        "contract": CONTRACT,
        "type": "watch.started",
        "at": model::iso_millis(Utc::now()),
    }));

    // Signal handling: SIGINT/SIGTERM set the stop flag.
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = stop_flag.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])
        .expect("signal hook");
        std::thread::spawn(move || {
            // Blocks until the first SIGINT/SIGTERM, then trips the flag.
            if signals.forever().next().is_some() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // Reader thread: blocking recv() forwards each envelope (or stream
    // end) into a channel so the main loop can multiplex events, the
    // interval ticker, and signals — the Rust equivalent of TS
    // `for await (const event of subscription.events)` + setInterval.
    let subscription = std::sync::Arc::new(subscription);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<hoxay_herdr_client::socket::HerdrEvent>();
    {
        let sub = subscription.clone();
        std::thread::spawn(move || {
            // recv() yields Ok envelopes; any Err (closed/io/rpc error
            // frame) is terminal upstream — the TS iterator ends the
            // for-await loop the same way, so we end the channel too.
            while let Some(Ok(event)) = sub.recv() {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        });
    }

    let mut last_tick = std::time::Instant::now();
    loop {
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Interval tick.
        if last_tick.elapsed() >= Duration::from_millis(interval_ms) {
            last_tick = std::time::Instant::now();
            tick_watch(options, "interval", None);
        }
        // Wait for the next event with a bounded slice so the interval
        // ticker and stop flag stay responsive.
        let wait = Duration::from_millis(50).min(Duration::from_millis(interval_ms.max(1)));
        match event_rx.recv_timeout(wait) {
            Ok(event) => {
                last_tick = std::time::Instant::now();
                let payload = json!({"event": event.event, "data": event.data});
                tick_watch(options, "event", Some(payload));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let stopping = stop_flag.load(std::sync::atomic::Ordering::Relaxed);
    subscription.close();

    emit_line(&json!({
        "contract": CONTRACT,
        "type": "watch.stopped",
        "reason": if stopping { "signal" } else { "stream_end" },
        "at": model::iso_millis(Utc::now()),
    }));
    if !stopping {
        std::process::exit(1);
    }
}

fn tick_watch(options: &CliOptions, trigger: &str, event: Option<Value>) {
    match reconcile_once(options) {
        Ok(result) => {
            let mut doc = json!({
                "contract": CONTRACT,
                "type": "watch.reconcile",
                "trigger": trigger,
                "resolved_at": result.resolved_at,
                "cleared": result.cleared,
                "pruned": result.pruned,
                "projected": result.projected,
                "warnings": result.warnings,
            });
            if let Some(ev) = event {
                doc["event"] = ev;
            }
            emit_line(&doc);
        }
        Err(e) => {
            emit_line(&json!({
                "contract": CONTRACT,
                "type": "watch.error",
                "trigger": trigger,
                "error": e.to_string(),
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// watch-pr: opt-in PR-update attention watcher (Issue #178)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct PollOutcome {
    target: WatchTarget,
    ok: bool,
    observation: Option<crate::state::LinkWatchObservation>,
    error: Option<String>,
    diff: Option<WatchDiff>,
    /// True when this poll established the baseline (no prior observation).
    baseline: bool,
    /// Committed watch record fields (set inside the state commit).
    record_failures: i64,
    record_next_poll_at: Option<i64>,
    record_last_checked_at: Option<String>,
}

fn cli_error_message(result: &crate::pluginroot::PluginCliRunResult) -> String {
    let trimmed = result.stderr.trim();
    if !trimmed.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
            if let Some(msg) = parsed
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                return msg.to_string();
            }
        }
        return trimmed.chars().take(200).collect();
    }
    format!("exit {}", result.status)
}

/// Poll one linked PR through the git plugin's public CLI — three reads
/// per tick. Any failure fails the whole poll honestly.
fn poll_pr_link(
    git_root: &std::path::Path,
    target: &WatchTarget,
    options: &CliOptions,
) -> PollOutcome {
    let pane_id = &target.pane_id;
    let link = &target.link;
    let call = PluginCliCallOptions {
        session: options.session.clone(),
        socket_path: options.socket_path.clone(),
        timeout_ms: Some(WATCH_PR_CALL_TIMEOUT_MS),
    };
    let run = |action: &str| -> Result<crate::pluginroot::PluginCliRunResult, String> {
        run_plugin_cli(
            git_root,
            GIT_PLUGIN_ID,
            &[
                "pr".to_string(),
                action.to_string(),
                pane_id.clone(),
                link.number.to_string(),
                "--repo".to_string(),
                link.repo.clone(),
            ],
            &call,
        )
    };
    let baseline = target
        .baseline
        .as_ref()
        .and_then(|b| b.observation.as_ref())
        .is_none();

    macro_rules! fail_poll {
        ($which:expr, $res:expr) => {
            match $res {
                Ok(r) if r.status == 0 && r.json.is_some() => r,
                Ok(r) => {
                    return PollOutcome {
                        target: target.clone(),
                        ok: false,
                        observation: None,
                        error: Some(format!(
                            "git pr {} failed for {}: {}",
                            $which,
                            link.ref_,
                            cli_error_message(&r)
                        )),
                        diff: None,
                        baseline,
                        record_failures: 0,
                        record_next_poll_at: None,
                        record_last_checked_at: None,
                    }
                }
                Err(e) => {
                    return PollOutcome {
                        target: target.clone(),
                        ok: false,
                        observation: None,
                        error: Some(format!(
                            "git plugin invocation failed for {}: {}",
                            link.ref_, e
                        )),
                        diff: None,
                        baseline,
                        record_failures: 0,
                        record_next_poll_at: None,
                        record_last_checked_at: None,
                    }
                }
            }
        };
    }

    let view = fail_poll!("view", run("view"));
    let comments = fail_poll!("comments", run("comments"));
    let checks = fail_poll!("checks", run("checks"));

    match build_observation(
        view.json.as_ref().unwrap(),
        comments.json.as_ref().unwrap(),
        checks.json.as_ref().unwrap(),
    ) {
        Ok(obs) => PollOutcome {
            target: target.clone(),
            ok: true,
            observation: Some(obs),
            error: None,
            diff: None,
            baseline,
            record_failures: 0,
            record_next_poll_at: None,
            record_last_checked_at: None,
        },
        Err(e) => PollOutcome {
            target: target.clone(),
            ok: false,
            observation: None,
            error: Some(format!("{}: {}", link.ref_, e)),
            diff: None,
            baseline: false,
            record_failures: 0,
            record_next_poll_at: None,
            record_last_checked_at: None,
        },
    }
}

#[derive(Debug)]
struct AttentionRecord {
    ref_: String,
    triggers: Vec<String>,
    new_failing_checks: Vec<String>,
    new_review_count: i64,
    new_comment_count: i64,
    cleared: Vec<String>,
}

struct GitResolution {
    root: Option<PathBuf>,
    error: Option<String>,
    via: Option<String>,
}

/// One watch-pr pass. Returns the stop reason when nothing watchable
/// remains, else None.
fn watch_pr_tick(
    options: &CliOptions,
    session_key: &str,
    interval_ms: i64,
    pane_filter: Option<&str>,
    git: &GitResolution,
) -> Result<Option<String>, RunError> {
    let now = Utc::now();
    let now_iso = model::iso_millis(now);
    let now_ms = now.timestamp_millis();
    let snapshot = commands::session_snapshot(&options.call_options())?;
    let agents = &snapshot.agents;
    let panes = &snapshot.panes;
    let live_pane_ids: BTreeSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();

    let state0 = load_state()?;
    let mut entries0 = state0
        .sessions
        .get(session_key)
        .cloned()
        .unwrap_or_default();
    resolve_state(&mut entries0, agents, now);
    let session_watch = session_watch_read(&state0, session_key);
    let scan = collect_watch_targets(&entries0, session_watch, &live_pane_ids, pane_filter);

    let mut outcomes: Vec<PollOutcome> = Vec::new();
    for target in &scan.targets {
        if target
            .baseline
            .as_ref()
            .and_then(|b| b.terminal.as_ref())
            .is_some()
        {
            continue;
        }
        if let Some(next) = target.baseline.as_ref().and_then(|b| b.next_poll_at) {
            if next > now_ms {
                continue; // failure backoff — deferred, not polled
            }
        }
        if git.root.is_none() {
            outcomes.push(PollOutcome {
                target: target.clone(),
                ok: false,
                observation: None,
                error: Some(format!(
                    "git plugin unavailable: {}",
                    git.error.clone().unwrap_or_else(|| "unknown".to_string())
                )),
                diff: None,
                baseline: target
                    .baseline
                    .as_ref()
                    .and_then(|b| b.observation.as_ref())
                    .is_none(),
                record_failures: 0,
                record_next_poll_at: None,
                record_last_checked_at: None,
            });
            continue;
        }
        outcomes.push(poll_pr_link(git.root.as_ref().unwrap(), target, options));
    }

    // Single commit: baselines, failure backoff, terminal marks, organize
    // clears for attention return, and baseline pruning — one lock.
    let mut attention: BTreeMap<String, Vec<AttentionRecord>> = BTreeMap::new();
    // `now_iso` is needed after the commit for event payloads; hand the
    // closure its own clone so the `move` capture leaves ours intact.
    let commit_now_iso = now_iso.clone();
    let (committed, attention) = update_state(move |state| {
        let now_iso = commit_now_iso;
        // Field-precise borrows: `sessions` and `watch` are disjoint.
        let entries = state
            .sessions
            .entry(session_key.to_string())
            .or_insert_with(BTreeMap::new);
        resolve_state(entries, agents, now);
        prune_watch_session(&mut state.watch, session_key, entries, agents, panes, now);
        // Only materialize the watch map when this pass has poll outcomes
        // or a pre-existing session map — an idle tick writes nothing.
        let has_outcomes = !outcomes.is_empty();
        let session_watch_exists = state
            .watch
            .as_ref()
            .map(|w| w.contains_key(session_key))
            .unwrap_or(false);
        if !has_outcomes && !session_watch_exists {
            return (Vec::<PollOutcome>::new(), attention);
        }
        {
            let watch = state.watch.get_or_insert_with(BTreeMap::new);
            watch
                .entry(session_key.to_string())
                .or_insert_with(BTreeMap::new);
        }

        for outcome in outcomes.iter_mut() {
            let pane_id = outcome.target.pane_id.clone();
            let ref_ = outcome.target.ref_.clone();
            let watch = state
                .watch
                .as_mut()
                .and_then(|w| w.get_mut(session_key))
                .expect("watch session");
            let pane_watch = watch.entry(pane_id.clone()).or_insert_with(BTreeMap::new);
            let record = pane_watch.entry(ref_.clone()).or_insert_with(|| LinkWatch {
                first_seen_at: now_iso.clone(),
                consecutive_failures: 0,
                ..Default::default()
            });
            if !outcome.ok {
                record.consecutive_failures += 1;
                record.last_error = Some(
                    outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown poll error".to_string()),
                );
                record.next_poll_at =
                    Some(now_ms + poll_backoff_ms(interval_ms, record.consecutive_failures));
                outcome.record_failures = record.consecutive_failures;
                outcome.record_next_poll_at = record.next_poll_at;
                outcome.record_last_checked_at = record.last_checked_at.clone();
                continue;
            }
            let prev = record.observation.clone();
            let obs = outcome
                .observation
                .clone()
                .expect("ok outcome has observation");
            let diff = diff_observation(prev.as_ref(), &obs);
            outcome.baseline = prev.is_none();
            record.observation = Some(obs);
            record.last_checked_at = Some(now_iso.clone());
            record.last_error = None;
            record.consecutive_failures = 0;
            record.next_poll_at = None;
            if let Some(terminal) = &diff.terminal {
                record.terminal = Some(terminal.clone());
            }
            outcome.record_failures = 0;
            outcome.record_next_poll_at = None;
            outcome.record_last_checked_at = record.last_checked_at.clone();
            outcome.diff = Some(diff.clone());

            if !diff.triggers.is_empty() {
                // Attention return = clear hide-kind organize marks only.
                let entries = state
                    .sessions
                    .get_mut(session_key)
                    .expect("session entries");
                let entry = entries.get_mut(&pane_id);
                let mut cleared: Vec<String> = Vec::new();
                if let Some(entry) = entry {
                    if entry.settled.is_some() {
                        entry.settled = None;
                        cleared.push("settled".to_string());
                    }
                    if entry.snoozed.is_some() {
                        entry.snoozed = None;
                        cleared.push("snoozed".to_string());
                    }
                    if entry.archived.is_some() {
                        entry.archived = None;
                        cleared.push("archived".to_string());
                    }
                    if !cleared.is_empty() {
                        entry.updated_at = now_iso.clone();
                    }
                }
                attention
                    .entry(pane_id.clone())
                    .or_default()
                    .push(AttentionRecord {
                        ref_: ref_.clone(),
                        triggers: diff
                            .triggers
                            .iter()
                            .map(|t| t.as_str().to_string())
                            .collect(),
                        new_failing_checks: diff.new_failing_checks.clone(),
                        new_review_count: diff.new_review_count,
                        new_comment_count: diff.new_comment_count,
                        cleared,
                    });
            }
        }
        (outcomes, attention)
    })?;

    // Metadata projection + event emission after the commit.
    let fresh = load_state()?;
    for (pane_id, records) in &attention {
        let entry = fresh.sessions.get(session_key).and_then(|m| m.get(pane_id));
        let pane_warnings = project_metadata(pane_id, entry, options);
        let mut all_triggers: Vec<String> = Vec::new();
        for r in records {
            for t in &r.triggers {
                if !all_triggers.contains(t) {
                    all_triggers.push(t.clone());
                }
            }
        }
        let mut cleared_all: Vec<String> = Vec::new();
        for r in records {
            for c in &r.cleared {
                if !cleared_all.contains(c) {
                    cleared_all.push(c.clone());
                }
            }
        }
        let links: Vec<Value> = records
            .iter()
            .map(|r| {
                json!({
                    "ref": r.ref_,
                    "triggers": r.triggers,
                    "new_failing_checks": r.new_failing_checks,
                    "new_review_count": r.new_review_count,
                    "new_comment_count": r.new_comment_count,
                })
            })
            .collect();
        emit_line(&json!({
            "contract": CONTRACT,
            "type": "watch-pr.attention",
            "at": now_iso,
            "pane_id": pane_id,
            "triggers": all_triggers,
            "links": links,
            "cleared": cleared_all,
            "warnings": pane_warnings,
        }));
    }

    let mut polled = 0i64;
    let mut failed = 0i64;
    let mut triggered = 0i64;
    for outcome in &committed {
        if outcome.ok {
            polled += 1;
            if outcome
                .diff
                .as_ref()
                .map(|d| !d.triggers.is_empty())
                .unwrap_or(false)
            {
                triggered += 1;
            }
            let obs = outcome.observation.as_ref();
            let triggers: Vec<&str> = outcome
                .diff
                .as_ref()
                .map(|d| d.triggers.iter().map(|t| t.as_str()).collect())
                .unwrap_or_default();
            emit_line(&json!({
                "contract": CONTRACT,
                "type": "watch-pr.poll",
                "at": now_iso,
                "pane_id": outcome.target.pane_id,
                "ref": outcome.target.ref_,
                "ok": true,
                "baseline": outcome.baseline,
                "triggers": triggers,
                "new_failing_checks": outcome.diff.as_ref().map(|d| d.new_failing_checks.clone()).unwrap_or_default(),
                "new_review_count": outcome.diff.as_ref().map(|d| d.new_review_count).unwrap_or(0),
                "new_comment_count": outcome.diff.as_ref().map(|d| d.new_comment_count).unwrap_or(0),
                "terminal": outcome.diff.as_ref().and_then(|d| d.terminal.clone()),
                "observation": obs.map(|o| json!({
                    "state": o.state,
                    "is_draft": o.is_draft,
                    "merge_state_status": o.merge_state_status,
                    "mergeable": o.mergeable,
                    "updated_at": o.updated_at,
                    "failing_checks": o.failing_checks,
                    "review_count": o.review_keys.len(),
                    "comment_count": o.comment_keys.len(),
                })),
                "last_checked_at": now_iso,
            }));
        } else {
            failed += 1;
            emit_line(&json!({
                "contract": CONTRACT,
                "type": "watch-pr.error",
                "at": now_iso,
                "pane_id": outcome.target.pane_id,
                "ref": outcome.target.ref_,
                "ok": false,
                "error": outcome.error.clone().unwrap_or_else(|| "unknown".to_string()),
                "consecutive_failures": outcome.record_failures.max(1),
                "next_poll_at": outcome.record_next_poll_at,
                "last_checked_at": outcome.record_last_checked_at,
            }));
        }
    }

    // Deferred/terminal counts from the committed records.
    let mut deferred = 0i64;
    let mut terminal = 0i64;
    let fresh_watch = session_watch_read(&fresh, session_key);
    for target in &scan.targets {
        let record = fresh_watch
            .and_then(|w| w.get(&target.pane_id))
            .and_then(|pw| pw.get(&target.ref_));
        if let Some(record) = record {
            if record.terminal.is_some() {
                terminal += 1;
            } else if record.next_poll_at.map(|n| n > now_ms).unwrap_or(false) {
                deferred += 1;
            }
        }
    }

    emit_line(&json!({
        "contract": CONTRACT,
        "type": "watch-pr.tick",
        "at": now_iso,
        "interval_ms": interval_ms,
        "pane_filter": pane_filter,
        "git_root": git.root.as_ref().map(|p| p.to_string_lossy().to_string()),
        "git_root_via": git.via,
        "targets": scan.targets.len(),
        "absent_panes": scan.absent_pane_ids,
        "polled": polled,
        "deferred": deferred,
        "failed": failed,
        "triggered": triggered,
        "terminal": terminal,
    }));

    // Stop conditions.
    let pending_links = scan.absent_pane_ids.len() as i64
        + scan
            .targets
            .iter()
            .filter(|t| {
                fresh_watch
                    .and_then(|w| w.get(&t.pane_id))
                    .and_then(|pw| pw.get(&t.ref_))
                    .map(|r| r.terminal.is_none())
                    .unwrap_or(true)
            })
            .count() as i64;
    if scan.total_pr_links == 0 {
        return Ok(Some("no_links".to_string()));
    }
    if pending_links == 0 {
        return Ok(Some("all_terminal".to_string()));
    }
    Ok(None)
}

/// `watch-pr [pane_id] [--interval SEC] [--once] [--git-root PATH]`.
fn cmd_watch_pr(options: &mut CliOptions) {
    options.mutation = true;
    must(validate_flags(
        &options.args,
        &flags_spec(&[
            ("--interval", FlagKind::Value),
            ("--once", FlagKind::Boolean),
            ("--git-root", FlagKind::Value),
        ]),
    ));
    must(assert_max_positionals(
        &options.args,
        &["--interval", "--git-root"],
        1,
        "watch-pr [pane_id]",
    ));
    let interval_sec = flag_value(&options.args, "--interval")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(WATCH_PR_DEFAULT_INTERVAL_SEC as f64);
    let interval_ms = if interval_sec.is_finite() && interval_sec > 0.0 {
        (interval_sec * 1000.0) as i64
    } else {
        WATCH_PR_DEFAULT_INTERVAL_SEC * 1000
    };
    let once = options.args.iter().any(|a| a == "--once");
    let pane_filter = positionals(&options.args, &["--interval", "--git-root"])
        .into_iter()
        .next();
    let git_root_flag = flag_value(&options.args, "--git-root");

    let session_key =
        session_key_for(&options.target_options()).unwrap_or_else(|e| fail_target(&e));
    let config = load_config();
    let explicit_root = git_root_flag.or(config.git_plugin_root);
    let git = match resolve_plugin_root(GIT_PLUGIN_ID, explicit_root.as_deref()) {
        PluginRootResolution::Ok { root, via } => GitResolution {
            root: Some(root),
            error: None,
            via: Some(via.to_string()),
        },
        PluginRootResolution::Err { error } => GitResolution {
            root: None,
            error: Some(error),
            via: None,
        },
    };

    emit_line(&json!({
        "contract": CONTRACT,
        "type": "watch-pr.started",
        "at": model::iso_millis(Utc::now()),
        "interval_ms": interval_ms,
        "pane_filter": pane_filter,
        "once": once,
        "git_root": git.root.as_ref().map(|p| p.to_string_lossy().to_string()),
        "git_root_via": git.via,
        "git_error": git.error,
    }));

    // Signal handling.
    let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = stop_flag.clone();
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])
        .expect("signal hook");
        std::thread::spawn(move || {
            // Blocks until the first SIGINT/SIGTERM, then trips the flag.
            if signals.forever().next().is_some() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // Every loop exit assigns `stop_reason` before breaking.
    let stop_reason: Option<String>;
    loop {
        match watch_pr_tick(
            options,
            &session_key,
            interval_ms,
            pane_filter.as_deref(),
            &git,
        ) {
            Ok(reason) => {
                if once {
                    stop_reason = Some(reason.unwrap_or_else(|| "once".to_string()));
                    break;
                }
                if let Some(reason) = reason {
                    stop_reason = Some(reason);
                    break;
                }
            }
            Err(e) => {
                emit_line(&json!({
                    "contract": CONTRACT,
                    "type": "watch-pr.error",
                    "at": model::iso_millis(Utc::now()),
                    "error": format!("tick failed: {}", e),
                }));
            }
        }
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            stop_reason = Some("signal".to_string());
            break;
        }
        // Sleep in small slices so signals wake us promptly.
        let sleep_ms = interval_ms.max(0) as u64;
        let mut elapsed = 0u64;
        while elapsed < sleep_ms {
            if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let slice = 100u64.min(sleep_ms - elapsed);
            std::thread::sleep(Duration::from_millis(slice));
            elapsed += slice;
        }
        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
            stop_reason = Some("signal".to_string());
            break;
        }
    }

    emit_line(&json!({
        "contract": CONTRACT,
        "type": "watch-pr.stopped",
        "reason": stop_reason.unwrap_or_else(|| "signal".to_string()),
        "at": model::iso_millis(Utc::now()),
    }));
}

// ---------------------------------------------------------------------------
// transitions: read-only page over the organize-transition log (#182)
// ---------------------------------------------------------------------------

pub fn cmd_transitions(options: &CliOptions) -> Result<Value, RunError> {
    must(validate_flags(
        &options.args,
        &flags_spec(&[("--after", FlagKind::Value), ("--limit", FlagKind::Value)]),
    ));
    must(assert_max_positionals(
        &options.args,
        &["--after", "--limit"],
        0,
        "transitions",
    ));
    let now = Utc::now();
    let snapshot = snapshot_or_err(options)?;
    let state = load_state()?;
    let session_key = session_key_for(&options.target_options()).map_err(RunError::Target)?;
    let mut entries = state
        .sessions
        .get(&session_key)
        .cloned()
        .unwrap_or_default();
    resolve_state(&mut entries, &snapshot.agents, now);

    let log = state.transitions.as_ref().and_then(|t| t.get(&session_key));
    let items: &[crate::state::InboxTransition] = log.map(|l| l.items.as_slice()).unwrap_or(&[]);
    let head_seq = log.map(|l| l.seq).unwrap_or(0);
    let oldest_seq = items.first().map(|t| t.seq);

    let after: Option<i64> = match flag_value(&options.args, "--after") {
        Some(raw) => match raw.parse::<i64>() {
            Ok(v) if v >= 0 => Some(v),
            _ => {
                return Err(RunError::usage(
                    "--after must be a non-negative integer seq",
                ));
            }
        },
        None => None,
    };
    let limit: usize = match flag_value(&options.args, "--limit") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(v) if (1..=MAX_TRANSITIONS_PER_SESSION).contains(&v) => v,
            _ => {
                return Err(RunError::usage(format!(
                    "--limit must be an integer between 1 and {}",
                    MAX_TRANSITIONS_PER_SESSION
                )));
            }
        },
        None => 200,
    };

    // `--after` is exclusive. Without it the newest page is returned.
    let page: Vec<&crate::state::InboxTransition> = match after {
        None => items.iter().rev().take(limit).rev().collect(),
        Some(after) => items.iter().filter(|t| t.seq > after).take(limit).collect(),
    };
    // Preserve original order (oldest→newest) for the newest-page slice.
    let page_values: Vec<Value> = match after {
        None => {
            let start = items.len().saturating_sub(limit);
            items[start..]
                .iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect()
        }
        Some(_) => page
            .iter()
            .map(|t| serde_json::to_value(*t).unwrap_or(Value::Null))
            .collect(),
    };

    Ok(json!({
        "contract": CONTRACT,
        "command": "transitions",
        "generated_at": model::iso_millis(now),
        "session_key": session_key,
        "head_seq": head_seq,
        "oldest_seq": oldest_seq,
        "dropped": log.map(|l| l.dropped).unwrap_or(0),
        "transitions": page_values,
    }))
}

pub fn cmd_capability(options: &CliOptions) -> Result<Value, RunError> {
    must(validate_flags(&options.args, &HashMap::new()));
    must(assert_max_positionals(&options.args, &[], 0, "capability"));
    Ok(json!({
        "contract": CONTRACT,
        "plugin": PLUGIN_ID,
        "version": env!("CARGO_PKG_VERSION"),
        "commands": [
            "list", "settle", "snooze", "restore", "pin", "unpin",
            "archive", "unarchive", "order", "link", "unlink", "links",
            "reconcile", "transitions", "watch", "watch-pr", "capability",
        ],
        "mutation_commands": [
            "settle", "snooze", "restore", "pin", "unpin",
            "archive", "unarchive", "order", "link", "unlink",
            "reconcile", "watch", "watch-pr",
        ],
        "actions": ["open", "settle", "restore", "snooze", "pin", "unpin", "archive", "unarchive"],
        // Additive per-command metadata (#263 minimal subset,
        // docs/research/disposition-ledger.md §5). `commands` and
        // `mutation_commands` stay unchanged for backward compatibility;
        // consumers read this opportunistically (absent ⇒ conservative
        // defaults). Daemon-class commands (`watch`, `watch-pr`) omit
        // `idempotent` — they are excluded from retry semantics.
        "command_metadata": {
            "list":        {"kind": "read", "idempotent": true, "coalesce": true, "freshness": "event-driven", "invalidation": "pull"},
            "links":       {"kind": "read", "idempotent": true, "coalesce": true, "freshness": "event-driven", "invalidation": "pull"},
            "transitions": {"kind": "read", "idempotent": true, "coalesce": true, "freshness": "snapshot-consistent", "invalidation": "pull"},
            "capability":  {"kind": "read", "idempotent": true, "coalesce": true},
            "help":        {"kind": "read", "idempotent": true, "coalesce": true},
            "settle":      {"kind": "mutation", "idempotent": true},
            "snooze":      {"kind": "mutation", "idempotent": true},
            "restore":     {"kind": "mutation", "idempotent": true},
            "pin":         {"kind": "mutation", "idempotent": true},
            "unpin":       {"kind": "mutation", "idempotent": true},
            "archive":     {"kind": "mutation", "idempotent": true},
            "unarchive":   {"kind": "mutation", "idempotent": true},
            "order":       {"kind": "mutation", "idempotent": true},
            "link":        {"kind": "mutation", "idempotent": true},
            "unlink":      {"kind": "mutation", "idempotent": true},
            "reconcile":   {"kind": "mutation", "idempotent": true},
            "watch":       {"kind": "mutation"},
            "watch-pr":    {"kind": "mutation"},
        },
        "tui": {"contract": "herdr-plugins.tui/v1", "entrypoint": "main", "action": "open"},
        "events": ["pane.agent_status_changed", "pane.closed"],
        "startup": ["reconcile"],
        "state_file": state::state_file_path().to_string_lossy(),
        "socket": options.socket_path,
    }))
}

const USAGE: &str = "rebuildup.inbox — agent work inbox (contract herdr-plugins.inbox/v1)\n\nUsage:\n  inbox list [--session NAME] [--socket PATH]\n  inbox settle <pane_id> [--session NAME]\n  inbox snooze <pane_id> --until <iso8601|+30m|+2h|+1d> [--session NAME]\n  inbox restore <pane_id> [--session NAME]\n  inbox pin <pane_id> [--session NAME]\n  inbox unpin <pane_id> [--session NAME]\n  inbox archive <pane_id> [--session NAME]\n  inbox unarchive <pane_id> [--session NAME]\n  inbox order <pane_id> (--first | --last | --before <pane_id> | --after <pane_id>) [--session NAME]\n  inbox link <pane_id> (--pr <owner>/<repo>#<n> | --issue <owner>/<repo>#<n>) [--verify] [--session NAME]\n  inbox unlink <pane_id> (--pr <owner>/<repo>#<n> | --issue <owner>/<repo>#<n>) [--session NAME]\n  inbox links <pane_id> [--session NAME]\n  inbox list [--attention] [--organize settled|snoozed|archived|none] [--search <text>] [--workspace <id>]\n  inbox reconcile [--session NAME]\n  inbox transitions [--after <seq>] [--limit <n>] [--session NAME]\n  inbox watch [--interval SECONDS] [--session NAME]\n  inbox watch-pr [pane_id] [--interval SECONDS] [--once] [--git-root PATH] [--session NAME]\n  inbox capability [--session NAME]\n\nPane id may be omitted inside a plugin action/hook context (HERDR_PANE_ID\nor HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id).\n";

/// Emit a core command's payload or exit with the mapped code/stderr.
fn emit_or_fail(result: Result<Value, RunError>) {
    match result {
        Ok(value) => emit(&value),
        Err(e) => e.exit(),
    }
}

/// Entry point for the CLI surface. `argv` excludes the program name.
pub fn run(argv: &[String]) -> ! {
    let mut options = parse_args(argv);
    match options.command.as_str() {
        "list" => emit_or_fail(cmd_list(&options)),
        "settle" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Settle)),
        "snooze" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Snooze)),
        "restore" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Restore)),
        "pin" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Pin)),
        "unpin" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Unpin)),
        "archive" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Archive)),
        "unarchive" => emit_or_fail(cmd_organize(&mut options, OrganizeCommand::Unarchive)),
        "order" => emit_or_fail(cmd_order(&mut options)),
        "link" => emit_or_fail(cmd_link(&mut options)),
        "unlink" => emit_or_fail(cmd_unlink(&mut options)),
        "links" => emit_or_fail(cmd_links(&options)),
        "reconcile" => emit_or_fail(cmd_reconcile(&mut options)),
        "transitions" => emit_or_fail(cmd_transitions(&options)),
        "capability" => emit_or_fail(cmd_capability(&options)),
        "_wake" => cmd_wake(&mut options),
        "watch" => cmd_watch(&mut options),
        "watch-pr" => cmd_watch_pr(&mut options),
        "help" | "--help" | "-h" => {
            print!("{}", USAGE);
            std::process::exit(0);
        }
        other => fail(&format!("unknown command: {}\n\n{}", other, USAGE), 2),
    }
    std::process::exit(0);
}
