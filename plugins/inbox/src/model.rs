//! Pure organize-state logic for the inbox plugin. No I/O here so the
//! state machine is unit-testable without a live herdr session.
//!
//! Rust port of `plugins/inbox/src/model.ts`. Semantics inherited from
//! T3 Code settle/snooze (docs/specs/inbox.md):
//! - settle/snooze are user-side organize state keyed by pane identity
//!   (pane_id + agent_session fingerprint + state_change_seq generation).
//! - Any advance of state_change_seq, an agent-session mismatch, or
//!   snooze expiry restores the pane to the inbox deterministically at
//!   read time.
//! - Attention signals are never hidden: a pane whose agent reports a
//!   status that requires user attention stays in the inbox regardless
//!   of marks.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use hoxay_herdr_client::snapshot::{AgentInfo, PaneInfo};

use serde::Serialize;

use crate::state::{AgentSessionRef, GenerationMark, InboxEntry, LinkKind, PaneLink, SnoozedMark};

pub type OrganizeKind = &'static str; // "settled" | "snoozed" | "archived"

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrganizeView {
    pub kind: OrganizeKind,
    /// ISO timestamp the organize state was applied.
    pub since: String,
    /// Snooze deadline (snoozed only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// Public view of a stored PaneLink (fingerprint/seq internals omitted).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InboxLink {
    pub kind: LinkKind,
    pub repo: String,
    pub number: i64,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub at: String,
}

pub fn to_public_link(link: &PaneLink) -> InboxLink {
    InboxLink {
        kind: link.kind.clone(),
        repo: link.repo.clone(),
        number: link.number,
        ref_: link.ref_.clone(),
        at: link.at.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InboxItem {
    pub pane_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_change_seq: Option<i64>,
    pub agent_session: Option<AgentSessionRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organize: Option<OrganizeView>,
    /// True while the pane carries a live pinned mark.
    pub pinned: bool,
    /// Manual order rank; lower sorts first. None = unordered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    /// True when the pane currently requires user attention.
    pub attention: bool,
    /// False when organize state hides the pane from the default inbox view.
    pub in_inbox: bool,
    /// User-declared tracker links (possibly empty).
    pub links: Vec<InboxLink>,
}

/// Statuses that must never be hidden by organize state.
pub const ATTENTION_STATUSES: &[&str] = &["blocked"];

pub fn agent_session_ref(agent: Option<&AgentInfo>) -> Option<AgentSessionRef> {
    let session = agent?.agent_session.as_ref()?;
    // TS copies any string-typed field verbatim (empty strings included).
    let ref_ = AgentSessionRef {
        agent: session.agent.clone(),
        kind: session.kind.clone(),
        value: session.value.clone(),
    };
    if ref_.agent.is_some() || ref_.kind.is_some() || ref_.value.is_some() {
        Some(ref_)
    } else {
        None
    }
}

fn same_agent_session(
    recorded: Option<&AgentSessionRef>,
    current: Option<&AgentSessionRef>,
) -> bool {
    // A mark recorded without a fingerprint matches only while the pane
    // still has none — an agent session appearing later is a generation
    // change.
    let Some(recorded) = recorded else {
        return current.is_none();
    };
    // A recorded fingerprint that temporarily disappears is not treated
    // as a mismatch; state_change_seq still guards real generation
    // advances.
    let Some(current) = current else {
        return true;
    };
    recorded.agent == current.agent && recorded.value == current.value
}

fn current_seq(agent: Option<&AgentInfo>) -> i64 {
    agent
        .and_then(|a| a.state_change_seq)
        .map(|v| v as i64)
        .unwrap_or(-1)
}

/// Why a mark was auto-cleared, when it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionClearCause {
    Expired,
    Generation,
    SessionMismatch,
}

impl ResolutionClearCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolutionClearCause::Expired => "expired",
            ResolutionClearCause::Generation => "generation",
            ResolutionClearCause::SessionMismatch => "session_mismatch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearedMarkField {
    Settled,
    Snoozed,
    Archived,
    Pinned,
}

impl ClearedMarkField {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClearedMarkField::Settled => "settled",
            ClearedMarkField::Snoozed => "snoozed",
            ClearedMarkField::Archived => "archived",
            ClearedMarkField::Pinned => "pinned",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClearedMarkInfo {
    pub field: ClearedMarkField,
    pub cause: ResolutionClearCause,
}

/// Shared mark-clear check: `until` is Some only for snoozed marks.
fn mark_clear_cause(
    at: &str,
    until: Option<&str>,
    seq: i64,
    agent_session: Option<&AgentSessionRef>,
    agent: Option<&AgentInfo>,
    now: DateTime<Utc>,
) -> Option<ResolutionClearCause> {
    let _ = at;
    if let Some(until_str) = until {
        if let Some(until_dt) = parse_instant(until_str) {
            if until_dt <= now {
                return Some(ResolutionClearCause::Expired);
            }
        }
    }
    if !same_agent_session(agent_session, agent_session_ref(agent).as_ref()) {
        return Some(ResolutionClearCause::SessionMismatch);
    }
    let seq_now = current_seq(agent);
    // seq -1 means "no agent generation observed". A mark recorded at -1
    // is advanced by any later real seq.
    if seq_now >= 0 && (seq < 0 || seq_now > seq) {
        return Some(ResolutionClearCause::Generation);
    }
    None
}

/// Link staleness: dropped when the agent-session fingerprint no longer
/// matches or the recorded seq regresses. Forward seq advances never
/// expire a link.
fn link_stale(link: &PaneLink, agent: Option<&AgentInfo>) -> bool {
    if !same_agent_session(
        link.agent_session.as_ref(),
        agent_session_ref(agent).as_ref(),
    ) {
        return true;
    }
    let seq = current_seq(agent);
    seq >= 0 && link.seq >= 0 && seq < link.seq
}

/// True while the entry carries any organize state worth retaining.
pub fn entry_has_state(entry: &InboxEntry) -> bool {
    entry.settled.is_some()
        || entry.snoozed.is_some()
        || entry.archived.is_some()
        || entry.pinned.is_some()
        || entry.order.is_some()
        || entry.links.as_ref().map(|l| !l.is_empty()).unwrap_or(false)
}

#[derive(Debug, Default)]
pub struct ResolveEntryResult {
    pub organize: Option<OrganizeView>,
    pub cleared: bool,
    pub cleared_marks: Vec<ClearedMarkInfo>,
}

/// Resolve one entry against the live agent snapshot. Returns the
/// surviving organize view (snooze > settle > archive precedence) and
/// whether any mark was cleared. Mutates `entry` in place.
pub fn resolve_entry(
    entry: &mut InboxEntry,
    agent: Option<&AgentInfo>,
    now: DateTime<Utc>,
) -> ResolveEntryResult {
    let mut result = ResolveEntryResult::default();

    macro_rules! clear_mark {
        ($field:expr, $mark:expr, $until:expr) => {
            if let Some(mark) = &$mark {
                if let Some(cause) = mark_clear_cause(
                    &mark.at,
                    $until,
                    mark.seq,
                    mark.agent_session.as_ref(),
                    agent,
                    now,
                ) {
                    result.cleared = true;
                    result.cleared_marks.push(ClearedMarkInfo {
                        field: $field,
                        cause,
                    });
                    match $field {
                        ClearedMarkField::Snoozed => entry.snoozed = None,
                        ClearedMarkField::Settled => entry.settled = None,
                        ClearedMarkField::Archived => entry.archived = None,
                        ClearedMarkField::Pinned => entry.pinned = None,
                    }
                }
            }
        };
    }

    // Order matches TS: snoozed, settled, archived, pinned.
    let snoozed_until = entry.snoozed.as_ref().map(|m| m.until.clone());
    clear_mark!(
        ClearedMarkField::Snoozed,
        entry.snoozed,
        snoozed_until.as_deref()
    );
    clear_mark!(ClearedMarkField::Settled, entry.settled, None);
    clear_mark!(ClearedMarkField::Archived, entry.archived, None);
    clear_mark!(ClearedMarkField::Pinned, entry.pinned, None);

    if entry.links.is_some() {
        let links = entry.links.take().unwrap_or_default();
        let old_len = links.len();
        let kept: Vec<PaneLink> = links
            .into_iter()
            .filter(|link| !link_stale(link, agent))
            .collect();
        if kept.len() != old_len {
            result.cleared = true;
            entry.links = if kept.is_empty() { None } else { Some(kept) };
        } else {
            entry.links = Some(kept);
        }
    }

    if let Some(snoozed) = &entry.snoozed {
        result.organize = Some(OrganizeView {
            kind: "snoozed",
            since: snoozed.at.clone(),
            until: Some(snoozed.until.clone()),
        });
        return result;
    }
    if let Some(settled) = &entry.settled {
        result.organize = Some(OrganizeView {
            kind: "settled",
            since: settled.at.clone(),
            until: None,
        });
        return result;
    }
    if let Some(archived) = &entry.archived {
        result.organize = Some(OrganizeView {
            kind: "archived",
            since: archived.at.clone(),
            until: None,
        });
        return result;
    }
    result
}

#[derive(Debug)]
pub struct ResolvedClear {
    pub pane_id: String,
    pub field: ClearedMarkField,
    pub cause: ResolutionClearCause,
}

/// Resolve every entry in the state document.
pub fn resolve_state(
    entries: &mut BTreeMap<String, InboxEntry>,
    agents: &BTreeMap<String, AgentInfo>,
    now: DateTime<Utc>,
) -> (Vec<String>, Vec<ResolvedClear>) {
    let mut cleared_pane_ids: Vec<String> = Vec::new();
    let mut cleared_marks: Vec<ResolvedClear> = Vec::new();
    let pane_ids: Vec<String> = entries.keys().cloned().collect();
    for pane_id in pane_ids {
        let agent = agents.get(&pane_id);
        let entry = entries.get_mut(&pane_id).expect("entry exists");
        let result = resolve_entry(entry, agent, now);
        if result.cleared {
            cleared_pane_ids.push(pane_id.clone());
        }
        for mark in result.cleared_marks {
            cleared_marks.push(ResolvedClear {
                pane_id: pane_id.clone(),
                field: mark.field,
                cause: mark.cause,
            });
        }
        let has_state = entry_has_state(entry);
        if !has_state {
            entries.remove(&pane_id);
        }
    }
    (cleared_pane_ids, cleared_marks)
}

pub fn upsert_entry<'a>(
    entries: &'a mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    now_iso: &str,
) -> &'a mut InboxEntry {
    let entry = entries
        .entry(pane.pane_id.clone())
        .or_insert_with(|| InboxEntry {
            pane_id: pane.pane_id.clone(),
            workspace_id: None,
            settled: None,
            snoozed: None,
            archived: None,
            pinned: None,
            order: None,
            links: None,
            updated_at: now_iso.to_string(),
        });
    if pane.workspace_id.is_some() {
        entry.workspace_id = pane.workspace_id.clone();
    }
    entry.updated_at = now_iso.to_string();
    entry
}

fn seq_mark(agent: Option<&AgentInfo>) -> (i64, Option<AgentSessionRef>) {
    (current_seq(agent), agent_session_ref(agent))
}

fn generation_mark(agent: Option<&AgentInfo>, now: DateTime<Utc>) -> GenerationMark {
    let (seq, agent_session) = seq_mark(agent);
    GenerationMark {
        at: iso_millis(now),
        seq,
        agent_session,
    }
}

pub fn settle_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    now: DateTime<Utc>,
) {
    let now_iso = iso_millis(now);
    let entry = upsert_entry(entries, pane, &now_iso);
    entry.settled = Some(generation_mark(agent, now));
    entry.snoozed = None;
    entry.archived = None;
}

pub fn snooze_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    until: DateTime<Utc>,
    now: DateTime<Utc>,
) {
    let now_iso = iso_millis(now);
    let entry = upsert_entry(entries, pane, &now_iso);
    let (seq, agent_session) = seq_mark(agent);
    entry.snoozed = Some(SnoozedMark {
        at: now_iso.clone(),
        until: iso_millis(until),
        seq,
        agent_session,
    });
    entry.settled = None;
    entry.archived = None;
}

/// Archive is a hide-kind like settle.
pub fn archive_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    now: DateTime<Utc>,
) {
    let now_iso = iso_millis(now);
    let entry = upsert_entry(entries, pane, &now_iso);
    entry.archived = Some(generation_mark(agent, now));
    entry.settled = None;
    entry.snoozed = None;
}

/// Pin is an ordering flag, not a hide-kind.
pub fn pin_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    now: DateTime<Utc>,
) {
    let now_iso = iso_millis(now);
    let entry = upsert_entry(entries, pane, &now_iso);
    entry.pinned = Some(generation_mark(agent, now));
}

/// Drop the entry once no organize state remains.
fn drop_if_empty(entries: &mut BTreeMap<String, InboxEntry>, pane_id: &str) {
    if let Some(entry) = entries.get(pane_id) {
        if !entry_has_state(entry) {
            entries.remove(pane_id);
        }
    }
}

pub fn unpin_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane_id: &str,
    now: DateTime<Utc>,
) -> bool {
    let Some(entry) = entries.get_mut(pane_id) else {
        return false;
    };
    if entry.pinned.is_none() {
        return false;
    }
    entry.pinned = None;
    entry.updated_at = iso_millis(now);
    drop_if_empty(entries, pane_id);
    true
}

pub fn unarchive_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane_id: &str,
    now: DateTime<Utc>,
) -> bool {
    let Some(entry) = entries.get_mut(pane_id) else {
        return false;
    };
    if entry.archived.is_none() {
        return false;
    }
    entry.archived = None;
    entry.updated_at = iso_millis(now);
    drop_if_empty(entries, pane_id);
    true
}

#[derive(Debug, Clone)]
pub struct NewPaneLink {
    pub kind: LinkKind,
    pub repo: String,
    pub number: i64,
    pub ref_: String,
}

/// Declare (or re-declare) a tracker link on a pane.
pub fn link_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane: &PaneInfo,
    agent: Option<&AgentInfo>,
    link: NewPaneLink,
    now: DateTime<Utc>,
) {
    let now_iso = iso_millis(now);
    let entry = upsert_entry(entries, pane, &now_iso);
    let (seq, agent_session) = seq_mark(agent);
    let record = PaneLink {
        kind: link.kind,
        repo: link.repo,
        number: link.number,
        ref_: link.ref_,
        at: now_iso,
        seq,
        agent_session,
    };
    let links = entry.links.get_or_insert_with(Vec::new);
    if let Some(existing) = links
        .iter_mut()
        .find(|l| l.kind == record.kind && l.repo == record.repo && l.number == record.number)
    {
        *existing = record;
    } else {
        links.push(record);
    }
}

/// Remove one declared link; returns the removed record for echo.
pub fn unlink_pane(
    entries: &mut BTreeMap<String, InboxEntry>,
    pane_id: &str,
    kind: &str,
    repo: &str,
    number: i64,
    now: DateTime<Utc>,
) -> Option<PaneLink> {
    let entry = entries.get_mut(pane_id)?;
    let links = entry.links.as_mut()?;
    let index = links
        .iter()
        .position(|l| l.kind == kind && l.repo == repo && l.number == number)?;
    let removed = links.remove(index);
    entry.updated_at = iso_millis(now);
    if links.is_empty() {
        entry.links = None;
    }
    drop_if_empty(entries, pane_id);
    Some(removed)
}

/// Restore clears organize state (marks, pin, order) but preserves
/// declared tracker links.
pub fn restore_pane(entries: &mut BTreeMap<String, InboxEntry>, pane_id: &str) -> bool {
    let Some(entry) = entries.remove(pane_id) else {
        return false;
    };
    if let Some(links) = entry.links {
        if !links.is_empty() {
            entries.insert(
                pane_id.to_string(),
                InboxEntry {
                    pane_id: pane_id.to_string(),
                    workspace_id: entry.workspace_id,
                    settled: None,
                    snoozed: None,
                    archived: None,
                    pinned: None,
                    order: None,
                    links: Some(links),
                    updated_at: entry.updated_at,
                },
            );
        }
    }
    true
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderPlacement {
    First,
    Last,
    Before(String),
    After(String),
}

/// Compute dense order ranks for a manual reorder. `siblings` must be
/// the visible inbox items of the moved pane's display group in current
/// display order, including the moved pane.
pub fn compute_order_ranks(
    siblings: &[InboxItem],
    moved_pane_id: &str,
    placement: &OrderPlacement,
) -> Result<BTreeMap<String, i64>, String> {
    let Some(moved_idx) = siblings.iter().position(|i| i.pane_id == moved_pane_id) else {
        return Err(format!(
            "pane is not in the inbox order group: {}",
            moved_pane_id
        ));
    };
    let moved = &siblings[moved_idx];
    let rest: Vec<&InboxItem> = siblings
        .iter()
        .filter(|i| i.pane_id != moved_pane_id)
        .collect();

    let index = match placement {
        OrderPlacement::First => 0usize,
        OrderPlacement::Last => rest.len(),
        OrderPlacement::Before(ref_id) | OrderPlacement::After(ref_id) => {
            let ref_index = rest.iter().position(|i| i.pane_id == *ref_id);
            match ref_index {
                None => {
                    return Err(format!(
                        "reference pane is not in the same inbox group: {}",
                        ref_id
                    ))
                }
                Some(idx) => {
                    if matches!(placement, OrderPlacement::Before(_)) {
                        idx
                    } else {
                        idx + 1
                    }
                }
            }
        }
    };
    let _ = moved;

    let mut sequence: Vec<&InboxItem> = Vec::with_capacity(siblings.len());
    sequence.extend(rest[..index].iter().copied());
    sequence.push(moved);
    sequence.extend(rest[index..].iter().copied());

    let mut ranks = BTreeMap::new();
    for (i, item) in sequence.iter().enumerate() {
        ranks.insert(item.pane_id.clone(), i as i64);
    }
    Ok(ranks)
}

#[derive(Default)]
pub struct PruneOptions {
    /// Entries for panes absent from the snapshot are dropped after this
    /// age. Default 24h.
    pub absent_retention_ms: Option<i64>,
    /// Hard cap on stored entries; oldest `updated_at` dropped first.
    /// Default 500.
    pub max_entries: Option<usize>,
}

/// Bound state growth: drop organize entries whose pane has been absent
/// from the session snapshot longer than the retention window, then
/// enforce a hard entry cap by evicting the oldest `updated_at`.
pub fn prune_state(
    entries: &mut BTreeMap<String, InboxEntry>,
    live_pane_ids: &BTreeSet<String>,
    now: DateTime<Utc>,
    options: &PruneOptions,
) -> Vec<String> {
    let retention_ms = options.absent_retention_ms.unwrap_or(86_400_000);
    let max_entries = options.max_entries.unwrap_or(500);
    let mut removed: Vec<String> = Vec::new();
    let now_ms = now.timestamp_millis();

    let pane_ids: Vec<String> = entries.keys().cloned().collect();
    for pane_id in pane_ids {
        if live_pane_ids.contains(&pane_id) {
            continue;
        }
        let entry = entries.get(&pane_id).expect("entry exists");
        // TS: `Number.isFinite(age)` — an unparseable updated_at yields NaN
        // and is NOT age-pruned (it still counts toward the hard cap).
        let age = entry
            .updated_at
            .parse::<DateTime<Utc>>()
            .ok()
            .map(|dt| now_ms - dt.timestamp_millis());
        if let Some(age) = age {
            if age > retention_ms {
                entries.remove(&pane_id);
                removed.push(pane_id);
                continue;
            }
        }
    }

    let remaining = entries.len();
    if remaining > max_entries {
        let mut sorted: Vec<(String, String)> = entries
            .iter()
            .map(|(k, e)| (k.clone(), e.updated_at.clone()))
            .collect();
        sorted.sort_by(|a, b| a.1.cmp(&b.1));
        for (pane_id, _) in sorted.iter().take(remaining - max_entries) {
            entries.remove(pane_id);
            removed.push(pane_id.clone());
        }
    }
    removed
}

/// Read-side list filters; they never mutate state.
#[derive(Debug, Default, Clone)]
pub struct InboxFilters {
    /// Keep only items currently requiring attention (`blocked`).
    pub attention: Option<bool>,
    /// Keep items whose organize kind matches; "none" = no hide-kind mark.
    pub organize: Option<String>,
    /// Case-insensitive substring over title, cwd, and pane_id.
    pub search: Option<String>,
    /// Exact workspace_id match.
    pub workspace: Option<String>,
}

pub fn apply_inbox_filters(items: &[InboxItem], filters: &InboxFilters) -> Vec<InboxItem> {
    let needle = filters
        .search
        .as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    items
        .iter()
        .filter(|item| {
            if filters.attention == Some(true) && !item.attention {
                return false;
            }
            if let Some(org) = &filters.organize {
                let matches = if org == "none" {
                    item.organize.is_none()
                } else {
                    item.organize
                        .as_ref()
                        .map(|o| o.kind == org.as_str())
                        .unwrap_or(false)
                };
                if !matches {
                    return false;
                }
            }
            if let Some(ws) = &filters.workspace {
                if item.workspace_id.as_deref() != Some(ws.as_str()) {
                    return false;
                }
            }
            if let Some(needle) = &needle {
                let haystack = [
                    item.title.as_deref(),
                    item.cwd.as_deref(),
                    Some(item.pane_id.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
                if !haystack.contains(needle.as_str()) {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Build the inbox view: every live pane with agent activity plus any
/// pane carrying organize state, resolved against current agent info.
pub fn build_inbox(
    panes: &[PaneInfo],
    agents: &BTreeMap<String, AgentInfo>,
    entries: &mut BTreeMap<String, InboxEntry>,
    now: DateTime<Utc>,
) -> Vec<InboxItem> {
    let mut items: Vec<InboxItem> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for pane in panes {
        seen.insert(pane.pane_id.clone());
        let agent = agents.get(&pane.pane_id);
        let resolved = if let Some(entry) = entries.get_mut(&pane.pane_id) {
            resolve_entry(entry, agent, now)
        } else {
            ResolveEntryResult::default()
        };
        let organize = resolved.organize;
        let entry_snapshot = entries.get(&pane.pane_id);

        let status = agent.and_then(|a| a.agent_status.clone());
        let attention = status
            .as_deref()
            .map(|s| ATTENTION_STATUSES.contains(&s))
            .unwrap_or(false);
        let hidden = organize.is_some() && !attention;

        items.push(InboxItem {
            pane_id: pane.pane_id.clone(),
            workspace_id: pane
                .workspace_id
                .clone()
                .or_else(|| entry_snapshot.and_then(|e| e.workspace_id.clone())),
            tab_id: pane.tab_id.clone(),
            title: agent
                .and_then(|a| a.terminal_title_stripped.clone())
                .or_else(|| pane.title.clone()),
            cwd: agent
                .and_then(|a| a.cwd.clone().or_else(|| a.foreground_cwd.clone()))
                .or_else(|| pane.cwd.clone()),
            agent_status: status,
            state_change_seq: agent.and_then(|a| a.state_change_seq).map(|v| v as i64),
            agent_session: agent_session_ref(agent),
            organize,
            pinned: entry_snapshot.map(|e| e.pinned.is_some()).unwrap_or(false),
            order: entry_snapshot.and_then(|e| e.order),
            attention,
            in_inbox: !hidden,
            links: entry_snapshot
                .and_then(|e| e.links.as_ref())
                .map(|links| links.iter().map(to_public_link).collect())
                .unwrap_or_default(),
        });
    }

    // Organize entries for panes missing from the snapshot are retained
    // in state but reported as absent rather than fabricated.
    let entry_ids: Vec<String> = entries.keys().cloned().collect();
    for pane_id in entry_ids {
        if seen.contains(&pane_id) {
            continue;
        }
        let resolved = {
            let entry = entries.get_mut(&pane_id).expect("entry exists");
            resolve_entry(entry, None, now)
        };
        let entry = entries.get(&pane_id).expect("entry exists");
        let links: Vec<InboxLink> = entry
            .links
            .as_ref()
            .map(|l| l.iter().map(to_public_link).collect())
            .unwrap_or_default();
        if resolved.organize.is_some() || !links.is_empty() {
            items.push(InboxItem {
                pane_id: pane_id.clone(),
                workspace_id: entry.workspace_id.clone(),
                tab_id: None,
                title: None,
                cwd: None,
                agent_status: None,
                state_change_seq: None,
                agent_session: None,
                organize: resolved.organize,
                pinned: entry.pinned.is_some(),
                order: entry.order,
                attention: false,
                in_inbox: false,
                links,
            });
        }
    }

    // Display order: pinned first, then attention, then manual order
    // rank, then pane id.
    items.sort_by(|a, b| {
        if a.pinned != b.pinned {
            return if a.pinned {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        if a.attention != b.attention {
            return if a.attention {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let ao = a.order.unwrap_or(i64::MAX);
        let bo = b.order.unwrap_or(i64::MAX);
        if ao != bo {
            return ao.cmp(&bo);
        }
        a.pane_id.cmp(&b.pane_id)
    });
    items
}

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

/// ISO-8601 with milliseconds and `Z`, matching JS `Date.toISOString()`.
pub fn iso_millis(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// Lenient ISO-8601 parse for stored timestamps (`Date.parse` parity for
/// the formats the plugin itself writes plus common variants).
pub fn parse_instant(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    // RFC3339 / ISO-8601 with offset or Z.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // "YYYY-MM-DDTHH:MM:SS(.fff)?" without zone → assume UTC.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(s, fmt) {
            return date
                .and_hms_opt(0, 0, 0)
                .map(|dt| Utc.from_utc_datetime(&dt));
        }
    }
    None
}
