//! rebuildup.conversations — shared model types for the
//! `herdr-plugins.conversations/v1` contract (Issue #86 / parent #5 T1,
//! native Rust port for Issue #293).
//!
//! Mirrors `plugins/conversations/src/model.ts` (retired TS source;
//! git history preserves it). Events are carried as `serde_json::Value`
//! payloads so provider-specific optional fields round-trip verbatim —
//! the normalized event log (`events/<key>.jsonl`) stays byte-compatible
//! with what the TS implementation wrote.

use hoxay_herdr_client::snapshot::AgentInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const CONTRACT: &str = "herdr-plugins.conversations/v1";
pub const PLUGIN_ID: &str = "rebuildup.conversations";
pub const PLUGIN_VERSION: &str = "0.7.0";

/// Normalized event kinds emitted by provider adapters.
pub const EVENT_TYPES: [&str; 6] = [
    "turn",
    "user_message",
    "assistant_message",
    "tool_call",
    "tool_result",
    "system",
];

/// Fields `search` matches against (lowercased substring, see spec).
pub const SEARCHABLE_FIELDS: [&str; 4] = ["text", "tool_name", "subtype", "model"];

// --- ConversationEvent -------------------------------------------------------
//
// Field presence rules mirror the TS object literals exactly:
// `timestamp`/`turn_index`/`identity` are always present (null allowed);
// every other field is emitted only when set. `extra` preserves any
// field a newer writer added so re-serialization is lossless.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageFields {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// ISO-8601 timestamp; `null` when the provider omits it.
    pub timestamp: Option<String>,
    pub identity: Value,
    /// 1-based turn index; `null` pre-first-turn.
    pub turn_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageFields>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_f64_js_number"
    )]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ConversationEvent {
    /// Construct a base event the way every adapter's `baseEvent` does.
    pub fn base(
        kind: &str,
        timestamp: Option<String>,
        identity: Value,
        turn_index: Option<i64>,
        event_id: String,
    ) -> Self {
        ConversationEvent {
            event_id,
            kind: kind.to_string(),
            timestamp,
            identity,
            turn_index,
            sidechain: None,
            text: None,
            text_truncated: None,
            model: None,
            usage: None,
            cost_usd: None,
            tool_use_id: None,
            tool_name: None,
            tool_input: None,
            input_truncated: None,
            is_error: None,
            subtype: None,
            extra: Map::new(),
        }
    }

    /// Serialize to the single-line JSON written to `events/<key>.jsonl`.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// --- SessionRecord / ConversationsState --------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    #[default]
    Ok,
    Missing,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanCursor {
    pub offset: u64,
    pub size: u64,
    pub mtime_ms: f64,
    /// Newline-terminated lines committed so far (1-based numbering base).
    pub line_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// `<provider>:<session_id>` — stable across pane rebinds.
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub status: SessionStatus,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub identity: Option<Value>,
    #[serde(default)]
    pub cursor: Option<ScanCursor>,
    /// FNV-1a guard of bytes preceding `offset`; detects in-place rewrites.
    #[serde(default)]
    pub guard_hash: Option<u32>,
    /// Turn counter value at the cursor; keeps turn_index monotonic.
    #[serde(default)]
    pub turn_count: i64,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub first_event_at: Option<String>,
    #[serde(default)]
    pub last_event_at: Option<String>,
    #[serde(default)]
    pub last_indexed_at: Option<String>,
    /// Unknown fields preserved verbatim on round-trip (state.json is
    /// plugin-private but forward-compatible).
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationsState {
    pub version: u32,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionRecord>,
}

pub fn empty_state() -> ConversationsState {
    ConversationsState {
        version: 1,
        sessions: BTreeMap::new(),
    }
}

// --- provider helpers ---------------------------------------------------------

/// Normalize an agent/agent_session provider hint to a provider id.
pub fn provider_id_of(agent: Option<&AgentInfo>) -> Option<String> {
    let agent_session_provider = agent
        .and_then(|a| a.agent_session.as_ref())
        .and_then(|s| s.extra.get("provider"))
        .and_then(|v| v.as_str());
    let hints: Vec<Option<&str>> = vec![
        agent
            .and_then(|a| a.agent_session.as_ref())
            .and_then(|s| s.agent.as_deref()),
        agent
            .and_then(|a| a.agent_session.as_ref())
            .and_then(|s| s.kind.as_deref()),
        agent.and_then(|a| a.agent.as_deref()),
        agent_session_provider,
    ];
    for hint in hints.into_iter().flatten() {
        if hint.is_empty() {
            continue;
        }
        let lower = hint.to_lowercase();
        for id in ["claude", "codex", "opencode"] {
            if lower.contains(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// agent_session.value is a session id, never assumed to be a path.
pub fn agent_session_value(agent: Option<&AgentInfo>) -> Option<String> {
    agent
        .and_then(|a| a.agent_session.as_ref())
        .and_then(|s| s.value.clone())
        .filter(|v| !v.is_empty())
}

/// Live binding info `sessions` reports for staleness checks.
#[derive(Debug, Clone)]
pub struct LiveBinding {
    pub pane_id: String,
    pub agent_session_value: Option<String>,
    pub state_change_seq: Option<f64>,
}

/// True when the live agent on the recorded pane still belongs to the
/// recorded transcript's generation.
pub fn binding_is_stale(
    record: &SessionRecord,
    live: Option<&LiveBinding>,
) -> (bool, Option<String>) {
    let identity = match &record.identity {
        Some(v) if v.is_object() => v,
        _ => return (false, None),
    };
    let pane_id = identity.get("pane_id").and_then(|v| v.as_str());
    let session_value = identity
        .get("agent_session")
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_str());
    let (Some(_pane), Some(session_value)) = (pane_id, session_value) else {
        return (false, None);
    };
    let Some(live) = live else {
        return (false, None);
    };
    if live.agent_session_value.as_deref() != Some(session_value) {
        return (true, Some("agent_session_changed".to_string()));
    }
    (false, None)
}

/// Serialize an f64 with JavaScript `JSON.stringify` number semantics:
/// integral finite values emit without a fraction (`1`, not `1.0`) so
/// the on-disk event log stays byte-identical to the TS writer.
fn serialize_f64_js_number<S: serde::Serializer>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) if v.is_finite() && v.fract() == 0.0 && v.abs() <= 9.007_199_254_740_992e15 => {
            serializer.serialize_i64(*v as i64)
        }
        Some(v) => serializer.serialize_f64(*v),
        None => serializer.serialize_none(),
    }
}
