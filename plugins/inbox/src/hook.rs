//! Event-hook entrypoint. herdr invokes `inbox hook` with
//! HERDR_PLUGIN_EVENT and HERDR_PLUGIN_EVENT_JSON set. Rust port of
//! `plugins/inbox/src/hook.ts`.
//!
//! - `pane.agent_status_changed`: resolve the affected pane's organize
//!   marks against a fresh snapshot and refresh its metadata tokens.
//! - `pane.closed`: drop the pane's organize entry and every watch
//!   baseline (a reused pane id starts with clean state).
//!
//! Contract: exits 0 on success; failures are logged to stderr — they
//! must never affect herdr itself.

use std::collections::BTreeMap;

use chrono::Utc;
use hoxay_herdr_client::commands::{self, CallOptions};
use serde_json::Value;

use crate::cli::fail_target;
use crate::model::{self, resolve_entry};
use crate::state::{remove_watch_link, session_entries, update_state};
use crate::transitions::{append_cleared_transitions, append_transition, TransitionContext};

struct EventEnvelope {
    event: Option<String>,
    pane_id: Option<String>,
}

fn parse_envelope(raw: &str) -> Option<EventEnvelope> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let data = parsed.get("data");
    Some(EventEnvelope {
        event: parsed
            .get("event")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        pane_id: data
            .and_then(|d| d.get("pane_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

// HERDR_PLUGIN_EVENT carries the dotted hook name ("pane.closed"); the
// JSON envelope serializes EventKind in snake_case ("pane_closed").
fn event_name(envelope: &EventEnvelope) -> String {
    if let Ok(env) = std::env::var("HERDR_PLUGIN_EVENT") {
        if !env.is_empty() {
            return env;
        }
    }
    match envelope.event.as_deref() {
        Some("pane_closed") => "pane.closed".to_string(),
        Some("pane_agent_status_changed") => "pane.agent_status_changed".to_string(),
        other => other.unwrap_or("").to_string(),
    }
}

fn hook_call_options() -> CallOptions {
    // Hooks only run inside the herdr plugin runtime; mutation:true keeps
    // a manually-invoked hook from silently falling back to the implicit
    // default session.
    CallOptions {
        mutation: true,
        ..Default::default()
    }
}

fn session_key_or_exit() -> String {
    let opts = hoxay_herdr_client::target::ResolveTargetOptions {
        mutation: true,
        ..Default::default()
    };
    hoxay_herdr_client::target::session_key_for(&opts).unwrap_or_else(|e| fail_target(&e))
}

struct AgentStatusOutcome {
    organize_kind: Option<String>,
    organize_until: Option<String>,
    pinned: bool,
}

fn run_hook() -> Result<(), String> {
    let Some(raw) = std::env::var("HERDR_PLUGIN_EVENT_JSON").ok() else {
        return Ok(());
    };
    if raw.is_empty() {
        return Ok(());
    }
    let Some(envelope) = parse_envelope(&raw) else {
        return Ok(());
    };
    let Some(pane_id) = envelope.pane_id.clone() else {
        return Ok(());
    };
    let name = event_name(&envelope);
    let session_key = session_key_or_exit();

    if name == "pane.closed" {
        update_state(|state| {
            // Sequential `state` borrows: entry removal, watch-baseline
            // cleanup, then the transition append.
            let entry = state
                .sessions
                .get_mut(&session_key)
                .and_then(|m| m.remove(&pane_id));
            // Record the organize clear before dropping the entry — the
            // pane is gone, so only the stored workspace id survives.
            let had_hide_kind = entry
                .as_ref()
                .map(|e| e.settled.is_some() || e.snoozed.is_some() || e.archived.is_some())
                .unwrap_or(false);
            // Drop every watch baseline for the closed pane.
            let watch_refs: Vec<String> = state
                .watch
                .as_ref()
                .and_then(|w| w.get(&session_key))
                .and_then(|s| s.get(&pane_id))
                .map(|pw| pw.keys().cloned().collect())
                .unwrap_or_default();
            for link_ref in watch_refs {
                remove_watch_link(state, &session_key, &pane_id, &link_ref);
            }
            if had_hide_kind {
                let ctx = TransitionContext {
                    pane: None,
                    agent: None,
                    workspace_id: entry.as_ref().and_then(|e| e.workspace_id.clone()),
                    at: model::iso_millis(Utc::now()),
                };
                append_transition(
                    state,
                    &session_key,
                    "cleared",
                    &pane_id,
                    &ctx,
                    Some("pane_closed"),
                );
            }
        })?;
        return Ok(());
    }

    if name != "pane.agent_status_changed" {
        return Ok(());
    }

    let call = hook_call_options();
    let snapshot = commands::session_snapshot(&call).map_err(|e| e.to_string())?;
    let agent = snapshot.agents.get(&pane_id).cloned();

    let now = Utc::now();
    let pane = snapshot.pane(&pane_id).cloned();
    let outcome: Option<AgentStatusOutcome> = update_state(|state| {
        // Phase 1: mutate the entry inside the session map (take it out,
        // resolve, reinsert only when organize state survives).
        let (workspace_id, pinned, organize_kind, organize_until, hide_clears) = {
            let entries = session_entries(state, &session_key);
            let mut entry = entries.remove(&pane_id)?;
            let workspace_id = entry.workspace_id.clone();
            let result = resolve_entry(&mut entry, agent.as_ref(), now);
            entry.updated_at = model::iso_millis(now);
            let has_state = entry.settled.is_some()
                || entry.snoozed.is_some()
                || entry.archived.is_some()
                || entry.pinned.is_some()
                || entry.order.is_some()
                || entry.links.as_ref().map(|l| !l.is_empty()).unwrap_or(false);
            let pinned = entry.pinned.is_some();
            if has_state {
                entries.insert(pane_id.clone(), entry);
            }
            // Organize-transition log: hide-kind clears resolved by this
            // hook are real organize mutations; pin clears are layout hints.
            let hide_clears: Vec<model::ClearedMarkInfo> = result
                .cleared_marks
                .iter()
                .filter(|m| m.field != model::ClearedMarkField::Pinned)
                .map(|m| model::ClearedMarkInfo {
                    field: m.field,
                    cause: m.cause,
                })
                .collect();
            (
                workspace_id,
                pinned,
                result.organize.as_ref().map(|o| o.kind.to_string()),
                result.organize.as_ref().and_then(|o| o.until.clone()),
                hide_clears,
            )
        };
        // Phase 2: transition log append once `entries` is free.
        if !hide_clears.is_empty() {
            let ctx = TransitionContext {
                pane: pane.clone(),
                agent: agent.clone(),
                workspace_id,
                at: model::iso_millis(now),
            };
            append_cleared_transitions(state, &session_key, &pane_id, &hide_clears, &ctx);
        }
        Some(AgentStatusOutcome {
            organize_kind,
            organize_until,
            pinned,
        })
    })?;

    let Some(outcome) = outcome else {
        return Ok(());
    };

    // Refresh the pane's inbox metadata token to match resolved state.
    let mut tokens: BTreeMap<String, Option<String>> = BTreeMap::new();
    tokens.insert("inbox_state".to_string(), outcome.organize_kind);
    tokens.insert("inbox_snoozed_until".to_string(), outcome.organize_until);
    tokens.insert(
        "inbox_pinned".to_string(),
        if outcome.pinned {
            Some("true".to_string())
        } else {
            None
        },
    );
    if let Err(err) = commands::report_pane_metadata(&pane_id, &tokens, &hook_call_options()) {
        eprintln!(
            "inbox hook: metadata projection failed for {}: {}",
            pane_id, err
        );
    }
    Ok(())
}

/// `inbox hook` entrypoint; exits 0 on success, 1 on failure.
pub fn run(_args: &[String]) -> ! {
    match run_hook() {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("inbox hook failed: {}", err);
            std::process::exit(1);
        }
    }
}
