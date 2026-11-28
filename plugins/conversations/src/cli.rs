//! rebuildup.conversations CLI — provider transcript index contract
//! (`herdr-plugins.conversations/v1`) for agents and external clients.
//! Single JSON document on stdout; diagnostics on stderr.
//! Exit 0 success, 1 handled error, 2 usage.
//!
//! Rust port of `plugins/conversations/src/cli.ts` (retired; git
//! history preserves it). `index`/`refresh` resolves each pane agent's
//! transcript via `agent_session.value` (a session id, never assumed a
//! path) + pane cwd + provider-known directories, then incrementally
//! normalizes new bytes into `events/<key>.jsonl`. `sessions` lists
//! indexed transcripts with their last herdr identity binding and a
//! staleness verdict against a live snapshot. No PTY text is ever read.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use hoxay_herdr_client::commands::{self, CallOptions, CommandError};
use hoxay_herdr_client::snapshot::{AgentInfo, PaneInfo, SessionSnapshot};
use serde_json::{json, Map, Value};

use crate::claude::{self, ClaudeLineParser};
use crate::codex::{self, CodexLineParser};
use crate::indexer::{scan_transcript, TranscriptCursor};
use crate::model::{
    agent_session_value, binding_is_stale, provider_id_of, ConversationEvent, LiveBinding,
    SessionRecord, CONTRACT, EVENT_TYPES, PLUGIN_ID, PLUGIN_VERSION, SEARCHABLE_FIELDS,
};
use crate::opencode::{self, OpenCodeResolution};
use crate::providers::{is_supported, provider_descriptor, PROVIDERS};
use crate::read::{self, EventMatcher, ScanTarget};
use crate::state::{
    append_events, events_file_path, load_state, new_session_record, path_exists, update_state,
    write_events_file,
};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

// --- args ---------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct CliOptions {
    pub command: String,
    /// Positional args (flags stripped; `--offline` kept as marker).
    pub args: Vec<String>,
    pub session: Option<String>,
    /// `history --pane` selector.
    pub pane: Option<String>,
    /// `history --session-id` selector (provider:id or bare id).
    pub session_id: Option<String>,
    /// `history --limit`.
    pub limit: Option<f64>,
    /// `history --cursor` (opaque byte offset).
    pub cursor: Option<String>,
    /// `search`/`show`/`usage --provider` filter.
    pub provider: Option<String>,
    /// `search --type` event-type filter.
    pub event_type: Option<String>,
    pub socket_path: Option<String>,
    pub timeout_ms: Option<u64>,
}

pub fn parse_args(argv: &[String]) -> CliOptions {
    let mut iter = argv.iter();
    let command = iter.next().cloned().unwrap_or_else(|| "help".to_string());
    let args: Vec<String> = iter.cloned().collect();
    let mut options = CliOptions {
        command,
        ..Default::default()
    };
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match arg.as_str() {
            "--session" => options.session = take(&mut i),
            "--session-id" => options.session_id = take(&mut i),
            "--pane" => options.pane = take(&mut i),
            "--limit" => {
                if let Some(v) = take(&mut i).and_then(|s| s.parse::<f64>().ok()) {
                    if v.is_finite() && v > 0.0 {
                        options.limit = Some(v);
                    }
                }
            }
            "--cursor" => options.cursor = take(&mut i),
            "--provider" => options.provider = take(&mut i),
            "--type" => options.event_type = take(&mut i),
            "--socket" => options.socket_path = take(&mut i),
            "--timeout" => {
                if let Some(v) = take(&mut i).and_then(|s| s.parse::<f64>().ok()) {
                    if v.is_finite() && v > 0.0 {
                        options.timeout_ms = Some(v as u64);
                    }
                }
            }
            // `--offline` is a marker flag consumed by `sessions`;
            // keep it in args (positionalArgs filters it later).
            _ => options.args.push(arg.clone()),
        }
        i += 1;
    }
    options
}

// --- emit / fail ---------------------------------------------------------------

fn emit(payload: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(payload).unwrap_or_else(|_| "null".into())
    );
}

/// Handled command failure — mirrors TS `fail()`: the error document
/// goes to stderr, the exit code is 0/1/2 mapped by `code`.
#[derive(Debug)]
pub struct RunError {
    pub code: i32,
    pub message: String,
    pub kind: Option<String>,
    pub extra: Option<Value>,
}

impl RunError {
    pub fn new(message: impl Into<String>, code: i32) -> Self {
        RunError {
            code,
            message: message.into(),
            kind: None,
            extra: None,
        }
    }
    pub fn kind(mut self, kind: &str) -> Self {
        self.kind = Some(kind.to_string());
        self
    }
    pub fn extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }
    /// The stderr JSON document (TS `fail` payload shape).
    pub fn stderr_doc(&self) -> String {
        let mut error = Map::new();
        error.insert(
            "code".to_string(),
            Value::String(if self.code == 2 { "usage" } else { "error" }.to_string()),
        );
        error.insert("message".to_string(), Value::String(self.message.clone()));
        if let Some(k) = &self.kind {
            error.insert("kind".to_string(), Value::String(k.clone()));
        }
        if let Some(Value::Object(map)) = &self.extra {
            for (k, v) in map {
                error.insert(k.clone(), v.clone());
            }
        }
        serde_json::to_string(&json!({
            "contract": CONTRACT,
            "ok": false,
            "error": Value::Object(error),
        }))
        .unwrap_or_default()
    }
}

/// Write the error envelope to stderr and return the exit code.
fn fail(message: &str, code: i32, kind: Option<&str>, extra: Option<Value>) -> RunError {
    let mut e = RunError::new(message, code);
    if let Some(k) = kind {
        e.kind = Some(k.to_string());
    }
    e.extra = extra;
    e
}

/// JSON number with JS `JSON.stringify` semantics: integral f64 emits
/// as an integer (`1`, not serde's `1.0`).
fn json_num(v: Option<f64>) -> Value {
    match v {
        Some(f) if f.is_finite() && f.fract() == 0.0 => json!(f as i64),
        Some(f) => json!(f),
        None => Value::Null,
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

// --- config / snapshot ----------------------------------------------------------

#[derive(Default)]
struct PluginConfig {
    timeout_ms: Option<u64>,
}

fn load_config() -> PluginConfig {
    let file = hoxay_herdr_client::dirs::plugin_dirs(PLUGIN_ID, None, None)
        .config_dir
        .join("config.json");
    let Ok(raw) = std::fs::read_to_string(&file) else {
        return PluginConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return PluginConfig::default();
    };
    let timeout_ms = parsed
        .get("timeoutMs")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v as u64);
    PluginConfig { timeout_ms }
}

fn effective_timeout_ms(options: &CliOptions, config: &PluginConfig) -> u64 {
    options
        .timeout_ms
        .or(config.timeout_ms)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

fn call_options(options: &CliOptions, config: &PluginConfig) -> CallOptions {
    CallOptions {
        session: options.session.clone(),
        socket_path: options.socket_path.clone(),
        timeout_ms: Some(effective_timeout_ms(options, config)),
        mutation: false,
    }
}

fn snapshot_or_fail(
    options: &CliOptions,
    config: &PluginConfig,
) -> Result<SessionSnapshot, RunError> {
    commands::session_snapshot(&call_options(options, config)).map_err(|e| {
        fail(
            &format!("herdr session unreachable: {}", command_error_message(&e)),
            1,
            Some("session_unreachable"),
            None,
        )
    })
}

fn command_error_message(e: &CommandError) -> String {
    // CommandError Display covers Client/Target variants; keep the
    // TS `err.message` shape (no "error:" prefix duplication).
    e.to_string()
}

// --- identity -------------------------------------------------------------------

fn agent_session_ref(agent: &AgentInfo) -> Value {
    let Some(session) = &agent.agent_session else {
        return Value::Null;
    };
    let mut ref_map = Map::new();
    if let Some(a) = &session.agent {
        ref_map.insert("agent".to_string(), Value::String(a.clone()));
    }
    if let Some(k) = &session.kind {
        ref_map.insert("kind".to_string(), Value::String(k.clone()));
    }
    if let Some(v) = &session.value {
        ref_map.insert("value".to_string(), Value::String(v.clone()));
    }
    if ref_map.is_empty() {
        Value::Null
    } else {
        Value::Object(ref_map)
    }
}

fn pane_cwd(pane: &PaneInfo, agent: &AgentInfo) -> Option<String> {
    pane.cwd
        .clone()
        .or_else(|| agent.cwd.clone())
        .or_else(|| agent.foreground_cwd.clone())
        .filter(|c| !c.is_empty())
}

fn herdr_session_name(snapshot: &SessionSnapshot, options: &CliOptions) -> String {
    if let Some(name) = snapshot
        .session
        .as_ref()
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
    {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    options
        .session
        .clone()
        .or_else(|| {
            std::env::var("HERDR_SESSION")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "default".to_string())
}

fn identity_for(
    snapshot: &SessionSnapshot,
    options: &CliOptions,
    pane: &PaneInfo,
    agent: &AgentInfo,
) -> Value {
    let agent_name = agent.agent.clone().filter(|a| !a.is_empty()).or_else(|| {
        agent_session_ref(agent)
            .get("agent")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });
    json!({
        "host": hostname(),
        "herdr_session": herdr_session_name(snapshot, options),
        "workspace_id": pane.workspace_id,
        "tab_id": pane.tab_id,
        "pane_id": pane.pane_id,
        "agent": agent_name,
        "agent_session": agent_session_ref(agent),
        "state_change_seq": json_num(agent.state_change_seq),
    })
}

fn hostname() -> String {
    // TS os.hostname(); libc gethostname equivalent.
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        "localhost".to_string()
    }
}

// --- candidates -------------------------------------------------------------------

struct Candidate {
    pane: PaneInfo,
    agent: AgentInfo,
    provider: String,
    session_id: String,
}

/// Collect index candidates from the snapshot; unsupported/malformed
/// agents are skipped with reasons. Mirrors TS paneAgentEntries:
/// panes without an agent entry get an empty AgentInfo.
fn collect_candidates(snapshot: &SessionSnapshot) -> (Vec<Candidate>, Vec<Value>) {
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for pane in &snapshot.panes {
        let agent = snapshot
            .agents
            .get(&pane.pane_id)
            .cloned()
            .unwrap_or_default();
        let base = json!({
            "pane_id": pane.pane_id,
            "workspace_id": pane.workspace_id,
        });
        let Some(provider) = provider_id_of(Some(&agent)) else {
            let mut s = base.as_object().cloned().unwrap_or_default();
            s.insert("reason".to_string(), json!("unknown_provider"));
            skipped.push(Value::Object(s));
            continue;
        };
        let descriptor = provider_descriptor(&provider);
        if descriptor.map(|d| d.status != "supported").unwrap_or(true) || !is_supported(&provider) {
            let mut s = base.as_object().cloned().unwrap_or_default();
            s.insert("provider".to_string(), json!(provider));
            s.insert("reason".to_string(), json!("unsupported_provider"));
            skipped.push(Value::Object(s));
            continue;
        }
        let Some(session_id) = agent_session_value(Some(&agent)) else {
            let mut s = base.as_object().cloned().unwrap_or_default();
            s.insert("provider".to_string(), json!(provider));
            s.insert("reason".to_string(), json!("no_agent_session"));
            skipped.push(Value::Object(s));
            continue;
        };
        candidates.push(Candidate {
            pane: pane.clone(),
            agent,
            provider,
            session_id,
        });
    }
    (candidates, skipped)
}

// --- index ------------------------------------------------------------------------

/// A candidate that could not be indexed, with an explicit reason.
fn skip_outcome(base: Value, reason: &str, extra: Value) -> Value {
    let mut obj = base.as_object().cloned().unwrap_or_default();
    obj.insert("reason".to_string(), json!(reason));
    if let Value::Object(m) = extra {
        for (k, v) in m {
            obj.insert(k, v);
        }
    }
    Value::Object(obj)
}

fn min_max_timestamps(events: &[ConversationEvent]) -> (Option<String>, Option<String>) {
    let mut first: Option<String> = None;
    let mut last: Option<String> = None;
    for e in events {
        let Some(t) = &e.timestamp else { continue };
        if first.as_ref().map(|f| t < f).unwrap_or(true) {
            first = Some(t.clone());
        }
        if last.as_ref().map(|l| t > l).unwrap_or(true) {
            last = Some(t.clone());
        }
    }
    (first, last)
}

/// Shared tail of a successful scan: event log write/append + record
/// bookkeeping. Returns the IndexOutcome JSON fields.
#[allow(clippy::too_many_arguments)]
fn apply_scan_result(
    state: &mut crate::model::ConversationsState,
    mut record: SessionRecord,
    key: &str,
    identity: &Value,
    events: &[ConversationEvent],
    cursor: Option<crate::indexer::TranscriptCursor>,
    guard_hash: Option<u32>,
    turn_index: i64,
    reindexed: bool,
    malformed_lines: u64,
    transcript_path: &str,
    _base: &Value,
) -> Result<Value, String> {
    if reindexed {
        write_events_file(key, events)?;
        record.event_count = events.len() as u64;
        let (first, last) = min_max_timestamps(events);
        record.first_event_at = first;
        record.last_event_at = last;
    } else {
        append_events(key, events)?;
        record.event_count += events.len() as u64;
        for e in events {
            let Some(t) = &e.timestamp else { continue };
            if record
                .first_event_at
                .as_ref()
                .map(|f| t < f)
                .unwrap_or(true)
            {
                record.first_event_at = Some(t.clone());
            }
            if record.last_event_at.as_ref().map(|l| t > l).unwrap_or(true) {
                record.last_event_at = Some(t.clone());
            }
        }
    }

    record.status = crate::model::SessionStatus::Ok;
    record.error = None;
    record.identity = Some(identity.clone());
    if let Some(c) = cursor {
        record.cursor = Some(crate::model::ScanCursor {
            offset: c.offset,
            size: c.size,
            mtime_ms: c.mtime_ms,
            line_count: c.line_count,
        });
    }
    record.guard_hash = guard_hash;
    record.turn_count = turn_index;
    record.last_indexed_at = Some(now());
    let total_events = record.event_count;
    state.sessions.insert(key.to_string(), record.clone());

    // TS IndexOutcome carries exactly these fields — `pane_id` appears
    // only on skip outcomes (which keep `base`), never on success.
    let mut out = Map::new();
    out.insert("key".to_string(), json!(key));
    out.insert("provider".to_string(), json!(record.provider));
    out.insert("session_id".to_string(), json!(record.session_id));
    out.insert("transcript_path".to_string(), json!(transcript_path));
    out.insert("events_appended".to_string(), json!(events.len()));
    out.insert("total_events".to_string(), json!(total_events));
    out.insert("reindexed".to_string(), json!(reindexed));
    out.insert("malformed_lines".to_string(), json!(malformed_lines));
    Ok(Value::Object(out))
}

fn index_file_candidate(
    state: &mut crate::model::ConversationsState,
    candidate: &Candidate,
    identity: &Value,
) -> Result<Value, String> {
    let cwd = pane_cwd(&candidate.pane, &candidate.agent);
    let base = json!({
        "pane_id": candidate.pane.pane_id,
        "provider": candidate.provider,
        "session_id": candidate.session_id,
    });

    let transcript_path = if candidate.provider == "codex" {
        codex::resolve_codex_transcript(&candidate.session_id, cwd.as_deref())
    } else {
        claude::resolve_claude_transcript(&candidate.session_id, cwd.as_deref())
    };
    let Some(transcript_path) = transcript_path else {
        return Ok(skip_outcome(
            base,
            "transcript_missing",
            json!({ "cwd": cwd }),
        ));
    };

    let key = format!("{}:{}", candidate.provider, candidate.session_id);
    let mut record = state.sessions.get(&key).cloned().unwrap_or_else(|| {
        new_session_record(
            &candidate.provider,
            &candidate.session_id,
            &transcript_path,
            cwd.as_deref(),
        )
    });
    record.transcript_path = transcript_path.clone();
    if let Some(c) = &cwd {
        record.cwd = Some(c.clone());
    }

    // Missing-event detection: the transcript cursor says everything is
    // consumed, but the event log is gone (deleted state, restored
    // backup). Reset to a from-0 scan so the log is rebuilt instead of
    // silently staying empty forever.
    if record.event_count > 0 && record.cursor.is_some() {
        let events_file = events_file_path(&key);
        let present = std::fs::metadata(&events_file)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !present {
            record.cursor = None;
            record.guard_hash = None;
            record.turn_count = 0;
            record.event_count = 0;
            record.first_event_at = None;
            record.last_event_at = None;
        }
    }

    let file_name = Path::new(&transcript_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let identity_v = identity.clone();
    let start_turn = record.turn_count;
    let cursor = record.cursor.clone();
    let guard = record.guard_hash;

    let scan = if candidate.provider == "codex" {
        scan_transcript(
            &transcript_path,
            cursor
                .as_ref()
                .map(|c| TranscriptCursor {
                    offset: c.offset,
                    size: c.size,
                    mtime_ms: c.mtime_ms,
                    line_count: c.line_count,
                })
                .as_ref(),
            guard,
            start_turn,
            move |start_turn, _line_base| {
                Box::new(CodexLineParser::new(
                    &file_name,
                    identity_v.clone(),
                    start_turn,
                )) as Box<dyn crate::indexer::LineParser>
            },
        )
    } else {
        scan_transcript(
            &transcript_path,
            cursor
                .as_ref()
                .map(|c| TranscriptCursor {
                    offset: c.offset,
                    size: c.size,
                    mtime_ms: c.mtime_ms,
                    line_count: c.line_count,
                })
                .as_ref(),
            guard,
            start_turn,
            move |start_turn, _line_base| {
                Box::new(ClaudeLineParser::new(
                    &file_name,
                    identity_v.clone(),
                    start_turn,
                )) as Box<dyn crate::indexer::LineParser>
            },
        )
    };

    if let Some(err) = &scan.error {
        record.status = if err == "transcript_missing" {
            crate::model::SessionStatus::Missing
        } else {
            crate::model::SessionStatus::Error
        };
        record.error = Some(err.clone());
        state.sessions.insert(key.clone(), record);
        return Ok(skip_outcome(
            base,
            err,
            json!({ "transcript_path": transcript_path }),
        ));
    }

    apply_scan_result(
        state,
        record,
        &key,
        identity,
        &scan.events,
        scan.cursor,
        scan.guard_hash,
        scan.turn_index,
        scan.reindexed,
        scan.malformed_lines,
        &transcript_path,
        &base,
    )
}

fn index_opencode_candidate(
    state: &mut crate::model::ConversationsState,
    candidate: &Candidate,
    identity: &Value,
) -> Result<Value, String> {
    let cwd = pane_cwd(&candidate.pane, &candidate.agent);
    let base = json!({
        "pane_id": candidate.pane.pane_id,
        "provider": "opencode",
        "session_id": candidate.session_id,
    });

    let resolution = opencode::resolve_opencode_transcript(&candidate.session_id, cwd.as_deref());
    match &resolution {
        OpenCodeResolution::Missing => {
            return Ok(skip_outcome(
                base,
                "transcript_missing",
                json!({ "cwd": cwd }),
            ));
        }
        OpenCodeResolution::UnsupportedLayout { data_dir } => {
            return Ok(skip_outcome(
                base,
                "unsupported_layout",
                json!({ "cwd": cwd, "data_dir": data_dir }),
            ));
        }
        OpenCodeResolution::SessionNotFound { db_path } => {
            return Ok(skip_outcome(
                base,
                "session_not_found",
                json!({ "cwd": cwd, "transcript_path": db_path }),
            ));
        }
        OpenCodeResolution::Ok { .. } => {}
    }
    let OpenCodeResolution::Ok {
        db_path, directory, ..
    } = &resolution
    else {
        unreachable!()
    };

    let key = format!("opencode:{}", candidate.session_id);
    let mut record = state.sessions.get(&key).cloned().unwrap_or_else(|| {
        new_session_record(
            "opencode",
            &candidate.session_id,
            db_path,
            cwd.as_deref().or(directory.as_deref()),
        )
    });
    record.transcript_path = db_path.clone();
    if let Some(c) = &cwd {
        record.cwd = Some(c.clone());
    } else if let Some(d) = directory {
        record.cwd = Some(d.clone());
    }

    // SQLite rows are mutable: every index run is a full rescan and the
    // event log is rewritten, so duplicates/missing rows cannot persist.
    let scan = opencode::scan_opencode_session(&resolution, identity.clone());
    if let Some(err) = &scan.error {
        record.status = crate::model::SessionStatus::Error;
        record.error = Some(err.clone());
        state.sessions.insert(key.clone(), record);
        return Ok(skip_outcome(
            base,
            err,
            json!({ "transcript_path": db_path }),
        ));
    }

    write_events_file(&key, &scan.events)?;
    record.event_count = scan.events.len() as u64;
    let (first, last) = min_max_timestamps(&scan.events);
    record.first_event_at = first;
    record.last_event_at = last;
    record.status = crate::model::SessionStatus::Ok;
    record.error = None;
    record.identity = Some(identity.clone());
    record.cursor = None;
    record.guard_hash = None;
    record.turn_count = scan.turn_count;
    record.last_indexed_at = Some(now());
    state.sessions.insert(key.clone(), record.clone());

    Ok(json!({
        "key": key,
        "provider": "opencode",
        "session_id": candidate.session_id,
        "transcript_path": db_path,
        "events_appended": scan.events.len(),
        "total_events": record.event_count,
        "reindexed": true,
        "malformed_lines": scan.malformed_lines,
    }))
}

fn index_candidate(
    state: &mut crate::model::ConversationsState,
    candidate: &Candidate,
    identity: &Value,
) -> Result<Value, String> {
    if candidate.provider == "opencode" {
        index_opencode_candidate(state, candidate, identity)
    } else {
        index_file_candidate(state, candidate, identity)
    }
}

fn cmd_index(options: &CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let snapshot = snapshot_or_fail(options, &config)?;
    let (candidates, skipped) = collect_candidates(&snapshot);

    // TS wraps candidate indexing in update_state (single locked
    // read-modify-write). Mirror that: mutate under the lock.
    let result = update_state(|locked| {
        let mut local_indexed: Vec<Value> = Vec::new();
        let mut local_misses: Vec<Value> = Vec::new();
        for candidate in &candidates {
            let identity = identity_for(&snapshot, options, &candidate.pane, &candidate.agent);
            match index_candidate(locked, candidate, &identity) {
                Ok(outcome) => {
                    if outcome.get("reason").is_some() {
                        local_misses.push(outcome);
                    } else {
                        local_indexed.push(outcome);
                    }
                }
                Err(e) => {
                    local_misses.push(json!({
                        "pane_id": candidate.pane.pane_id,
                        "provider": candidate.provider,
                        "session_id": candidate.session_id,
                        "reason": "index_error",
                        "error": e,
                    }));
                }
            }
        }
        Ok((local_indexed, local_misses))
    });
    let (indexed, extra_misses) = match result {
        Ok(pair) => pair,
        Err(e) => return Err(fail(&e, 1, Some("state_error"), None)),
    };
    // `skipped` from collect_candidates precede index misses (TS order).
    let mut misses = skipped;
    misses.extend(extra_misses);

    let events_appended: u64 = indexed
        .iter()
        .filter_map(|o| o.get("events_appended").and_then(|v| v.as_u64()))
        .sum();
    Ok(json!({
        "contract": CONTRACT,
        "command": options.command,
        "indexed": indexed,
        "skipped": misses,
        "sessions_indexed": indexed.len(),
        "events_appended": events_appended,
        "generated_at": now(),
    }))
}

// --- sessions ---------------------------------------------------------------------

struct LiveContext {
    live_by_pane: HashMap<String, LiveBinding>,
    live_session: Option<String>,
}

/// Best-effort live snapshot for staleness checks; unreachable socket
/// or `--offline` degrades to null — never a fabricated verdict.
fn live_context(options: &CliOptions) -> Option<LiveContext> {
    if has_flag(&options.args, "--offline") {
        return None;
    }
    let config = load_config();
    let snapshot = commands::session_snapshot(&call_options(options, &config)).ok()?;
    let mut live_by_pane = HashMap::new();
    for pane in &snapshot.panes {
        let agent = snapshot
            .agents
            .get(&pane.pane_id)
            .cloned()
            .unwrap_or_default();
        live_by_pane.insert(
            pane.pane_id.clone(),
            LiveBinding {
                pane_id: pane.pane_id.clone(),
                agent_session_value: agent_session_value(Some(&agent)),
                state_change_seq: agent.state_change_seq,
            },
        );
    }
    Some(LiveContext {
        live_by_pane,
        live_session: Some(herdr_session_name(&snapshot, options)),
    })
}

fn record_identity_pane_id(record: &SessionRecord) -> Option<String> {
    record
        .identity
        .as_ref()
        .and_then(|i| i.get("pane_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Session block shared by sessions/history/search/show/usage
/// responses. `with_events` mirrors the TS split: `sessionReadEntry`
/// (history/search/show/usage) adds `events_present`; the `sessions`
/// command entry does not.
fn session_entry(
    record: &SessionRecord,
    live_ctx: Option<&LiveContext>,
    with_events: bool,
) -> Value {
    let live = record_identity_pane_id(record)
        .and_then(|pid| live_ctx.and_then(|c| c.live_by_pane.get(&pid)).cloned());
    let (stale, reason) = binding_is_stale(record, live.as_ref());
    let mut entry = json!({
        "key": record.key,
        "provider": record.provider,
        "provider_status": provider_descriptor(&record.provider)
            .map(|d| d.status)
            .unwrap_or("unknown"),
        "session_id": record.session_id,
        "cwd": record.cwd,
        "transcript_path": record.transcript_path,
        "status": record.status,
        "error": record.error,
        "identity": record.identity,
        "live": live.map(|l| json!({
            "pane_id": l.pane_id,
            "agent_session_value": l.agent_session_value,
            "state_change_seq": json_num(l.state_change_seq),
        })),
        "live_session": live_ctx.and_then(|c| c.live_session.as_deref()),
        "stale": stale,
        "stale_reason": reason,
        "event_count": record.event_count,
        "turn_count": record.turn_count,
        "first_event_at": record.first_event_at,
        "last_event_at": record.last_event_at,
        "last_indexed_at": record.last_indexed_at,
        "transcript_present": path_exists(Path::new(&record.transcript_path)),
    });
    if with_events {
        entry.as_object_mut().unwrap().insert(
            "events_present".to_string(),
            json!(path_exists(&events_file_path(&record.key))),
        );
    }
    entry
}

fn cmd_sessions(options: &CliOptions) -> Result<Value, RunError> {
    let state = match load_state() {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("state_error"), None)),
    };
    let live_ctx = live_context(options);

    let mut records: Vec<&SessionRecord> = state.sessions.values().collect();
    // TS sorts by last_event_at desc via localeCompare (nulls → "" sort
    // last); BTreeMap iteration is already key-ordered, giving a
    // deterministic tie-break the TS stable sort approximated.
    records.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
    let sessions: Vec<Value> = records
        .iter()
        .map(|r| session_entry(r, live_ctx.as_ref(), false))
        .collect();
    let count = sessions.len();
    Ok(json!({
        "contract": CONTRACT,
        "command": "sessions",
        "sessions": sessions,
        "count": count,
        "generated_at": now(),
    }))
}

// --- read APIs: history / search / show / usage -------------------------------

const DEFAULT_HISTORY_LIMIT: usize = 200;
const MAX_HISTORY_LIMIT: usize = 1_000;
const DEFAULT_SEARCH_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 500;
const MAX_SHOW_RESULTS: usize = 200;

/// Pick the session record a `history` invocation refers to.
fn resolve_history_record<'a>(
    state: &'a crate::model::ConversationsState,
    options: &CliOptions,
) -> Result<(&'a SessionRecord, Option<String>), String> {
    let records: Vec<&SessionRecord> = state.sessions.values().collect();
    if let Some(pane) = &options.pane {
        let mut matches: Vec<&SessionRecord> = records
            .iter()
            .copied()
            .filter(|r| record_identity_pane_id(r).as_deref() == Some(pane.as_str()))
            .collect();
        matches.sort_by(|a, b| {
            b.last_indexed_at
                .cmp(&a.last_indexed_at)
                .then_with(|| a.key.cmp(&b.key))
        });
        let Some(record) = matches.first() else {
            return Err(format!("no indexed session for pane {}", pane));
        };
        let note = if matches.len() > 1 {
            Some("multiple_sessions_for_pane".to_string())
        } else {
            None
        };
        return Ok((record, note));
    }
    if let Some(session_id) = &options.session_id {
        if let Some(exact) = state.sessions.get(session_id) {
            return Ok((exact, None));
        }
        let mut matches: Vec<&SessionRecord> = records
            .iter()
            .copied()
            .filter(|r| r.session_id == *session_id)
            .collect();
        matches.sort_by(|a, b| {
            b.last_event_at
                .cmp(&a.last_event_at)
                .then_with(|| a.key.cmp(&b.key))
        });
        let Some(record) = matches.first() else {
            return Err(format!("no indexed session for {}", session_id));
        };
        let note = if matches.len() > 1 {
            Some("ambiguous_session_id".to_string())
        } else {
            None
        };
        return Ok((record, note));
    }
    Err("history requires --pane PANE or --session-id PROVIDER:ID".to_string())
}

/// Session block shared by history/search/show/usage responses.
fn cmd_history(options: &CliOptions) -> Result<Value, RunError> {
    let state = match load_state() {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("state_error"), None)),
    };
    let (record, note) = match resolve_history_record(&state, options) {
        Ok(v) => v,
        Err(e) => {
            let code = if e.starts_with("history requires") {
                2
            } else {
                1
            };
            return Err(fail(
                &e,
                code,
                Some("session_not_found"),
                Some(
                    json!({ "hint": "run `index` to build the transcript index, or check the selector" }),
                ),
            ));
        }
    };

    let limit = options
        .limit
        .map(|v| (v.floor() as usize).min(MAX_HISTORY_LIMIT))
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .min(MAX_HISTORY_LIMIT);
    let mut offset = 0u64;
    if let Some(cursor) = &options.cursor {
        // TS `Number(cursor)` + Number.isInteger + >= 0: "1e3" → 1000.
        match cursor.parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 && v.fract() == 0.0 => offset = v as u64,
            _ => {
                return Err(fail(
                    &format!("invalid --cursor: {}", cursor),
                    2,
                    None,
                    None,
                ))
            }
        }
    }

    let live_ctx = live_context(options);
    let page = match read::read_event_page(&record.key, offset, limit, &|e| read::match_all(e)) {
        Ok(p) => p,
        Err(e) => return Err(fail(&e, 1, Some("cursor_invalid"), None)),
    };

    let events: Vec<Value> = page.matches.iter().map(|m| m.event.clone()).collect();
    Ok(json!({
        "contract": CONTRACT,
        "command": "history",
        "session": session_entry(record, live_ctx.as_ref(), true),
        "selector_note": note,
        "events": events,
        "page": {
            "limit": limit,
            "offset": offset,
            "returned": page.matches.len(),
            "next_cursor": page.next_offset.to_string(),
            "has_more": page.has_more,
            "malformed_lines": page.malformed_lines,
            "duplicates_skipped": page.duplicates_skipped,
            "events_present": page.present,
            "events_size": page.size,
        },
        "generated_at": now(),
    }))
}

// --- search / usage / show ---------------------------------------------------

/// Select the session records a read command covers. `--pane`,
/// `--session-id`, and `--provider` combine as AND filters; with none
/// given every indexed session is covered. Order is by record key so
/// cursor paging is deterministic across invocations.
fn select_records<'a>(
    state: &'a crate::model::ConversationsState,
    options: &CliOptions,
) -> Result<(Vec<&'a SessionRecord>, Option<String>), String> {
    let mut records: Vec<&SessionRecord> = state.sessions.values().collect();
    let mut note: Option<String> = None;
    if let Some(pane) = &options.pane {
        let before = records.len();
        records.retain(|r| record_identity_pane_id(r).as_deref() == Some(pane.as_str()));
        if records.is_empty() {
            return Err(format!("no indexed session for pane {}", pane));
        }
        if before > records.len() {
            note = Some("filtered_by_pane".to_string());
        }
    }
    if let Some(session_id) = &options.session_id {
        if let Some(exact) = state.sessions.get(session_id) {
            records.retain(|r| r.key == exact.key);
        } else {
            records.retain(|r| r.session_id == *session_id);
        }
        if records.is_empty() {
            return Err(format!("no indexed session for {}", session_id));
        }
        if records.len() > 1 {
            note = Some("ambiguous_session_id".to_string());
        }
    }
    if let Some(provider) = &options.provider {
        records.retain(|r| r.provider == *provider);
        if records.is_empty() {
            return Err(format!("no indexed session for provider {}", provider));
        }
    }
    records.sort_by(|a, b| a.key.cmp(&b.key));
    Ok((records, note))
}

fn positional_args(options: &CliOptions) -> Vec<&str> {
    options
        .args
        .iter()
        .filter(|a| a.as_str() != "--offline")
        .map(|s| s.as_str())
        .collect()
}

fn read_limit(options: &CliOptions, fallback: usize, max: usize) -> usize {
    options
        .limit
        .map(|v| (v.floor() as usize).min(max))
        .unwrap_or(fallback)
        .min(max)
}

/// Shared paging driver for `search`/`usage`: resolves records, decodes
/// the opaque per-session offset cursor, scans, and emits the common
/// envelope. `matcher` returns the matched field names or null.
fn run_paged_read(
    options: &CliOptions,
    command: &str,
    extra: Value,
    matcher: &EventMatcher,
    limit: usize,
) -> Result<Value, RunError> {
    let state = match load_state() {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("state_error"), None)),
    };
    let (records, note) = match select_records(&state, options) {
        Ok(v) => v,
        Err(e) => {
            return Err(fail(
                &e,
                1,
                Some("session_not_found"),
                Some(
                    json!({ "hint": "run `index` to build the transcript index, or check the selector" }),
                ),
            ));
        }
    };

    let mut decoded: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(cursor) = &options.cursor {
        match read::decode_offset_cursor(cursor) {
            Some(map) => decoded = map,
            None => {
                return Err(fail(
                    &format!("invalid --cursor: {}", cursor),
                    2,
                    None,
                    None,
                ))
            }
        }
    }

    let live_ctx = live_context(options);
    let targets: Vec<ScanTarget> = records
        .iter()
        .map(|r| ScanTarget {
            key: r.key.clone(),
            offset: decoded.get(&r.key).copied().unwrap_or(0),
        })
        .collect();
    let scan = match read::scan_paged_sessions(&targets, limit, matcher) {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("read_failed"), None)),
    };

    let sessions: Vec<Value> = records
        .iter()
        .map(|r| session_entry(r, live_ctx.as_ref(), true))
        .collect();

    let results: Vec<Value> = scan
        .matches
        .iter()
        .map(|m| {
            json!({
                "session_key": m.key,
                "event_id": m.event.get("event_id").cloned().unwrap_or(Value::Null),
                "type": m.event.get("type").cloned().unwrap_or(Value::Null),
                "timestamp": m.event.get("timestamp").cloned().unwrap_or(Value::Null),
                "turn_index": m.event.get("turn_index").cloned().unwrap_or(Value::Null),
                "matched_fields": m.fields,
                "event": m.event,
            })
        })
        .collect();

    let mut payload = match extra {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    payload.insert("contract".to_string(), json!(CONTRACT));
    payload.insert("command".to_string(), json!(command));
    payload.insert(
        "selector_note".to_string(),
        note.map(Value::String).unwrap_or(Value::Null),
    );
    payload.insert("results".to_string(), json!(results));
    payload.insert(
        "page".to_string(),
        json!({
            "limit": limit,
            "returned": scan.matches.len(),
            "next_cursor": if scan.has_more {
                Value::String(read::encode_offset_cursor(&scan.offsets))
            } else {
                Value::Null
            },
            "has_more": scan.has_more,
            "malformed_lines": scan.malformed_lines,
            "duplicates_skipped": scan.duplicates_skipped,
            "restarted_sessions": scan.restarted,
        }),
    );
    payload.insert("sessions".to_string(), json!(sessions));
    if state.sessions.is_empty() {
        payload.insert(
            "hint".to_string(),
            json!("no indexed sessions; run `index` to build the transcript index"),
        );
    }
    payload.insert("generated_at".to_string(), json!(now()));
    Ok(Value::Object(payload))
}

/// Case-insensitive substring check matching TS
/// `value.toLowerCase().includes(needle)` with `needle` already
/// lowercased. ASCII needle+haystack takes an allocation-free byte
/// path (the hot loop scans every event); anything else falls back
/// to the Unicode-aware lowercase comparison.
fn field_contains_ci(value: &str, needle_lower: &str) -> bool {
    if needle_lower.is_ascii() && value.is_ascii() {
        return ascii_contains_lower(value.as_bytes(), needle_lower.as_bytes());
    }
    value.to_lowercase().contains(needle_lower)
}

/// `hay` contains `needle` ignoring ASCII case. `needle` must already
/// be lowercase ASCII. `memchr2` on the first byte (both cases) makes
/// the scan O(n) with SIMD memchr; candidate windows verify with
/// `eq_ignore_ascii_case`.
fn ascii_contains_lower(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let lo = needle[0].to_ascii_lowercase();
    let hi = needle[0].to_ascii_uppercase();
    let mut i = 0usize;
    // Positions where a match may START: i in 0..=hay.len()-n.
    while i + n <= hay.len() {
        let window = &hay[i..=hay.len() - n];
        let Some(pos) = memchr::memchr2(lo, hi, window) else {
            return false;
        };
        let start = i + pos;
        if hay[start..start + n].eq_ignore_ascii_case(needle) {
            return true;
        }
        i = start + 1;
    }
    false
}

fn cmd_search(options: &CliOptions) -> Result<Value, RunError> {
    let query = positional_args(options).join(" ");
    let query = query.trim().to_string();
    if query.is_empty() {
        return Err(fail("search requires a query", 2, None, None));
    }
    if let Some(t) = &options.event_type {
        if !EVENT_TYPES.contains(&t.as_str()) {
            return Err(fail(
                &format!(
                    "invalid --type: {} (expected one of {})",
                    t,
                    EVENT_TYPES.join(", ")
                ),
                2,
                None,
                None,
            ));
        }
    }
    let needle = query.to_lowercase();
    let type_filter = options.event_type.clone();
    // Case-insensitive substring match over the searchable payload
    // fields; no tokenization in v1 (see docs/specs/conversations.md).
    // TS `toLowerCase()` is Unicode-aware; Rust `to_lowercase` matches.
    let matcher = move |event: &Value| -> Option<Vec<String>> {
        if let Some(tf) = &type_filter {
            if event.get("type").and_then(|v| v.as_str()) != Some(tf.as_str()) {
                return None;
            }
        }
        let mut fields = Vec::new();
        for field in SEARCHABLE_FIELDS {
            if let Some(v) = event.get(field).and_then(|v| v.as_str()) {
                if field_contains_ci(v, &needle) {
                    fields.push(field.to_string());
                }
            }
        }
        if fields.is_empty() {
            None
        } else {
            Some(fields)
        }
    };
    run_paged_read(
        options,
        "search",
        json!({
            "query": query,
            "filters": {
                "pane_id": options.pane,
                "session_id": options.session_id,
                "provider": options.provider,
                "type": options.event_type,
            },
        }),
        &matcher,
        read_limit(options, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT),
    )
}

fn cmd_usage(options: &CliOptions) -> Result<Value, RunError> {
    // Read surface for #28: pages only events carrying provider-reported
    // usage; aggregation stays consumer-side.
    let matcher = |event: &Value| -> Option<Vec<String>> {
        match event.get("usage") {
            Some(u) if u.is_object() => Some(vec!["usage".to_string()]),
            _ => None,
        }
    };
    run_paged_read(
        options,
        "usage",
        json!({
            "filters": {
                "pane_id": options.pane,
                "session_id": options.session_id,
                "provider": options.provider,
            },
        }),
        &matcher,
        read_limit(options, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT),
    )
}

fn cmd_show(options: &CliOptions) -> Result<Value, RunError> {
    let reference = positional_args(options).join(" ");
    let reference = reference.trim().to_string();
    if reference.is_empty() {
        return Err(fail(
            "show requires a reference (event id, tool:<id>, or turn:<n>)",
            2,
            None,
            None,
        ));
    }

    let turn_re = regex::Regex::new(r"^turn:(\d+)$").unwrap();
    let tool_re = regex::Regex::new(r"^tool:(.+)$").unwrap();
    #[allow(clippy::type_complexity)]
    let (resolved_kind, matcher): (&str, Box<dyn Fn(&Value) -> Option<Vec<String>>>) =
        if let Some(caps) = turn_re.captures(&reference) {
            let turn_index: i64 = caps[1].parse().unwrap_or(0);
            (
                "turn_index",
                Box::new(move |event: &Value| {
                    // TS `event.turn_index === turnIndex` — strict number
                    // equality; JSON numbers compare as f64.
                    match event.get("turn_index").and_then(|v| v.as_f64()) {
                        Some(n) if n == turn_index as f64 => Some(vec!["turn_index".to_string()]),
                        _ => None,
                    }
                }),
            )
        } else if let Some(caps) = tool_re.captures(&reference) {
            let tool_id = caps[1].to_string();
            (
                "tool_use_id",
                Box::new(move |event: &Value| {
                    if event.get("tool_use_id").and_then(|v| v.as_str()) == Some(tool_id.as_str()) {
                        Some(vec!["tool_use_id".to_string()])
                    } else {
                        None
                    }
                }),
            )
        } else {
            let reference = reference.clone();
            (
                "event_id",
                Box::new(move |event: &Value| {
                    if event.get("event_id").and_then(|v| v.as_str()) == Some(reference.as_str()) {
                        return Some(vec!["event_id".to_string()]);
                    }
                    if event.get("tool_use_id").and_then(|v| v.as_str()) == Some(reference.as_str())
                    {
                        return Some(vec!["tool_use_id".to_string()]);
                    }
                    None
                }),
            )
        };

    let state = match load_state() {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("state_error"), None)),
    };
    let (records, note) = match select_records(&state, options) {
        Ok(v) => v,
        Err(e) => {
            return Err(fail(
                &e,
                1,
                Some("session_not_found"),
                Some(
                    json!({ "hint": "run `index` to build the transcript index, or check the selector" }),
                ),
            ));
        }
    };

    let live_ctx = live_context(options);
    let targets: Vec<ScanTarget> = records
        .iter()
        .map(|r| ScanTarget {
            key: r.key.clone(),
            offset: 0,
        })
        .collect();
    let scan = match read::scan_paged_sessions(&targets, MAX_SHOW_RESULTS, &*matcher) {
        Ok(s) => s,
        Err(e) => return Err(fail(&e, 1, Some("read_failed"), None)),
    };

    // A bare ref may match tool_use_id when no event_id matched; report
    // the kind actually observed rather than the assumed one.
    let mut observed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in &scan.matches {
        for f in &m.fields {
            observed.insert(f.as_str());
        }
    }
    let kind = if resolved_kind == "event_id"
        && !observed.contains("event_id")
        && observed.contains("tool_use_id")
    {
        "tool_use_id"
    } else {
        resolved_kind
    };

    let sessions: Vec<Value> = records
        .iter()
        .map(|r| session_entry(r, live_ctx.as_ref(), true))
        .collect();
    let results: Vec<Value> = scan
        .matches
        .iter()
        .map(|m| {
            json!({
                "session_key": m.key,
                "matched_fields": m.fields,
                "event": m.event,
            })
        })
        .collect();

    let mut payload = json!({
        "contract": CONTRACT,
        "command": "show",
        "ref": reference,
        "resolved": { "kind": kind },
        "selector_note": note,
        "results": results,
        "matched": scan.matches.len(),
        "truncated": scan.has_more,
        "sessions": sessions,
    });
    if state.sessions.is_empty() {
        payload.as_object_mut().unwrap().insert(
            "hint".to_string(),
            json!("no indexed sessions; run `index` to build the transcript index"),
        );
    }
    payload
        .as_object_mut()
        .unwrap()
        .insert("generated_at".to_string(), json!(now()));
    Ok(payload)
}

// --- entrypoint -------------------------------------------------------------------

fn emit_or_fail(result: Result<Value, RunError>) -> i32 {
    match result {
        Ok(value) => {
            emit(&value);
            0
        }
        Err(e) => {
            eprintln!("{}", e.stderr_doc());
            e.code
        }
    }
}

/// Dispatch a domain command in-process — the same `cmd_*` cores the
/// CLI entrypoint uses. The TUI calls this for reads; index/refresh
/// are reachable but pane-scoped commands always carry explicit
/// selectors so ambient HERDR_* context can never become a target.
pub fn run_domain(args: &[String]) -> Result<Value, RunError> {
    let options = parse_args(args);
    match options.command.as_str() {
        "capability" => cmd_capability(),
        "index" | "refresh" => cmd_index(&options),
        "sessions" => cmd_sessions(&options),
        "history" => cmd_history(&options),
        "search" => cmd_search(&options),
        "show" => cmd_show(&options),
        "usage" => cmd_usage(&options),
        "help" | "--help" | "-h" => Ok(json!({
            "contract": CONTRACT,
            "plugin": PLUGIN_ID,
            "version": PLUGIN_VERSION,
            "usage": "conversations <capability|index|refresh|sessions|history|search|show|usage> [--session NAME] [--session-id ID] [--pane PANE] [--provider ID] [--type TYPE] [--limit N] [--cursor CUR] [--socket PATH] [--timeout MS] [--offline]",
        })),
        other => Err(RunError::new(format!("unknown command: {}", other), 2)),
    }
}

pub fn run(argv: &[String]) -> i32 {
    emit_or_fail(run_domain(argv))
}

fn cmd_capability() -> Result<Value, RunError> {
    Ok(json!({
        "contract": CONTRACT,
        "command": "capability",
        "plugin": PLUGIN_ID,
        "version": PLUGIN_VERSION,
        "read_only": false,
        "mutation_commands": ["index", "refresh"],
        "commands": ["capability", "index", "refresh", "sessions", "history", "search", "show", "usage"],
        "providers": PROVIDERS,
        "event_types": EVENT_TYPES,
        "notes": [
            "Transcript resolution uses agent_session.value (session id) + pane cwd + provider-known directories; agent_session_path is not exposed by herdr AgentInfo (upstream gap, ADR-0012).",
            "PTY text is never read; unsupported providers and missing transcripts are reported explicitly.",
            "Normalized events are appended to <stateDir>/events/<provider>:<session>.jsonl; `history`/`search`/`usage` page them with opaque cursors, `show` resolves stable event/tool/turn references (T3 #88).",
            "opencode sessions are fully rescanned and their event log rewritten on each index (SQLite rows are mutable); file-backed providers scan incrementally.",
        ],
        "generated_at": now(),
    }))
}
