//! Persistent organize state for the inbox plugin.
//!
//! Rust port of `plugins/inbox/src/state.ts`. Stored as a single JSON
//! document under the plugin state dir (`HERDR_PLUGIN_STATE_DIR`,
//! falling back to the platform state dir for external clients). Writes
//! are serialized through an exclusive lock file and committed via
//! tmp+rename so concurrent CLI/hook invocations cannot corrupt or
//! interleave state.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use hoxay_herdr_client::dirs::plugin_dirs;
use hoxay_herdr_client::target::{session_key_for, ResolveTargetOptions};

pub const PLUGIN_ID: &str = "rebuildup.inbox";
pub const STATE_SCHEMA_VERSION: u64 = 2;

const LOCK_STALE_MS: u64 = 30_000;
const LOCK_TIMEOUT_MS: u64 = 10_000;
const LOCK_RETRY_MS: u64 = 25;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Schema (schema v2, namespaced per session key)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Generation-guarded organize mark fields shared by settled/snoozed/
/// archived/pinned. TS writes `agent_session` (possibly null) on every
/// mark, so it serializes unconditionally here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationMark {
    pub at: String,
    /// state_change_seq observed when the mark was applied (-1 = none).
    pub seq: i64,
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
}

pub type SettledMark = GenerationMark;
pub type ArchivedMark = GenerationMark;
pub type PinnedMark = GenerationMark;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnoozedMark {
    pub at: String,
    pub until: String,
    pub seq: i64,
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
}

pub type LinkKind = String; // "pr" | "issue"

pub const LINK_KIND_PR: &str = "pr";
pub const LINK_KIND_ISSUE: &str = "issue";

/// User-declared tracker reference ("this pane is working on repo#n").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneLink {
    pub kind: LinkKind,
    /// Forge repo path as declared, e.g. "owner/name".
    pub repo: String,
    pub number: i64,
    /// Canonical display ref "<repo>#<number>".
    #[serde(rename = "ref")]
    pub ref_: String,
    /// ISO timestamp the link was (re)declared.
    pub at: String,
    /// state_change_seq observed at link time.
    pub seq: i64,
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
}

impl PaneLink {
    /// JSON field name is `ref` (a Rust keyword workaround via rename).
    pub fn json_ref(&self) -> &str {
        &self.ref_
    }
}

/// Point-in-time PR observation recorded by `watch-pr` (Issue #178).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkWatchObservation {
    /// Raw gh lifecycle state uppercased: OPEN | MERGED | CLOSED | …
    pub state: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "mergeStateStatus")]
    pub merge_state_status: Option<String>,
    pub mergeable: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
    pub author: Option<String>,
    #[serde(rename = "failingChecks")]
    pub failing_checks: Vec<String>,
    #[serde(rename = "reviewKeys")]
    pub review_keys: Vec<String>,
    #[serde(rename = "commentKeys")]
    pub comment_keys: Vec<String>,
}

pub type WatchTerminal = String; // "merged" | "closed"

/// Watcher bookkeeping for one declared PR link.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkWatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<LinkWatchObservation>,
    pub first_seen_at: String,
    /// ISO time of the last *successful* poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// Last poll error message; cleared on the next success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub consecutive_failures: i64,
    /// Set once the PR reaches MERGED/CLOSED.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<WatchTerminal>,
    /// Epoch ms before which this link is skipped (failure backoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_poll_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxEntry {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled: Option<SettledMark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed: Option<SnoozedMark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<ArchivedMark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<PinnedMark>,
    /// Manual order rank (lower sorts first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// User-declared PR/Issue links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<PaneLink>>,
    pub updated_at: String,
}

pub type TransitionKind = String; // "settled" | "snoozed" | "restored" | "cleared"
pub type ClearedCause = String; // "expired" | "generation" | "session_mismatch" | "pane_closed" | "pruned"

/// One recorded organize transition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InboxTransition {
    pub seq: i64,
    pub at: String,
    pub kind: TransitionKind,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_change_seq: Option<i64>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
    /// `cleared` transitions only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<ClearedCause>,
}

/// Bounded per-session transition log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TransitionLog {
    pub seq: i64,
    #[serde(default)]
    pub dropped: i64,
    pub items: Vec<InboxTransition>,
}

/// State is namespaced per herdr session AND per host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxState {
    pub version: u64,
    /// sessionKey → pane_id → InboxEntry
    #[serde(default)]
    pub sessions: BTreeMap<String, BTreeMap<String, InboxEntry>>,
    /// `watch-pr` baselines: sessionKey → pane_id → link ref → LinkWatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchBaselines>,
    /// Organize-transition log per session key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transitions: Option<BTreeMap<String, TransitionLog>>,
}

/// watch-pr baselines: sessionKey → pane_id → link ref → LinkWatch.
pub type WatchBaselines = BTreeMap<String, BTreeMap<String, BTreeMap<String, LinkWatch>>>;

/// Per-session watch map: pane_id → link ref → LinkWatch.
pub type SessionWatchMap = BTreeMap<String, BTreeMap<String, LinkWatch>>;

pub fn empty_state() -> InboxState {
    InboxState {
        version: STATE_SCHEMA_VERSION,
        sessions: BTreeMap::new(),
        watch: None,
        transitions: None,
    }
}

/// Inner per-session entry map, created on demand for writes.
pub fn session_entries<'a>(
    state: &'a mut InboxState,
    session_key: &str,
) -> &'a mut BTreeMap<String, InboxEntry> {
    state.sessions.entry(session_key.to_string()).or_default()
}

/// Watch baselines for one session, created on demand for writes.
pub fn session_watch<'a>(state: &'a mut InboxState, session_key: &str) -> &'a mut SessionWatchMap {
    let watch = state.watch.get_or_insert_with(BTreeMap::new);
    watch.entry(session_key.to_string()).or_default()
}

/// Read-only watch baselines for one session.
pub fn session_watch_read<'a>(
    state: &'a InboxState,
    session_key: &str,
) -> Option<&'a SessionWatchMap> {
    state.watch.as_ref().and_then(|w| w.get(session_key))
}

/// Drop one link's watch baseline (unlink / pane.closed paths).
pub fn remove_watch_link(state: &mut InboxState, session_key: &str, pane_id: &str, link_ref: &str) {
    let Some(watch) = state.watch.as_mut() else {
        return;
    };
    let Some(session) = watch.get_mut(session_key) else {
        return;
    };
    let Some(pane) = session.get_mut(pane_id) else {
        return;
    };
    pane.remove(link_ref);
    if pane.is_empty() {
        session.remove(pane_id);
    }
    if session.is_empty() {
        watch.remove(session_key);
    }
}

// ---------------------------------------------------------------------------
// Filesystem: state file, lock, atomic writes
// ---------------------------------------------------------------------------

pub fn state_file_path() -> PathBuf {
    plugin_dirs(PLUGIN_ID, None, None)
        .state_dir
        .join("state.json")
}

fn lock_path() -> PathBuf {
    let mut p = state_file_path().into_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

pub fn load_state() -> Result<InboxState, String> {
    let path = state_file_path();
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(empty_state()),
        Err(e) => return Err(format!("cannot read state file {}: {}", path.display(), e)),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return Err(format!("cannot parse state file {}: {}", path.display(), e)),
    };
    let version = parsed.get("version").and_then(|v| v.as_u64());
    let sessions_ok = parsed
        .get("sessions")
        .map(|v| v.is_object())
        .unwrap_or(false);
    if version != Some(STATE_SCHEMA_VERSION) || !sessions_ok {
        return Ok(empty_state());
    }

    // The transition log is append-only audit data: keep entries that
    // look well-formed, drop malformed ones individually.
    let mut transitions: BTreeMap<String, TransitionLog> = BTreeMap::new();
    if let Some(Value::Object(map)) = parsed.get("transitions") {
        for (key, log_value) in map {
            let seq_ok = log_value.get("seq").map(|v| v.is_number()).unwrap_or(false);
            let items_ok = log_value
                .get("items")
                .map(|v| v.is_array())
                .unwrap_or(false);
            if !seq_ok || !items_ok {
                continue;
            }
            // Per-item filter mirroring the TS loadState: keep only
            // entries with numeric seq, string kind, string pane_id.
            let mut items: Vec<InboxTransition> = Vec::new();
            if let Some(Value::Array(raw_items)) = log_value.get("items") {
                for item_value in raw_items {
                    let well_formed = item_value
                        .get("seq")
                        .map(|v| v.is_number())
                        .unwrap_or(false)
                        && item_value
                            .get("kind")
                            .map(|v| v.is_string())
                            .unwrap_or(false)
                        && item_value
                            .get("pane_id")
                            .map(|v| v.is_string())
                            .unwrap_or(false);
                    if !well_formed {
                        continue;
                    }
                    if let Ok(t) = serde_json::from_value::<InboxTransition>(item_value.clone()) {
                        items.push(t);
                    }
                }
            }
            let seq = log_value.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
            let dropped = log_value
                .get("dropped")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            transitions.insert(
                key.clone(),
                TransitionLog {
                    seq,
                    dropped,
                    items,
                },
            );
        }
    }

    let mut state: InboxState = serde_json::from_value(parsed)
        .map_err(|e| format!("cannot decode state file {}: {}", path.display(), e))?;
    state.version = STATE_SCHEMA_VERSION;
    if !transitions.is_empty() {
        state.transitions = Some(transitions);
    } else {
        state.transitions = None;
    }
    Ok(state)
}

fn acquire_lock() -> Result<Box<dyn FnOnce() + Send>, String> {
    let file = lock_path();
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create state dir {}: {}", parent.display(), e))?;
    }
    let deadline = now_millis() + LOCK_TIMEOUT_MS;
    loop {
        // O_EXCL create — mirrors `open(file, "wx")`.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(mut handle) => {
                let body = serde_json::json!({
                    "pid": std::process::id(),
                    "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                });
                let _ = handle.write_all(body.to_string().as_bytes());
                let _ = handle.sync_all();
                drop(handle);
                return Ok(Box::new(move || {
                    let _ = fs::remove_file(&file);
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Stale lock (>30s) is removed and retried.
                match fs::metadata(&file) {
                    Ok(meta) => {
                        let age = meta
                            .modified()
                            .ok()
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .map(|d| now_millis().saturating_sub(d.as_millis() as u64))
                            .unwrap_or(0);
                        if age > LOCK_STALE_MS {
                            let _ = fs::remove_file(&file);
                            continue;
                        }
                    }
                    Err(_) => continue,
                }
                if now_millis() > deadline {
                    return Err(
                        "inbox state lock timed out; another command may be running".to_string()
                    );
                }
                std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(e) => return Err(format!("cannot acquire state lock: {}", e)),
        }
    }
}

/// Read-modify-write under the exclusive lock.
pub fn update_state<T, F>(mutate: F) -> Result<T, String>
where
    F: FnOnce(&mut InboxState) -> T,
{
    let release = acquire_lock()?;
    let result = (|| {
        let mut state = load_state()?;
        let out = mutate(&mut state);
        let path = state_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create state dir {}: {}", parent.display(), e))?;
        }
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(".tmp-{}", std::process::id()));
        let tmp_path = PathBuf::from(tmp);
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&state).unwrap_or_default()
        );
        fs::write(&tmp_path, body).map_err(|e| format!("cannot write state tmp file: {}", e))?;
        fs::rename(&tmp_path, &path).map_err(|e| format!("cannot commit state file: {}", e))?;
        Ok(out)
    })();
    release();
    result
}

/// Convenience: resolve target options → session key for this invocation.
pub fn session_key_for_options(options: &ResolveTargetOptions) -> Result<String, String> {
    session_key_for(options).map_err(|e| format!("{}: {}", e.code.as_str(), e.message))
}
