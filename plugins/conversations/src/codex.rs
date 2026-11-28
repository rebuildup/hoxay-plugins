//! Codex rollout transcript adapter (provider id `codex`).
//!
//! Rust port of `plugins/conversations/src/codex.ts` (retired; git
//! history preserves it).
//!
//! Transcript location (verified against codex CLI 0.169.x):
//!   $CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<session-id>.jsonl
//!   (default home: ~/.codex). `archived_sessions/` holds `.jsonl.zst`
//!   compressed rollouts — not readable by the line scanner, so they
//!   resolve as missing rather than being decompressed silently.
//!
//! Line format: one JSON object per line
//! `{timestamp, ordinal, type, payload}`. Normalized:
//!   event_msg / item_completed UserMessage|AgentMessage
//!                                    → turn + user_message | assistant_message
//!   event_msg / user_message|agent_message (legacy payloads)
//!                                    → same, for older rollouts
//!   response_item function_call|custom_tool_call (+ *_output)
//!                                    → tool_call | tool_result
//!   event_msg / token_count, token_usage_record
//!                                    → usage attached to the turn's
//!                                      last assistant_message
//! Everything else is provider bookkeeping — skipped, never fabricated.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude::bounded;
use crate::indexer::LineParser;
use crate::model::{ConversationEvent, UsageFields};

/// Bound stored text/payload so the index stays navigational, not archival.
pub const CODEX_MAX_TEXT_CHARS: usize = 16_384;
pub const CODEX_MAX_INPUT_CHARS: usize = 8_192;

/// sessions/YYYY/MM/DD/file.jsonl is depth 4; allow slack for variants.
const MAX_WALK_DEPTH: usize = 6;
const MAX_WALK_DIRS: usize = 20_000;

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

pub fn codex_home_dir() -> PathBuf {
    if let Some(configured) = env_var("CODEX_HOME") {
        // TS path.resolve(): relative values resolve against cwd.
        let p = PathBuf::from(configured);
        if p.is_absolute() {
            return p;
        }
        return std::env::current_dir().map(|cwd| cwd.join(&p)).unwrap_or(p);
    }
    let home = env_var("HOME").unwrap_or_else(|| "/".to_string());
    Path::new(&home).join(".codex")
}

fn file_exists(file: &Path) -> bool {
    std::fs::metadata(file)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

fn find_session_file(
    root: &Path,
    session_id: &str,
    depth: usize,
    budget: &mut usize,
) -> Option<PathBuf> {
    if depth > MAX_WALK_DEPTH || *budget == 0 {
        return None;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return None,
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => dirs.push(entry.path()),
            Ok(ft) if ft.is_file() => files.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )),
            _ => {}
        }
    }
    // Files first: rollout files live at the leaves, dirs at the top.
    for (name, path) in files {
        if !name.ends_with(".jsonl") {
            continue;
        }
        if name.contains(session_id) {
            return Some(path);
        }
    }
    for dir in dirs {
        *budget = budget.saturating_sub(1);
        if *budget == 0 {
            return None;
        }
        if let Some(found) = find_session_file(&dir, session_id, depth + 1, budget) {
            return Some(found);
        }
    }
    None
}

/// Resolve the rollout file for a codex session id. Returns None when
/// nothing matches — callers must report `missing`, never guess.
pub fn resolve_codex_transcript(session_id: &str, _cwd: Option<&str>) -> Option<String> {
    // Defensive: a value that is already a readable file path.
    if (session_id.contains('/') || session_id.contains('\\')) && file_exists(Path::new(session_id))
    {
        return Some(session_id.to_string());
    }
    let home = codex_home_dir();
    let mut budget = MAX_WALK_DIRS;
    for sub in ["sessions", "archived_sessions"] {
        if let Some(found) = find_session_file(&home.join(sub), session_id, 0, &mut budget) {
            return Some(found.to_string_lossy().into_owned());
        }
    }
    None
}

// --- Normalization -----------------------------------------------------------

fn as_record(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn token_count(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.is_finite() && f > 0.0 {
                f.floor() as i64
            } else {
                0
            }
        }
        _ => 0,
    }
}

fn iso_timestamp(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Codex token usage → contract UsageFields (input includes cached).
fn usage_from(raw: Option<&Value>) -> Option<UsageFields> {
    let usage = as_record(raw?)?;
    let input = token_count(usage.get("input_tokens"));
    let output = token_count(usage.get("output_tokens"));
    let cache_read = token_count(usage.get("cached_input_tokens"));
    let cache_write = token_count(usage.get("cache_write_input_tokens"));
    let reasoning = token_count(usage.get("reasoning_output_tokens"));
    if input + output + cache_read + cache_write + reasoning == 0 {
        return None;
    }
    let mut result = UsageFields {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        thinking_tokens: None,
    };
    if reasoning > 0 {
        result.thinking_tokens = Some(reasoning);
    }
    Some(result)
}

/// Join `[{type:"text",text}]`-style content blocks into bounded text.
fn content_text(content: Option<&Value>, max: usize) -> (String, bool) {
    match content {
        Some(Value::String(s)) => bounded(s, max),
        _ => {
            let blocks = content.and_then(|c| c.as_array());
            let mut parts: Vec<String> = Vec::new();
            let mut truncated = false;
            if let Some(blocks) = blocks {
                for block in blocks {
                    let Some(b) = as_record(block) else { continue };
                    let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if matches!(ty, "text" | "Text" | "input_text" | "output_text") {
                        if let Some(Value::String(text)) = b.get("text") {
                            let (part, t) = bounded(text, max);
                            parts.push(part);
                            truncated = truncated || t;
                        }
                    }
                }
            }
            let joined = parts.join("\n");
            let (cap, t2) = bounded(&joined, max);
            (cap, truncated || t2)
        }
    }
}

/// tool_input payload: JSON string arguments or object input.
fn tool_input_from(raw: Option<&Value>) -> (Value, bool) {
    let json = match raw {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
        None => "null".to_string(),
    };
    let (cap, truncated) = bounded(&json, CODEX_MAX_INPUT_CHARS);
    match serde_json::from_str::<Value>(&cap) {
        Ok(v) => (v, truncated),
        Err(_) => (Value::String(cap), truncated),
    }
}

struct ParseContext {
    identity: Value,
    file_name: String,
    turn_index: i64,
    seen_turn: bool,
    /// Model carried from the latest turn_context record.
    model: Option<String>,
    /// Request-ledger mode: token_usage_record seen → token_count is stale.
    usage_ledger_active: bool,
}

impl ParseContext {
    fn current_turn(&self) -> Option<i64> {
        if self.seen_turn {
            Some(self.turn_index)
        } else {
            None
        }
    }

    fn base_event(
        &self,
        kind: &str,
        record: &serde_json::Map<String, Value>,
        turn_index: Option<i64>,
        event_id: String,
    ) -> ConversationEvent {
        ConversationEvent::base(
            kind,
            iso_timestamp(record.get("timestamp")),
            self.identity.clone(),
            turn_index,
            event_id,
        )
    }
}

/// Stateful codex line parser for `scan_transcript`. `last_assistant`
/// tracks the index (into the scan's event sink) of the most recent
/// assistant_message so usage records can attach retroactively —
/// mirroring the TS `ctx.lastAssistant` object reference.
pub struct CodexLineParser {
    ctx: ParseContext,
    last_assistant: Option<usize>,
    last_assistant_turn: Option<i64>,
}

impl CodexLineParser {
    pub fn new(file_name: &str, identity: Value, start_turn: i64) -> Self {
        CodexLineParser {
            ctx: ParseContext {
                identity,
                file_name: file_name.to_string(),
                turn_index: start_turn,
                seen_turn: start_turn > 0,
                model: None,
                usage_ledger_active: false,
            },
            last_assistant: None,
            last_assistant_turn: None,
        }
    }

    /// Attach per-request usage to the current turn's last assistant message.
    fn attach_usage(&mut self, usage: UsageFields, out: &mut [ConversationEvent]) {
        let (Some(idx), Some(turn)) = (self.last_assistant, self.last_assistant_turn) else {
            return;
        };
        if Some(turn) != self.ctx.current_turn() {
            return;
        }
        if let Some(event) = out.get_mut(idx) {
            event.usage = Some(usage);
        }
    }
}

impl LineParser for CodexLineParser {
    fn parse_line(&mut self, line: &str, line_number: u64, out: &mut Vec<ConversationEvent>) {
        parse_codex_line(line, line_number, self, out);
    }
    fn turn_index(&self) -> i64 {
        self.ctx.turn_index
    }
}

fn parse_codex_line(
    line: &str,
    line_number: u64,
    parser: &mut CodexLineParser,
    out: &mut Vec<ConversationEvent>,
) {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(record) = as_record(&parsed) else {
        return;
    };
    let Some(payload) = record.get("payload").and_then(as_record) else {
        return;
    };

    let fallback_id = format!("{}:{}", parser.ctx.file_name, line_number);
    let payload_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    match record.get("type").and_then(|t| t.as_str()) {
        Some("event_msg") => parse_event_msg(
            record,
            payload,
            parser,
            &fallback_id,
            payload_id.as_deref(),
            out,
        ),
        Some("response_item") => parse_response_item(
            record,
            payload,
            &parser.ctx,
            &fallback_id,
            payload_id.as_deref(),
            out,
        ),
        Some("turn_context") => {
            parser.ctx.usage_ledger_active = false;
            if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
                if !model.is_empty() {
                    parser.ctx.model = Some(model.to_string());
                }
            }
        }
        Some("token_usage_record") => {
            if let Some(usage) = usage_from(payload.get("usage")) {
                parser.attach_usage(usage, out);
            }
            parser.ctx.usage_ledger_active = true;
        }
        // session_meta, world_state, ghost_snapshot, …: bookkeeping.
        _ => {}
    }
}

fn parse_event_msg(
    record: &serde_json::Map<String, Value>,
    payload: &serde_json::Map<String, Value>,
    parser: &mut CodexLineParser,
    fallback_id: &str,
    payload_id: Option<&str>,
    out: &mut Vec<ConversationEvent>,
) {
    let kind = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // Legacy payloads (pre item_completed rollouts).
    if kind == "user_message" {
        user_turn_events(
            record,
            payload.get("message"),
            &mut parser.ctx,
            payload_id.unwrap_or(fallback_id),
            out,
        );
        return;
    }
    if kind == "agent_message" {
        push_assistant(
            parser,
            record,
            payload.get("message"),
            payload_id.unwrap_or(fallback_id),
            out,
        );
        return;
    }

    if kind == "item_completed" {
        let Some(item) = payload.get("item").and_then(as_record) else {
            return;
        };
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| payload_id.map(|s| s.to_string()))
            .unwrap_or_else(|| fallback_id.to_string());
        match item.get("type").and_then(|t| t.as_str()) {
            Some("UserMessage") => {
                user_turn_events(record, item.get("content"), &mut parser.ctx, &item_id, out);
            }
            Some("AgentMessage") => {
                push_assistant(parser, record, item.get("content"), &item_id, out);
            }
            // Reasoning / ImageView / CommandExecution / … skipped.
            _ => {}
        }
        return;
    }

    if kind == "token_count" {
        // Context snapshots are cumulative/stale once the request ledger
        // is active; otherwise last_token_usage is the most recent request.
        if parser.ctx.usage_ledger_active {
            return;
        }
        if let Some(usage) = usage_from(
            payload
                .get("info")
                .and_then(as_record)
                .and_then(|i| i.get("last_token_usage")),
        ) {
            parser.attach_usage(usage, out);
        }
    }
    // task_started / task_complete / agent_reasoning / … : bookkeeping.
}

fn user_turn_events(
    record: &serde_json::Map<String, Value>,
    content: Option<&Value>,
    ctx: &mut ParseContext,
    base_id: &str,
    out: &mut Vec<ConversationEvent>,
) {
    ctx.turn_index += 1;
    ctx.seen_turn = true;
    out.push(ctx.base_event(
        "turn",
        record,
        Some(ctx.turn_index),
        format!("{}:turn", base_id),
    ));
    let mut message = ctx.base_event(
        "user_message",
        record,
        Some(ctx.turn_index),
        base_id.to_string(),
    );
    let (text, truncated) = content_text(content, CODEX_MAX_TEXT_CHARS);
    message.text = Some(text);
    if truncated {
        message.text_truncated = Some(true);
    }
    out.push(message);
}

/// Emit an assistant_message and register it for retroactive usage
/// attachment (the TS `ctx.lastAssistant` object reference).
fn push_assistant(
    parser: &mut CodexLineParser,
    record: &serde_json::Map<String, Value>,
    content: Option<&Value>,
    event_id: &str,
    out: &mut Vec<ConversationEvent>,
) {
    let ctx = &parser.ctx;
    let mut event = ctx.base_event(
        "assistant_message",
        record,
        ctx.current_turn(),
        event_id.to_string(),
    );
    let (text, truncated) = content_text(content, CODEX_MAX_TEXT_CHARS);
    if !text.is_empty() {
        event.text = Some(text);
        if truncated {
            event.text_truncated = Some(true);
        }
    }
    if let Some(model) = &ctx.model {
        event.model = Some(model.clone());
    }
    let turn = event.turn_index;
    out.push(event);
    parser.last_assistant = Some(out.len() - 1);
    parser.last_assistant_turn = turn;
}

const TOOL_CALL_TYPES: &[&str] = &["function_call", "custom_tool_call", "local_shell_call"];
const TOOL_OUTPUT_TYPES: &[&str] = &[
    "function_call_output",
    "custom_tool_call_output",
    "local_shell_call_output",
];

fn parse_response_item(
    record: &serde_json::Map<String, Value>,
    payload: &serde_json::Map<String, Value>,
    ctx: &ParseContext,
    fallback_id: &str,
    payload_id: Option<&str>,
    out: &mut Vec<ConversationEvent>,
) {
    let kind = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");

    let is_tool_call = TOOL_CALL_TYPES.contains(&kind) || kind.ends_with("_tool_call");
    let is_tool_output = TOOL_OUTPUT_TYPES.contains(&kind)
        || kind.ends_with("_tool_call_output")
        || kind == "function_call_output";

    if is_tool_call {
        let mut event = ctx.base_event(
            "tool_call",
            record,
            ctx.current_turn(),
            payload_id.unwrap_or(fallback_id).to_string(),
        );
        // TS `payload.call_id` truthiness: empty string falls through.
        let call_id = payload
            .get("call_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(id) = call_id {
            event.tool_use_id = Some(id.to_string());
        } else if let Some(id) = payload_id {
            event.tool_use_id = Some(id.to_string());
        }
        if let Some(name) = payload.get("name").and_then(|v| v.as_str()) {
            event.tool_name = Some(name.to_string());
        }
        // TS `payload.arguments ?? payload.input`: `??` falls on null too.
        let input_raw = payload
            .get("arguments")
            .filter(|v| !v.is_null())
            .or_else(|| payload.get("input").filter(|v| !v.is_null()));
        let (input, truncated) = tool_input_from(input_raw);
        event.tool_input = Some(input);
        if truncated {
            event.input_truncated = Some(true);
        }
        out.push(event);
        return;
    }

    if is_tool_output {
        let mut event = ctx.base_event(
            "tool_result",
            record,
            ctx.current_turn(),
            payload_id.unwrap_or(fallback_id).to_string(),
        );
        let call_id = payload
            .get("call_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(id) = call_id {
            event.tool_use_id = Some(id.to_string());
        } else if let Some(id) = payload_id {
            event.tool_use_id = Some(id.to_string());
        }
        let (text, truncated) = content_text(payload.get("output"), CODEX_MAX_TEXT_CHARS);
        event.text = Some(text);
        if truncated {
            event.text_truncated = Some(true);
        }
        out.push(event);
    }
    // message / reasoning / additional_tools / … skipped (see header).
}
