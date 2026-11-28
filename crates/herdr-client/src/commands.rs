//! High-level herdr socket operations used by plugins.
//! Every function opens one request connection; only `subscribe` stays
//! open. Mirrors `packages/herdr-client/src/commands.ts`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::snapshot::{AgentInfo, PaneInfo, SessionSnapshot};
use crate::socket::{request, subscribe, ClientError, Subscription};
use crate::target::{resolve_target, ResolveTargetOptions, TargetResolutionError};

/// Error surface for socket commands: target-resolution failures keep
/// their structured code/usage (callers map them to exit codes);
/// transport/RPC failures are plain client errors.
#[derive(Debug)]
pub enum CommandError {
    Target(TargetResolutionError),
    Client(ClientError),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Target(e) => write!(f, "{}", e),
            CommandError::Client(e) => write!(f, "{}", e),
        }
    }
}
impl std::error::Error for CommandError {}

impl From<ClientError> for CommandError {
    fn from(e: ClientError) -> Self {
        CommandError::Client(e)
    }
}
impl From<TargetResolutionError> for CommandError {
    fn from(e: TargetResolutionError) -> Self {
        CommandError::Target(e)
    }
}
impl From<std::io::Error> for CommandError {
    fn from(e: std::io::Error) -> Self {
        CommandError::Client(ClientError::Io(e.to_string()))
    }
}

/// Per-call options shared by all socket commands (CallOptions).
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
    /// Request timeout in ms (default 10_000).
    pub timeout_ms: Option<u64>,
    /// Named herdr session; explicit flag, beats ambient env.
    pub session: Option<String>,
    /// Explicit socket path; beats ambient env.
    pub socket_path: Option<String>,
    /// Mutating operation: target resolution fails closed with
    /// `target_required` on the implicit default session socket.
    pub mutation: bool,
}

impl CallOptions {
    pub fn timeout(timeout_ms: u64) -> Self {
        CallOptions {
            timeout_ms: Some(timeout_ms),
            ..Default::default()
        }
    }

    fn timeout_or_default(&self) -> u64 {
        self.timeout_ms.unwrap_or(10_000)
    }

    fn target_options(&self) -> ResolveTargetOptions {
        ResolveTargetOptions {
            session: self.session.clone(),
            socket_path: self.socket_path.clone(),
            mutation: self.mutation,
        }
    }

    fn socket_for(&self) -> Result<String, CommandError> {
        Ok(resolve_target(&self.target_options())?.socket_path)
    }
}

/// `session.snapshot` — fetch and normalize. The wire format carries
/// `agents` as an array keyed by `pane_id`; normalize to the map shape.
pub fn session_snapshot(options: &CallOptions) -> Result<SessionSnapshot, CommandError> {
    let socket = options.socket_for()?;
    let result = request(
        &socket,
        "session.snapshot",
        json!({}),
        options.timeout_or_default(),
    )?;

    // `{snapshot: {...}}` envelope or a bare snapshot object.
    let snapshot_value = result
        .get("snapshot")
        .cloned()
        .unwrap_or_else(|| result.clone());

    let mut agents: BTreeMap<String, AgentInfo> = BTreeMap::new();
    match snapshot_value.get("agents") {
        Some(Value::Array(arr)) => {
            for item in arr {
                if let Ok(agent) = serde_json::from_value::<AgentInfo>(item.clone()) {
                    if let Some(pane_id) = agent.pane_id.clone().or_else(|| {
                        item.get("pane_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    }) {
                        if !pane_id.is_empty() {
                            agents.insert(pane_id, agent);
                        }
                    }
                }
            }
        }
        Some(Value::Object(map)) => {
            for (key, val) in map {
                if let Ok(mut agent) = serde_json::from_value::<AgentInfo>(val.clone()) {
                    if agent.pane_id.is_none() {
                        agent.pane_id = Some(key.clone());
                    }
                    agents.insert(key.clone(), agent);
                }
            }
        }
        _ => {}
    }

    let panes = snapshot_value
        .get("panes")
        .and_then(|v| serde_json::from_value::<Vec<PaneInfo>>(v.clone()).ok())
        .unwrap_or_default();
    let tabs = snapshot_value
        .get("tabs")
        .and_then(|v| serde_json::from_value::<Vec<crate::snapshot::TabInfo>>(v.clone()).ok())
        .unwrap_or_default();
    let workspaces = snapshot_value
        .get("workspaces")
        .and_then(|v| serde_json::from_value::<Vec<crate::snapshot::WorkspaceInfo>>(v.clone()).ok())
        .unwrap_or_default();

    Ok(SessionSnapshot {
        session: snapshot_value.get("session").cloned(),
        workspaces,
        tabs,
        panes,
        agents,
        raw: snapshot_value,
    })
}

// Metadata `seq` orders reports from the same source; keep it monotonic
// even when two invocations land in the same millisecond.
static LAST_METADATA_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_metadata_seq() -> u64 {
    LAST_METADATA_SEQ
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(now_millis().max(cur.saturating_add(1)))
        })
        .unwrap_or_else(|_| now_millis())
}

/// `pane.report_metadata` — write plugin-owned metadata tokens onto a
/// pane. `source` identifies the reporting integration; tokens merge per
/// source and a `null` value clears a token.
pub fn report_pane_metadata(
    pane_id: &str,
    tokens: &BTreeMap<String, Option<String>>,
    options: &CallOptions,
) -> Result<Value, CommandError> {
    report_pane_metadata_as(pane_id, tokens, "rebuildup.inbox", None, options)
}

/// `pane.report_metadata` with explicit source/agent overrides.
pub fn report_pane_metadata_as(
    pane_id: &str,
    tokens: &BTreeMap<String, Option<String>>,
    source: &str,
    agent: Option<&str>,
    options: &CallOptions,
) -> Result<Value, CommandError> {
    let socket = options.socket_for()?;
    let mut params = json!({
        "pane_id": pane_id,
        "source": source,
        "tokens": tokens,
        "seq": next_metadata_seq(),
    });
    if let Some(agent) = agent {
        params["agent"] = json!(agent);
    }
    request(
        &socket,
        "pane.report_metadata",
        params,
        options.timeout_or_default(),
    )
    .map_err(CommandError::from)
}

/// Publish a plugin view into herdr's single Agent View slot.
pub fn set_agent_view(view: Value, options: &CallOptions) -> Result<Value, CommandError> {
    let socket = options.socket_for()?;
    request(
        &socket,
        "agent.view.set",
        view,
        options.timeout_or_default(),
    )
    .map_err(CommandError::from)
}

/// Clear the plugin's Agent View slot.
pub fn clear_agent_view(
    source: Option<&str>,
    options: &CallOptions,
) -> Result<Value, CommandError> {
    let socket = options.socket_for()?;
    let params = match source {
        Some(s) => json!({"source": s}),
        None => json!({}),
    };
    request(
        &socket,
        "agent.view.clear",
        params,
        options.timeout_or_default(),
    )
    .map_err(CommandError::from)
}

/// `notification.show`.
pub fn show_notification(
    message: &str,
    title: Option<&str>,
    options: &CallOptions,
) -> Result<Value, CommandError> {
    let socket = options.socket_for()?;
    request(
        &socket,
        "notification.show",
        json!({
            "title": title.unwrap_or("hoxay-plugins"),
            "message": message,
        }),
        options.timeout_or_default(),
    )
    .map_err(CommandError::from)
}

/// Subscribe to pane agent status changes
/// (`events.subscribe` with `pane.agent_status_changed`).
pub fn subscribe_agent_events(options: &CallOptions) -> Result<Subscription, CommandError> {
    subscribe_agent_events_with(options, vec![json!({"type": "pane.agent_status_changed"})])
}

/// Subscribe with an explicit subscription list
/// (`pane.output_matched` requires `source`/`match` params upstream).
pub fn subscribe_agent_events_with(
    options: &CallOptions,
    subscriptions: Vec<Value>,
) -> Result<Subscription, CommandError> {
    let socket = options.socket_for()?;
    let sub = subscribe(
        &socket,
        Value::Array(subscriptions),
        options.timeout_or_default(),
    )?;
    let _ = sub.started()?;
    Ok(sub)
}
