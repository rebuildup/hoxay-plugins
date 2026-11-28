//! Pure logic for `inbox watch-pr` (Issue #178): turn `git pr view` /
//! `git pr comments` / `git pr checks` payloads into a normalized PR
//! observation, diff consecutive observations into attention-return
//! triggers, and manage watch-target selection and baseline pruning.
//!
//! Rust port of `plugins/inbox/src/watchpr.ts`. Everything here is
//! I/O-free so the state machine is unit-testable. Only *observed* forge
//! data drives triggers — never PTY text, never guessed statuses.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use hoxay_herdr_client::snapshot::{AgentInfo, PaneInfo};
use serde_json::Value;

use crate::model::resolve_entry;
use crate::state::{
    InboxEntry, LinkWatch, LinkWatchObservation, PaneLink, SessionWatchMap, WatchBaselines,
    WatchTerminal,
};

// ---------------------------------------------------------------------------
// Observation building
// ---------------------------------------------------------------------------

fn as_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn login_of(value: Option<&Value>) -> Option<String> {
    let v = value?;
    if let Some(s) = v.as_str() {
        return if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }
    as_string(v.get("login"))
}

/// gh `pr checks` buckets that mean "this check is not passing".
const FAILING_CHECK_BUCKETS: &[&str] = &["fail", "cancel"];

fn sorted_unique(values: Vec<String>) -> Vec<String> {
    let set: BTreeSet<String> = values.into_iter().collect();
    set.into_iter().collect()
}

fn same_login(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// Build a normalized observation from the three git-plugin CLI
/// payloads. `view_doc` is the `pr view` stdout document (`{…, pr: {…}}`),
/// `comments_doc` the `pr comments` document, and `checks_doc` the
/// `pr checks` document. Returns an error string when a payload is
/// missing its expected shape — callers treat that as a failed poll.
pub fn build_observation(
    view_doc: &Value,
    comments_doc: &Value,
    checks_doc: &Value,
) -> Result<LinkWatchObservation, String> {
    let pr = view_doc.get("pr").and_then(|v| v.as_object());
    let Some(pr) = pr else {
        return Err("git pr view payload missing `pr` object".to_string());
    };
    if pr.is_empty() {
        return Err("git pr view payload missing `pr` object".to_string());
    }
    let get = |key: &str| pr.get(key);

    let author = login_of(get("author"));

    // Reviews: newest ≤10 reviews as {author, state, submittedAt}.
    // Self-authored reviews never wake the pane (self proxy).
    let mut review_keys: Vec<String> = Vec::new();
    if let Some(reviews) = get("reviews").and_then(|v| v.as_array()) {
        for entry in reviews {
            let reviewer = login_of(entry.get("author"));
            if same_login(reviewer.as_deref(), author.as_deref()) {
                continue;
            }
            review_keys.push(format!(
                "{}|{}|{}",
                reviewer.as_deref().unwrap_or("?"),
                as_string(entry.get("state")).as_deref().unwrap_or("?"),
                as_string(entry.get("submittedAt"))
                    .as_deref()
                    .unwrap_or("?"),
            ));
        }
    }

    // Comments: issue (conversation) + line-level review comments.
    let mut comment_keys: Vec<String> = Vec::new();
    for list_name in ["issue_comments", "line_comments"] {
        if let Some(list) = comments_doc.get(list_name).and_then(|v| v.as_array()) {
            for entry in list {
                if same_login(login_of(entry.get("author")).as_deref(), author.as_deref()) {
                    continue;
                }
                if let Some(id) = entry.get("id") {
                    if id.is_number() || id.is_string() {
                        comment_keys.push(
                            id.as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| id.to_string()),
                        );
                    }
                }
            }
        }
    }

    // Checks: `gh pr checks --json` buckets — fail/cancel count as failing.
    let mut failing_checks: Vec<String> = Vec::new();
    if let Some(checks) = checks_doc.get("checks").and_then(|v| v.as_array()) {
        for entry in checks {
            let bucket = as_string(entry.get("bucket")).map(|b| b.to_lowercase());
            if let Some(bucket) = bucket {
                if FAILING_CHECK_BUCKETS.contains(&bucket.as_str()) {
                    failing_checks.push(
                        as_string(entry.get("name"))
                            .or_else(|| as_string(entry.get("state")))
                            .unwrap_or_else(|| "?".to_string()),
                    );
                }
            }
        }
    }

    let mergeable = get("mergeable");
    Ok(LinkWatchObservation {
        state: as_string(get("state"))
            .unwrap_or_else(|| "UNKNOWN".to_string())
            .to_uppercase(),
        is_draft: get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false),
        merge_state_status: as_string(get("mergeStateStatus")),
        mergeable: mergeable.and_then(|v| v.as_str()).map(|s| s.to_string()),
        updated_at: as_string(get("updatedAt")),
        author,
        failing_checks: sorted_unique(failing_checks),
        review_keys: sorted_unique(review_keys),
        comment_keys: sorted_unique(comment_keys),
    })
}

// ---------------------------------------------------------------------------
// Diffing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchTrigger {
    Merged,
    Closed,
    CheckFailed,
    MergeConflict,
    Review,
    Comment,
}

impl WatchTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            WatchTrigger::Merged => "merged",
            WatchTrigger::Closed => "closed",
            WatchTrigger::CheckFailed => "check_failed",
            WatchTrigger::MergeConflict => "merge_conflict",
            WatchTrigger::Review => "review",
            WatchTrigger::Comment => "comment",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WatchDiff {
    pub triggers: Vec<WatchTrigger>,
    pub terminal: Option<WatchTerminal>,
    /// New failing check names (for the event payload).
    pub new_failing_checks: Vec<String>,
    pub new_review_count: i64,
    pub new_comment_count: i64,
}

fn terminal_state(state: &str) -> Option<WatchTerminal> {
    match state {
        "MERGED" => Some("merged".to_string()),
        "CLOSED" => Some("closed".to_string()),
        _ => None,
    }
}

/// Diff a fresh observation against the stored baseline. The first
/// observation never triggers — it only establishes the baseline, and an
/// already-terminal PR is recorded terminal silently.
pub fn diff_observation(
    prev: Option<&LinkWatchObservation>,
    next: &LinkWatchObservation,
) -> WatchDiff {
    let mut diff = WatchDiff::default();
    let Some(prev) = prev else {
        diff.terminal = terminal_state(&next.state);
        return diff;
    };

    // Lifecycle: only a transition *into* MERGED/CLOSED qualifies.
    if let Some(terminal) = terminal_state(&next.state) {
        if prev.state != next.state {
            diff.triggers.push(if terminal == "merged" {
                WatchTrigger::Merged
            } else {
                WatchTrigger::Closed
            });
            diff.terminal = Some(terminal);
        }
    }

    let prev_failing: BTreeSet<&String> = prev.failing_checks.iter().collect();
    diff.new_failing_checks = next
        .failing_checks
        .iter()
        .filter(|name| !prev_failing.contains(name))
        .cloned()
        .collect();
    if !diff.new_failing_checks.is_empty() {
        diff.triggers.push(WatchTrigger::CheckFailed);
    }

    // Merge conflict: transition INTO DIRTY only.
    if next.merge_state_status.as_deref() == Some("DIRTY")
        && prev.merge_state_status.as_deref() != Some("DIRTY")
    {
        diff.triggers.push(WatchTrigger::MergeConflict);
    }

    let prev_reviews: BTreeSet<&String> = prev.review_keys.iter().collect();
    diff.new_review_count = next
        .review_keys
        .iter()
        .filter(|key| !prev_reviews.contains(key))
        .count() as i64;
    if diff.new_review_count > 0 {
        diff.triggers.push(WatchTrigger::Review);
    }

    let prev_comments: BTreeSet<&String> = prev.comment_keys.iter().collect();
    diff.new_comment_count = next
        .comment_keys
        .iter()
        .filter(|key| !prev_comments.contains(key))
        .count() as i64;
    if diff.new_comment_count > 0 {
        diff.triggers.push(WatchTrigger::Comment);
    }

    diff
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Exponential failure backoff, capped at 30 minutes.
pub fn poll_backoff_ms(interval_ms: i64, consecutive_failures: i64) -> i64 {
    let exponent = consecutive_failures.clamp(1, 5);
    let backoff = interval_ms.saturating_mul(2i64.pow(exponent as u32));
    backoff.min(30 * 60_000)
}

// ---------------------------------------------------------------------------
// Target selection & baseline pruning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WatchTarget {
    pub pane_id: String,
    pub link: PaneLink,
    /// Ref key used in the watch map (`<repo>#<number>`).
    pub ref_: String,
    pub baseline: Option<LinkWatch>,
}

#[derive(Debug, Default)]
pub struct WatchTargetScan {
    /// Live-pane, still-linked PR targets (terminal/deferred flagged on
    /// baseline).
    pub targets: Vec<WatchTarget>,
    /// Pane ids carrying links but absent from the snapshot.
    pub absent_pane_ids: Vec<String>,
    /// Total declared `pr` links seen (any pane state).
    pub total_pr_links: usize,
    /// Links whose baseline is already terminal.
    pub terminal_count: usize,
}

/// Collect watch targets from resolved session entries. `entries` must
/// already have gone through `resolve_state`/`resolve_entry` so stale
/// links are filtered. Only `kind === "pr"` links are watched.
pub fn collect_watch_targets(
    entries: &BTreeMap<String, InboxEntry>,
    watch_session: Option<&SessionWatchMap>,
    live_pane_ids: &BTreeSet<String>,
    pane_filter: Option<&str>,
) -> WatchTargetScan {
    let mut scan = WatchTargetScan::default();
    for (pane_id, entry) in entries {
        if let Some(filter) = pane_filter {
            if pane_id != filter {
                continue;
            }
        }
        let pr_links: Vec<&PaneLink> = entry
            .links
            .as_ref()
            .map(|links| links.iter().filter(|l| l.kind == "pr").collect())
            .unwrap_or_default();
        scan.total_pr_links += pr_links.len();
        if pr_links.is_empty() {
            continue;
        }
        if !live_pane_ids.contains(pane_id) {
            scan.absent_pane_ids.push(pane_id.clone());
            continue;
        }
        let pane_watch = watch_session.and_then(|w| w.get(pane_id));
        for link in pr_links {
            let baseline = pane_watch.and_then(|pw| pw.get(&link.ref_)).cloned();
            if baseline
                .as_ref()
                .and_then(|b| b.terminal.as_ref())
                .is_some()
            {
                scan.terminal_count += 1;
            }
            scan.targets.push(WatchTarget {
                pane_id: pane_id.clone(),
                link: link.clone(),
                ref_: link.ref_.clone(),
                baseline,
            });
        }
    }
    scan
}

/// Drop watch baselines that no longer correspond to a live declared PR
/// link. Called inside the state commit so baselines cannot leak or
/// resurrect for recycled identities. Returns the number of removed
/// baselines.
/// `watch` is `&mut state.watch` passed separately so callers can hold
/// the session-entries borrow at the same time (disjoint fields).
pub fn prune_watch_session(
    watch: &mut Option<WatchBaselines>,
    session_key: &str,
    entries: &mut BTreeMap<String, InboxEntry>,
    agents: &BTreeMap<String, AgentInfo>,
    panes: &[PaneInfo],
    now: DateTime<Utc>,
) -> usize {
    let has_watch = watch
        .as_ref()
        .map(|w| w.contains_key(session_key))
        .unwrap_or(false);
    if !has_watch {
        return 0;
    }
    // Collect pane ids first to satisfy the borrow checker; entries are
    // mutated via resolve_entry below.
    let pane_ids: Vec<String> = {
        let w = watch.as_ref().unwrap().get(session_key).unwrap();
        w.keys().cloned().collect()
    };
    let live_pane_ids: BTreeSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    let mut removed = 0usize;

    for pane_id in pane_ids {
        // Re-resolve the entry so staleness is judged against live data.
        let mut live_links: Vec<PaneLink> = Vec::new();
        if live_pane_ids.contains(&pane_id) {
            if let Some(entry) = entries.get_mut(&pane_id) {
                if panes.iter().any(|p| p.pane_id == pane_id) {
                    resolve_entry(entry, agents.get(&pane_id), now);
                    live_links = entry
                        .links
                        .as_ref()
                        .map(|links| links.iter().filter(|l| l.kind == "pr").cloned().collect())
                        .unwrap_or_default();
                }
            }
        }
        let live_refs: BTreeSet<String> = live_links.iter().map(|l| l.ref_.clone()).collect();

        {
            let w = watch
                .as_mut()
                .and_then(|wm| wm.get_mut(session_key))
                .expect("watch session exists");
            let Some(pane_watch) = w.get_mut(&pane_id) else {
                continue;
            };
            let refs: Vec<String> = pane_watch.keys().cloned().collect();
            for link_ref in refs {
                if !live_refs.contains(&link_ref) {
                    pane_watch.remove(&link_ref);
                    removed += 1;
                }
            }
            if pane_watch.is_empty() {
                w.remove(&pane_id);
                removed += 1;
            }
        }
    }
    let session_empty = watch
        .as_ref()
        .and_then(|w| w.get(session_key))
        .map(|s| s.is_empty())
        .unwrap_or(false);
    if session_empty {
        if let Some(w) = watch.as_mut() {
            w.remove(session_key);
        }
    }
    removed
}
