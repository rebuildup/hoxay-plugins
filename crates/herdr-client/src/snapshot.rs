//! Typed subset of herdr's `session.snapshot` and `agent.list` responses.
//! Mirrors `packages/herdr-client/src/snapshot.ts` — permissive: unknown
//! fields pass through via `extra`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    /// Reporting source of the session identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Per-agent entry in a snapshot. `pane_id` is not part of the TS
/// `AgentInfo` (it lives in the agents map key), but the wire array form
/// carries it — keep it optional-tolerant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Agent display name (e.g. "claude"), reported by the agent source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    /// Monotonic per-pane counter; bumps on every agent status change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_change_seq: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_seq: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    /// Present when agents arrive as an array on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneInfo {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TabInfo {
    #[serde(default)]
    pub tab_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl crate::target::AmbientPane for PaneInfo {
    fn pane_id(&self) -> &str {
        &self.pane_id
    }
    fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }
    fn tab_id(&self) -> Option<&str> {
        self.tab_id.as_deref()
    }
}

/// Normalized `session.snapshot` result. `agents` is keyed by pane_id —
/// the wire array form is folded into the map by `session_snapshot`.
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub session: Option<Value>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub tabs: Vec<TabInfo>,
    pub panes: Vec<PaneInfo>,
    /// Per-pane agent entry keyed by pane_id, as returned in snapshots.
    pub agents: BTreeMap<String, AgentInfo>,
    pub raw: Value,
}

impl SessionSnapshot {
    /// Tolerant pane lookup by id.
    pub fn pane(&self, pane_id: &str) -> Option<&PaneInfo> {
        self.panes.iter().find(|p| p.pane_id == pane_id)
    }

    /// Tolerant agent lookup by pane id.
    pub fn agent(&self, pane_id: &str) -> Option<&AgentInfo> {
        self.agents.get(pane_id)
    }
}
