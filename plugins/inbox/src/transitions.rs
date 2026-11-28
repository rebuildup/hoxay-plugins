//! Organize-transition log for the inbox plugin (#182, ADR-0029).
//!
//! Rust port of `plugins/inbox/src/transitions.ts`. Every real organize
//! mutation appends one `InboxTransition` inside the same `update_state`
//! critical section that persists the mark.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hoxay_herdr_client::snapshot::{AgentInfo, PaneInfo};

use crate::model::{agent_session_ref, iso_millis};
use crate::state::{AgentSessionRef, ClearedCause, InboxState, InboxTransition, TransitionLog};

/// Per-session retention cap, matching the entry-cap discipline.
pub const MAX_TRANSITIONS_PER_SESSION: usize = 500;

/// Snapshot-derived fields shared by every transition kind.
#[derive(Debug, Clone, Default)]
pub struct TransitionContext {
    pub pane: Option<PaneInfo>,
    pub agent: Option<AgentInfo>,
    /// Fallback workspace id (e.g. from a surviving entry) when the pane
    /// is gone.
    pub workspace_id: Option<String>,
    pub at: String,
}

/// Fields captured alongside a transition: workspace, cwd, agent_status,
/// state_change_seq, agent_session fingerprint.
type ContextFields = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<AgentSessionRef>,
);

fn context_fields(pane_id: &str, ctx: &TransitionContext) -> ContextFields {
    let agent = ctx.agent.as_ref();
    let workspace_id = ctx
        .pane
        .as_ref()
        .and_then(|p| p.workspace_id.clone())
        .or_else(|| ctx.workspace_id.clone());
    let cwd = agent
        .and_then(|a| a.cwd.clone().or_else(|| a.foreground_cwd.clone()))
        .or_else(|| ctx.pane.as_ref().and_then(|p| p.cwd.clone()));
    let agent_status = agent.and_then(|a| a.agent_status.clone());
    let state_change_seq = agent.and_then(|a| a.state_change_seq).map(|v| v as i64);
    let agent_session = agent_session_ref(agent);
    (
        Some(pane_id.to_string()),
        workspace_id,
        cwd,
        agent_status,
        state_change_seq,
        agent_session,
    )
}

/// Append one transition to the per-session log inside `update_state`.
/// `seq` is assigned here (per-session monotonic); overflow drops the
/// oldest items and counts them in `dropped`.
pub fn append_transition(
    state: &mut InboxState,
    session_key: &str,
    kind: &str,
    pane_id: &str,
    ctx: &TransitionContext,
    cause: Option<&str>,
) -> InboxTransition {
    if state.transitions.is_none() {
        state.transitions = Some(BTreeMap::new());
    }
    let logs = state.transitions.as_mut().expect("transitions map");
    let log = logs
        .entry(session_key.to_string())
        .or_insert_with(|| TransitionLog {
            seq: 0,
            dropped: 0,
            items: Vec::new(),
        });
    let seq = log.seq + 1;
    log.seq = seq;
    let (pane_id_f, workspace_id, cwd, agent_status, state_change_seq, agent_session) =
        context_fields(pane_id, ctx);
    let item = InboxTransition {
        seq,
        at: ctx.at.clone(),
        kind: kind.to_string(),
        pane_id: pane_id_f.unwrap_or_default(),
        workspace_id,
        cwd,
        agent_status,
        state_change_seq,
        agent_session,
        cause: cause.map(|c| c.to_string()),
    };
    log.items.push(item.clone());
    let overflow = log.items.len() as i64 - MAX_TRANSITIONS_PER_SESSION as i64;
    if overflow > 0 {
        log.items.drain(0..overflow as usize);
        log.dropped += overflow;
    }
    item
}

/// User-driven organize mutation (settle/snooze/restore commands).
pub fn append_organize_transition(
    state: &mut InboxState,
    session_key: &str,
    kind: &str,
    pane_id: &str,
    ctx: &TransitionContext,
) -> InboxTransition {
    append_transition(state, session_key, kind, pane_id, ctx, None)
}

/// Record `cleared` transitions for marks removed by expiry/generation
/// resolution. Collapses to a single record using the first (primary)
/// cause — one cleared record per removed mark would spam the log.
pub fn append_cleared_transitions(
    state: &mut InboxState,
    session_key: &str,
    pane_id: &str,
    marks: &[crate::model::ClearedMarkInfo],
    ctx: &TransitionContext,
) {
    let Some(first) = marks.first() else {
        return;
    };
    append_transition(
        state,
        session_key,
        "cleared",
        pane_id,
        ctx,
        Some(first.cause.as_str()),
    );
}

/// Build a TransitionContext from parts (pane/agent/workspace fallback).
pub fn transition_context(
    pane: Option<PaneInfo>,
    agent: Option<AgentInfo>,
    workspace_id: Option<String>,
    at: DateTime<Utc>,
) -> TransitionContext {
    TransitionContext {
        pane,
        agent,
        workspace_id,
        at: iso_millis(at),
    }
}

// Keep ClearedCause referenced for callers building causes.
#[allow(dead_code)]
fn _cause_marker(_: ClearedCause) {}
