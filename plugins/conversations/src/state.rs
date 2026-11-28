//! Persistent index state for rebuildup.conversations.
//!
//! Rust port of `plugins/conversations/src/state.ts` (retired; git
//! history preserves it). `state.json` holds the per-transcript registry
//! (identity binding, scan cursors, counters) — schema unchanged so
//! state written by the TS implementation is honored verbatim.
//! Normalized events are appended to `events/<key>.jsonl` — append-only,
//! one JSON object per line. Writes are serialized through an exclusive
//! lock file and committed via tmp+rename; event appends happen inside
//! the same locked section (AGENTS §4).
//!
//! `state.json` + `events/*.jsonl` are the canonical on-disk contract;
//! reads page the JSONL logs directly (no derived index — the #293
//! `index.sqlite` experiment was dropped in favor of byte-offset
//! cursors, see `read.rs`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hoxay_herdr_client::dirs::plugin_dirs;
use serde_json::Value;

use crate::model::{empty_state, ConversationEvent, ConversationsState, SessionRecord, PLUGIN_ID};

pub const STATE_SCHEMA_VERSION: u32 = 1;

const LOCK_STALE_MS: u64 = 30_000;
const LOCK_TIMEOUT_MS: u64 = 60_000;
const LOCK_RETRY_MS: u64 = 25;

fn state_file_path() -> PathBuf {
    plugin_dirs(PLUGIN_ID, None, None)
        .state_dir
        .join("state.json")
}

fn lock_path() -> PathBuf {
    PathBuf::from(format!("{}.lock", state_file_path().display()))
}

/// Filesystem-safe event-log name for a session record key.
/// JS `replace(/[^A-Za-z0-9._-]/g, "_")` operates on UTF-16 code
/// units — mirror that (non-BMP chars yield one `_` per surrogate).
pub fn events_file_name(key: &str) -> String {
    let mut safe = String::new();
    for u in key.encode_utf16() {
        let c = char::from_u32(u as u32).unwrap_or('_');
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            safe.push(c);
        } else {
            safe.push('_');
        }
    }
    format!("{}.jsonl", safe)
}

pub fn events_file_path(key: &str) -> PathBuf {
    plugin_dirs(PLUGIN_ID, None, None)
        .state_dir
        .join("events")
        .join(events_file_name(key))
}

pub fn load_state() -> Result<ConversationsState, String> {
    let file = state_file_path();
    let raw = match fs::read_to_string(&file) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(empty_state()),
        Err(e) => return Err(e.to_string()),
    };
    let parsed: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let version = parsed.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    let sessions = parsed.get("sessions").cloned().unwrap_or(Value::Null);
    if version != STATE_SCHEMA_VERSION as u64 || !sessions.is_object() {
        return Ok(empty_state());
    }
    let mut state = empty_state();
    // Per-record deserialize: TS passes `sessions` through verbatim, so
    // one malformed record must not nuke the whole registry. Records
    // that fail to parse are dropped (they could not be rewritten
    // faithfully anyway) while valid siblings survive.
    if let Value::Object(map) = sessions {
        for (k, v) in map {
            if let Ok(record) = serde_json::from_value::<SessionRecord>(v) {
                state.sessions.insert(k, record);
            }
        }
    }
    Ok(state)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn file_mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Acquire the exclusive state lock (mirrors the TS `acquireLock`).
fn acquire_lock() -> Result<Box<dyn FnOnce() + Send>, String> {
    let file = lock_path();
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + Duration::from_millis(LOCK_TIMEOUT_MS);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&file) {
            Ok(mut handle) => {
                let body = serde_json::json!({
                    "pid": std::process::id(),
                    "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                });
                let _ = handle.write_all(body.to_string().as_bytes());
                drop(handle);
                return Ok(Box::new(move || {
                    let _ = fs::remove_file(&file);
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = file_mtime_ms(&file);
                let age = now_millis().saturating_sub(stale);
                if stale > 0 && age > LOCK_STALE_MS {
                    let _ = fs::remove_file(&file);
                    continue;
                }
                if Instant::now() > deadline {
                    return Err(
                        "conversations state lock timed out; another command may be running"
                            .to_string(),
                    );
                }
                std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Read-modify-write under the exclusive lock; I/O inside is allowed.
pub fn update_state<T, F: FnOnce(&mut ConversationsState) -> Result<T, String>>(
    mutate: F,
) -> Result<T, String> {
    let release = acquire_lock()?;
    let result = (|| {
        let mut state = load_state()?;
        let out = mutate(&mut state)?;
        let file = state_file_path();
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
        let body = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        fs::write(&tmp, format!("{}\n", body)).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &file).map_err(|e| e.to_string())?;
        Ok(out)
    })();
    release();
    result
}

// --- Event log ---------------------------------------------------------------

/// Replace the event log (used when a transcript is re-indexed from 0).
pub fn write_events_file(key: &str, events: &[ConversationEvent]) -> Result<(), String> {
    let file = events_file_path(key);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut body = String::new();
    for e in events {
        body.push_str(&e.to_json_line().map_err(|e| e.to_string())?);
        body.push('\n');
    }
    let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
    fs::write(&tmp, body).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &file).map_err(|e| e.to_string())
}

/// Append events to the log (incremental scans).
pub fn append_events(key: &str, events: &[ConversationEvent]) -> Result<(), String> {
    if events.is_empty() {
        return Ok(());
    }
    let file = events_file_path(key);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| e.to_string())?;
    for e in events {
        f.write_all(e.to_json_line().map_err(|e| e.to_string())?.as_bytes())
            .map_err(|e| e.to_string())?;
        f.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn path_exists(p: &Path) -> bool {
    fs::metadata(p).is_ok()
}

pub fn new_session_record(
    provider: &str,
    session_id: &str,
    transcript_path: &str,
    cwd: Option<&str>,
) -> SessionRecord {
    SessionRecord {
        key: format!("{}:{}", provider, session_id),
        provider: provider.to_string(),
        session_id: session_id.to_string(),
        transcript_path: transcript_path.to_string(),
        cwd: cwd.map(|s| s.to_string()),
        status: crate::model::SessionStatus::Ok,
        error: None,
        identity: None,
        cursor: None,
        guard_hash: None,
        turn_count: 0,
        event_count: 0,
        first_event_at: None,
        last_event_at: None,
        last_indexed_at: None,
        extra: serde_json::Map::new(),
    }
}
