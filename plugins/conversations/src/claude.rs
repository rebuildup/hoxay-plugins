//! Claude Code transcript adapter (provider id `claude`).
//!
//! Rust port of `plugins/conversations/src/claude.ts` (retired; git
//! history preserves it).
//!
//! Transcript location (verified against claude 2.1.x):
//!   $CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/<session-id>.jsonl
//!   (default config dir: ~/.claude). The directory name is the cwd with
//!   every non [A-Za-z0-9] byte replaced by `-`; the file name is the
//!   session UUID — which is exactly what herdr reports as
//!   `agent_session.value` (a session id, NOT a path).
//!
//! Line format: one JSON object per line. Types we normalize:
//!   user (string or block content)     → turn + user_message | tool_result
//!   assistant (message.content blocks) → assistant_message (+usage) | tool_call
//!   system                             → system (subtype carried)
//! Everything else is provider bookkeeping — skipped, never fabricated.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::indexer::LineParser;
use crate::model::{ConversationEvent, UsageFields};

/// Bound stored text/payload so the index stays navigational, not archival.
pub const MAX_TEXT_CHARS: usize = 16_384;
pub const MAX_INPUT_CHARS: usize = 8_192;

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

pub fn claude_projects_dir() -> PathBuf {
    let configured = env_var("CLAUDE_CONFIG_DIR");
    let home = configured.unwrap_or_else(|| {
        let home = env_var("HOME").unwrap_or_else(|| "/".to_string());
        Path::new(&home)
            .join(".claude")
            .to_string_lossy()
            .into_owned()
    });
    Path::new(&home).join("projects")
}

/// Claude's cwd→directory encoding: non [A-Za-z0-9] → `-`. JS regex
/// semantics operate on UTF-16 code units, so a non-BMP char yields
/// one `-` per surrogate half — mirror that exactly.
pub fn encode_claude_cwd(cwd: &str) -> String {
    cwd.encode_utf16()
        .map(|u| {
            if (u as u32) < 128 && (u as u8 as char).is_ascii_alphanumeric() {
                char::from_u32(u as u32).unwrap()
            } else {
                '-'
            }
        })
        .collect()
}

fn file_exists(file: &Path) -> bool {
    std::fs::metadata(file)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Resolve the transcript file for a claude session id. Returns None
/// when nothing matches — callers must report `missing`, never guess.
pub fn resolve_claude_transcript(session_id: &str, cwd: Option<&str>) -> Option<String> {
    // Defensive: a value that is already a readable file path.
    if (session_id.contains('/') || session_id.contains('\\')) && file_exists(Path::new(session_id))
    {
        return Some(session_id.to_string());
    }
    let projects_dir = claude_projects_dir();
    let file_name = format!("{}.jsonl", session_id);
    if let Some(cwd) = cwd {
        if !cwd.is_empty() {
            let direct = projects_dir.join(encode_claude_cwd(cwd)).join(&file_name);
            if file_exists(&direct) {
                return Some(direct.to_string_lossy().into_owned());
            }
        }
    }
    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return None,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let candidate = entry.path().join(&file_name);
        if file_exists(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

// --- Normalization -----------------------------------------------------------

/// Bound `text` to `max` UTF-16 code units (TS `String.length`/`slice`
/// semantics). Returns (bounded_text, truncated).
pub fn bounded(text: &str, max: usize) -> (String, bool) {
    let mut units = 0usize;
    for (i, ch) in text.char_indices() {
        let w = ch.len_utf16();
        if units + w > max {
            return (text[..i].to_string(), true);
        }
        units += w;
    }
    (text.to_string(), false)
}

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

fn usage_from(raw: Option<&Value>) -> Option<UsageFields> {
    let usage = as_record(raw?)?;
    let input = token_count(usage.get("input_tokens"));
    let output = token_count(usage.get("output_tokens"));
    let cache_read = token_count(usage.get("cache_read_input_tokens"));
    let cache_create = token_count(usage.get("cache_creation_input_tokens"));
    if input + output + cache_read + cache_create == 0 {
        return None;
    }
    let mut result = UsageFields {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_create,
        thinking_tokens: None,
    };
    let thinking = usage
        .get("output_tokens_details")
        .and_then(as_record)
        .map(|d| token_count(d.get("thinking_tokens")))
        .unwrap_or(0);
    if thinking > 0 {
        result.thinking_tokens = Some(thinking);
    }
    Some(result)
}

/// Join `[{type:"text",text}]`-style content into bounded text.
pub fn content_text(content: Option<&Value>) -> (String, bool) {
    match content {
        Some(Value::String(s)) => bounded(s, MAX_TEXT_CHARS),
        _ => {
            let blocks = content.and_then(|c| c.as_array());
            let mut parts: Vec<String> = Vec::new();
            let mut truncated = false;
            if let Some(blocks) = blocks {
                for block in blocks {
                    let Some(b) = as_record(block) else { continue };
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(Value::String(text)) = b.get("text") {
                            let (part, t) = bounded(text, MAX_TEXT_CHARS);
                            parts.push(part);
                            truncated = truncated || t;
                        }
                    }
                }
            }
            let joined = parts.join("\n");
            let (cap, t2) = bounded(&joined, MAX_TEXT_CHARS);
            (cap, truncated || t2)
        }
    }
}

struct ParseContext {
    identity: Value,
    file_name: String,
    /// Mutable turn counter threaded through the file in line order.
    turn_index: i64,
    /// Events before the first user prompt get turn_index null.
    seen_turn: bool,
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
        let mut event = ConversationEvent::base(
            kind,
            iso_timestamp(record.get("timestamp")),
            self.identity.clone(),
            turn_index,
            event_id,
        );
        if record.get("isSidechain") == Some(&Value::Bool(true)) {
            event.sidechain = Some(true);
        }
        event
    }

    fn event_id_for(
        &self,
        record: &serde_json::Map<String, Value>,
        line_number: u64,
        suffix: &str,
    ) -> String {
        let base = record
            .get("uuid")
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}:{}", self.file_name, line_number));
        if suffix.is_empty() {
            base
        } else {
            format!("{}{}", base, suffix)
        }
    }
}

/// Stateful claude line parser for `scan_transcript`.
pub struct ClaudeLineParser {
    ctx: ParseContext,
}

impl ClaudeLineParser {
    pub fn new(file_name: &str, identity: Value, start_turn: i64) -> Self {
        ClaudeLineParser {
            ctx: ParseContext {
                identity,
                file_name: file_name.to_string(),
                turn_index: start_turn,
                seen_turn: start_turn > 0,
            },
        }
    }
}

impl LineParser for ClaudeLineParser {
    fn parse_line(&mut self, line: &str, line_number: u64, out: &mut Vec<ConversationEvent>) {
        out.extend(parse_claude_line(line, line_number, &mut self.ctx));
    }
    fn turn_index(&self) -> i64 {
        self.ctx.turn_index
    }
}

/// Parse ONE claude transcript line into zero or more normalized events.
fn parse_claude_line(
    line: &str,
    line_number: u64,
    ctx: &mut ParseContext,
) -> Vec<ConversationEvent> {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(record) = as_record(&parsed) else {
        return Vec::new();
    };
    match record.get("type").and_then(|t| t.as_str()) {
        Some("user") => parse_user_line(record, ctx, line_number),
        Some("assistant") => parse_assistant_line(record, ctx, line_number),
        Some("system") => {
            let mut event = ctx.base_event(
                "system",
                record,
                ctx.current_turn(),
                ctx.event_id_for(record, line_number, ""),
            );
            if let Some(subtype) = record.get("subtype").and_then(|s| s.as_str()) {
                event.subtype = Some(subtype.to_string());
            }
            vec![event]
        }
        _ => Vec::new(),
    }
}

fn parse_user_line(
    record: &serde_json::Map<String, Value>,
    ctx: &mut ParseContext,
    line_number: u64,
) -> Vec<ConversationEvent> {
    let content = record
        .get("message")
        .and_then(as_record)
        .and_then(|m| m.get("content"));
    let blocks: Vec<&Value> = content
        .and_then(|c| c.as_array())
        .map(|v| v.iter().collect())
        .unwrap_or_default();
    // `content` may be a plain string — not an array → no tool_result blocks.
    let has_tool_result = blocks.iter().any(|b| {
        as_record(b)
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            == Some("tool_result")
    });

    if has_tool_result {
        // A user line carrying tool_result blocks is tool output, not a
        // user prompt — emit tool_result events only, no turn boundary.
        let mut events = Vec::new();
        let mut result_index = 0usize;
        for block in &blocks {
            let Some(b) = as_record(block) else { continue };
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            result_index += 1;
            let suffix = match b.get("tool_use_id").and_then(|v| v.as_str()) {
                Some(id) => format!(":result:{}", id),
                None => format!(":result:{}", result_index),
            };
            let mut event = ctx.base_event(
                "tool_result",
                record,
                ctx.current_turn(),
                ctx.event_id_for(record, line_number, &suffix),
            );
            if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                event.tool_use_id = Some(id.to_string());
            }
            if b.get("is_error") == Some(&Value::Bool(true)) {
                event.is_error = Some(true);
            }
            let (text, truncated) = content_text(b.get("content"));
            event.text = Some(text);
            if truncated {
                event.text_truncated = Some(true);
            }
            events.push(event);
        }
        return events;
    }

    // Genuine user prompt: turn boundary, then the message itself.
    ctx.turn_index += 1;
    ctx.seen_turn = true;
    let mut events = Vec::new();
    events.push(ctx.base_event(
        "turn",
        record,
        Some(ctx.turn_index),
        ctx.event_id_for(record, line_number, ""),
    ));
    let mut message_event = ctx.base_event(
        "user_message",
        record,
        Some(ctx.turn_index),
        ctx.event_id_for(record, line_number, ":message"),
    );
    let (text, truncated) = content_text(content);
    message_event.text = Some(text);
    if truncated {
        message_event.text_truncated = Some(true);
    }
    events.push(message_event);
    events
}

fn parse_assistant_line(
    record: &serde_json::Map<String, Value>,
    ctx: &mut ParseContext,
    line_number: u64,
) -> Vec<ConversationEvent> {
    let Some(message) = record.get("message").and_then(as_record) else {
        return Vec::new();
    };
    let content = message.get("content").and_then(|c| c.as_array());
    let empty: Vec<Value> = Vec::new();
    let blocks: &[Value] = content.map(|v| v.as_slice()).unwrap_or(&empty);

    let mut tool_events: Vec<ConversationEvent> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut text_truncated = false;
    let mut has_content = false;

    for block in blocks.iter() {
        let Some(b) = as_record(block) else { continue };
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(Value::String(text)) = b.get("text") {
                    has_content = true;
                    let (part, t) = bounded(text, MAX_TEXT_CHARS);
                    text_parts.push(part);
                    text_truncated = text_truncated || t;
                }
            }
            Some("tool_use") => {
                has_content = true;
                // TS fallback is `events.length` — tool events emitted
                // so far on this line, not the block index.
                let id_part = b
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| tool_events.len().to_string());
                let mut event = ctx.base_event(
                    "tool_call",
                    record,
                    ctx.current_turn(),
                    ctx.event_id_for(record, line_number, &format!(":tool:{}", id_part)),
                );
                if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                    event.tool_use_id = Some(id.to_string());
                }
                if let Some(name) = b.get("name").and_then(|v| v.as_str()) {
                    event.tool_name = Some(name.to_string());
                }
                let input_json =
                    serde_json::to_string(&b.get("input").cloned().unwrap_or(Value::Null))
                        .unwrap_or_else(|_| "null".to_string());
                let (input_text, truncated) = bounded(&input_json, MAX_INPUT_CHARS);
                event.tool_input = Some(
                    serde_json::from_str::<Value>(&input_text)
                        .unwrap_or_else(|_| Value::String(input_text.clone())),
                );
                if truncated {
                    event.input_truncated = Some(true);
                }
                tool_events.push(event);
            }
            // thinking/redacted_thinking blocks: provider internals.
            _ => {}
        }
    }

    // Assistant usage rides on the message-level event (v1 schema).
    let usage = usage_from(message.get("usage"));
    let cost = record.get("costUSD").and_then(|v| v.as_f64());
    // TS gate is `typeof cost === "number"` (NaN counts); assignment is
    // separately gated by Number.isFinite — JSON cannot carry NaN/Inf
    // anyway, so `is_some` is the faithful check.
    let has_cost = cost.is_some();
    if !has_content && usage.is_none() && !has_cost {
        return tool_events;
    }

    let (joined, joined_trunc) = bounded(&text_parts.join("\n"), MAX_TEXT_CHARS);
    let mut message_event = ctx.base_event(
        "assistant_message",
        record,
        ctx.current_turn(),
        ctx.event_id_for(record, line_number, ""),
    );
    if !joined.is_empty() {
        message_event.text = Some(joined);
        if joined_trunc || text_truncated {
            message_event.text_truncated = Some(true);
        }
    }
    if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
        if !model.is_empty() {
            message_event.model = Some(model.to_string());
        }
    }
    if let Some(u) = usage {
        message_event.usage = Some(u);
    }
    if let Some(c) = cost {
        if c.is_finite() {
            message_event.cost_usd = Some(c);
        }
    }
    // Message first, then its tool_call children — file order semantics.
    let mut events = vec![message_event];
    events.extend(tool_events);
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_claude_cwd_matches_claude_directory_encoding() {
        assert_eq!(
            encode_claude_cwd("/Users/rebui/.dotfiles"),
            "-Users-rebui--dotfiles"
        );
        assert_eq!(encode_claude_cwd("/repo/proj"), "-repo-proj");
        // JS regex semantics run on UTF-16 code units: a non-BMP char is
        // a surrogate pair → one '-' per half.
        assert_eq!(encode_claude_cwd("/a\u{1F600}b"), "-a--b");
        assert_eq!(encode_claude_cwd(""), "");
    }

    #[test]
    fn bounded_uses_utf16_code_units() {
        // "é" is one UTF-16 unit; "😀" is two.
        assert_eq!(bounded("abc", 2), ("ab".to_string(), true));
        assert_eq!(bounded("abc", 3), ("abc".to_string(), false));
        assert_eq!(bounded("a\u{1F600}b", 3), ("a\u{1F600}".to_string(), true));
        assert_eq!(
            bounded("a\u{1F600}b", 4),
            ("a\u{1F600}b".to_string(), false)
        );
        // Boundary: max lands between the surrogate halves — the whole
        // scalar is dropped, never split into a lone surrogate.
        assert_eq!(bounded("a\u{1F600}b", 2), ("a".to_string(), true));
        assert_eq!(bounded("\u{1F600}", 1), ("".to_string(), true));
    }
}
