//! View-model and parsers for the inbox TUI (`src/tui.rs`, contract
//! `herdr-plugins.tui/v1`). Pure functions only — no I/O, no process
//! control — so the whole surface is unit-testable without a PTY or a
//! live herdr session. Rust port of `plugins/inbox/src/tui-model.ts`.
//!
//! The TUI calls the domain in-process (model/state/transitions) rather
//! than respawning the CLI; the JSON shapes below still mirror the
//! `herdr-plugins.inbox/v1` `list` payload so tests and external
//! consumers see identical wire data.

use serde::Deserialize;
use serde_json::Value;

use hoxay_tui::safety::sanitize_display;

use crate::state::AgentSessionRef;

// --- CLI JSON shapes -------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListCounts {
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub inbox: i64,
    #[serde(default)]
    pub attention: i64,
    #[serde(default)]
    pub organized: i64,
    #[serde(default)]
    pub pinned: i64,
    #[serde(default)]
    pub archived: i64,
    #[serde(default)]
    pub linked: i64,
}

/// Deserialize-side mirror of `model::InboxItem` (which is
/// serialize-only in the domain crate).
#[derive(Debug, Clone, Deserialize)]
pub struct TuiItem {
    pub pane_id: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub state_change_seq: Option<i64>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionRef>,
    #[serde(default)]
    pub organize: Option<TuiOrganize>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub attention: bool,
    #[serde(default)]
    pub in_inbox: bool,
    #[serde(default)]
    pub links: Vec<TuiLink>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TuiOrganize {
    pub kind: String,
    pub since: String,
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TuiLink {
    pub kind: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub number: i64,
    #[serde(rename = "ref")]
    pub ref_: String,
    #[serde(default)]
    pub at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListSession {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub socket: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListPayload {
    #[serde(default)]
    pub contract: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub session: Option<ListSession>,
    #[serde(default)]
    pub state_file: Option<String>,
    #[serde(default)]
    pub counts: Option<ListCounts>,
    #[serde(default)]
    pub items: Option<Vec<TuiItem>>,
}

pub enum CliJsonResult {
    Ok(ListPayload),
    Err(String),
}

/// Parse `inbox list` JSON stdout (or an in-process equivalent).
/// Never fails hard — mirrors `parseListOutput`.
pub fn parse_list_output(stdout: &str) -> CliJsonResult {
    let parsed: Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return CliJsonResult::Err("cli returned non-JSON output".to_string()),
    };
    if !parsed.is_object() {
        return CliJsonResult::Err("cli returned an unexpected payload".to_string());
    }
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        let msg = parsed
            .get("error")
            .and_then(|e| {
                e.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| e.as_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| "unknown error".to_string());
        return CliJsonResult::Err(msg);
    }
    let payload: ListPayload = match serde_json::from_value(parsed) {
        Ok(p) => p,
        Err(_) => return CliJsonResult::Err("cli payload has no items array".to_string()),
    };
    if payload.items.is_none() {
        return CliJsonResult::Err("cli payload has no items array".to_string());
    }
    CliJsonResult::Ok(payload)
}

/// Extract an honest error message from a failed CLI invocation.
pub fn cli_error_message(status: i32, stdout: &str, stderr: &str) -> String {
    let text = {
        let t = stderr.trim();
        if t.is_empty() {
            stdout.trim()
        } else {
            t
        }
    };
    if !text.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if let Some(err) = parsed.get("error") {
                if let Some(msg) = err.get("message").and_then(|m| m.as_str()) {
                    return msg.to_string();
                }
                if let Some(s) = err.as_str() {
                    return s.to_string();
                }
            }
        }
        return text.chars().take(300).collect();
    }
    format!("cli exited with status {}", status)
}

// --- Display formatting -----------------------------------------------------------

fn organize_label(kind: &str) -> &str {
    kind
}

/// Short badge string for one inbox item (pin / attention / organize / links).
pub fn item_badges(item: &TuiItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if item.pinned {
        parts.push("pin".to_string());
    }
    if item.attention {
        parts.push("!".to_string());
    }
    if let Some(org) = &item.organize {
        let kind = organize_label(&org.kind);
        if org.kind == "snoozed" {
            if let Some(until) = &org.until {
                parts.push(format!("{}→{}", kind, until));
                // fallthrough handled below — kind already pushed
            } else {
                parts.push(kind.to_string());
            }
        } else {
            parts.push(kind.to_string());
        }
    }
    if !item.in_inbox && item.organize.is_none() {
        parts.push("hidden".to_string());
    }
    if !item.links.is_empty() {
        parts.push(format!("links:{}", item.links.len()));
    }
    // Badge text can carry state-derived values (e.g. snooze `until`) —
    // sanitize before it can reach the renderer (tui-v1 §5 item 8).
    sanitize_display(&parts.join(" "))
}

/// Primary list-row text: title (or pane id fallback) + pane id.
/// Untrusted pane titles are sanitized before display.
pub fn item_title(item: &TuiItem) -> String {
    sanitize_display(&item.title.clone().unwrap_or_else(|| item.pane_id.clone()))
}

/// Searchable text for `/` filtering — mirrors the CLI's search fields
/// (title, cwd, pane_id) plus link refs and workspace for convenience.
pub fn item_search_text(item: &TuiItem) -> String {
    let mut fields: Vec<String> = Vec::new();
    if let Some(t) = &item.title {
        fields.push(t.clone());
    }
    if let Some(c) = &item.cwd {
        fields.push(c.clone());
    }
    fields.push(item.pane_id.clone());
    if let Some(w) = &item.workspace_id {
        fields.push(w.clone());
    }
    if let Some(s) = &item.agent_status {
        fields.push(s.clone());
    }
    for l in &item.links {
        fields.push(l.ref_.clone());
    }
    fields.join("\n")
}

/// Scope filter for the main list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxScope {
    Inbox,
    All,
    Hidden,
    Attention,
}

pub const INBOX_SCOPES: &[InboxScope] = &[
    InboxScope::Inbox,
    InboxScope::All,
    InboxScope::Hidden,
    InboxScope::Attention,
];

pub fn scope_label(scope: InboxScope) -> &'static str {
    match scope {
        InboxScope::Inbox => "inbox",
        InboxScope::All => "all",
        InboxScope::Hidden => "organized/hidden",
        InboxScope::Attention => "attention",
    }
}

/// Next scope in the cycle (f key).
pub fn next_scope(scope: InboxScope) -> InboxScope {
    let i = INBOX_SCOPES.iter().position(|s| *s == scope).unwrap_or(0);
    INBOX_SCOPES[(i + 1) % INBOX_SCOPES.len()]
}

/// Apply scope + free-text query. Case-insensitive AND over terms.
pub fn visible_items(items: &[TuiItem], scope: InboxScope, query: &str) -> Vec<TuiItem> {
    let mut out: Vec<TuiItem> = match scope {
        InboxScope::Inbox => items.iter().filter(|i| i.in_inbox).cloned().collect(),
        InboxScope::Hidden => items.iter().filter(|i| !i.in_inbox).cloned().collect(),
        InboxScope::Attention => items.iter().filter(|i| i.attention).cloned().collect(),
        InboxScope::All => items.to_vec(),
    };
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return out;
    }
    let terms: Vec<&str> = q.split_whitespace().filter(|t| !t.is_empty()).collect();
    out.retain(|item| {
        let hay = item_search_text(item).to_lowercase();
        terms.iter().all(|t| hay.contains(t))
    });
    out
}

/// Human-readable detail lines for the read-only detail view.
pub fn item_detail_lines(item: &TuiItem, generated_at: Option<&str>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("pane:      {}", item.pane_id));
    if let Some(w) = &item.workspace_id {
        lines.push(format!("workspace: {}", w));
    }
    if let Some(t) = &item.tab_id {
        lines.push(format!("tab:       {}", t));
    }
    if let Some(t) = &item.title {
        lines.push(format!("title:     {}", t));
    }
    if let Some(c) = &item.cwd {
        lines.push(format!("cwd:       {}", c));
    }
    if let Some(s) = &item.agent_status {
        lines.push(format!("agent:     {}", s));
    }
    if let Some(seq) = item.state_change_seq {
        lines.push(format!("seq:       {}", seq));
    }
    if let Some(sess) = &item.agent_session {
        let parts: Vec<&str> = [
            sess.agent.as_deref(),
            sess.kind.as_deref(),
            sess.value.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
        if !parts.is_empty() {
            lines.push(format!("session:  {}", parts.join("/")));
        }
    }
    lines.push(format!(
        "pinned:    {}",
        if item.pinned { "yes" } else { "no" }
    ));
    if let Some(order) = item.order {
        lines.push(format!("order:     {}", order));
    }
    lines.push(format!(
        "attention: {}",
        if item.attention { "yes" } else { "no" }
    ));
    lines.push(format!(
        "inbox:     {}",
        if item.in_inbox { "visible" } else { "hidden" }
    ));
    if let Some(o) = &item.organize {
        let until = o
            .until
            .as_deref()
            .map(|u| format!(" until {}", u))
            .unwrap_or_default();
        lines.push(format!("organize: {} since {}{}", o.kind, o.since, until));
    }
    if !item.links.is_empty() {
        lines.push("links:".to_string());
        for link in &item.links {
            lines.push(format!(
                "  {}  {}  (declared {})",
                link.kind, link.ref_, link.at
            ));
        }
    } else {
        lines.push("links:     none".to_string());
    }
    if let Some(gen) = generated_at {
        lines.push(format!("generated: {}", gen));
    }
    lines.push(String::new());
    lines.push("Mutations go through the inbox domain — the TUI never".to_string());
    lines.push("writes organize state directly. `?` shows the keymap.".to_string());
    // Detail lines interpolate untrusted pane/session/link fields —
    // sanitize each rendered line (tui-v1 §5 item 8).
    lines.into_iter().map(|l| sanitize_display(&l)).collect()
}

// --- Stale-generation revalidation (contract §3.3) ----------------------------------

pub enum StaleVerdict {
    Ok,
    Gone { reason: String },
    Stale { reason: String },
}

fn session_ref_key(s: Option<&AgentSessionRef>) -> String {
    match s {
        Some(s) => format!(
            "{}|{}|{}",
            s.agent.clone().unwrap_or_default(),
            s.kind.clone().unwrap_or_default(),
            s.value.clone().unwrap_or_default()
        ),
        None => String::new(),
    }
}

/// Compare the item the user acted on with a fresh read taken immediately
/// before a mutation. Any mismatch aborts the mutation — the TUI never
/// silently retargets.
pub fn stale_check(before: Option<&TuiItem>, after: Option<&TuiItem>) -> StaleVerdict {
    let Some(before) = before else {
        return StaleVerdict::Gone {
            reason: "no item was selected".to_string(),
        };
    };
    let Some(after) = after else {
        return StaleVerdict::Gone {
            reason: format!("pane {} is no longer in the session", before.pane_id),
        };
    };
    if session_ref_key(before.agent_session.as_ref())
        != session_ref_key(after.agent_session.as_ref())
    {
        return StaleVerdict::Stale {
            reason: format!(
                "agent session changed for {} (generation swap)",
                before.pane_id
            ),
        };
    }
    if before.state_change_seq != after.state_change_seq {
        return StaleVerdict::Stale {
            reason: format!(
                "state_change_seq advanced for {} ({} → {})",
                before.pane_id,
                before
                    .state_change_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                after
                    .state_change_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".to_string()),
            ),
        };
    }
    StaleVerdict::Ok
}

// --- Input parsers (mirror CLI validation; CLI re-validates anyway) -----------------

use regex::Regex;
use std::sync::LazyLock;

static TRACKER_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9][A-Za-z0-9_.-]*(?:/[A-Za-z0-9][A-Za-z0-9_.-]*)+)#([1-9][0-9]*)$")
        .unwrap()
});
static LINK_SPEC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(pr|pull|issue|bug)\s*[: ]\s*(\S+)$").unwrap());
static SNOOZE_SPEC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\+?\d+[mhd]|\d{4}-\d{2}-\d{2}[T ].*)$").unwrap());
static ISO_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-").unwrap());

pub enum LinkSpec {
    Ok {
        kind: &'static str,
        flag: &'static str,
        ref_: String,
    },
    Err(String),
}

/// Parse `pr owner/repo#123` / `issue owner/repo#123` (also `pull`/`pr:`
/// spellings). Anything else is a usage error shown inline.
pub fn parse_link_spec(input: &str) -> LinkSpec {
    let trimmed = input.trim();
    let Some(m) = LINK_SPEC_RE.captures(trimmed) else {
        return LinkSpec::Err(
            "expected: pr <owner>/<repo>#<n>  or  issue <owner>/<repo>#<n>".to_string(),
        );
    };
    let word = m[1].to_lowercase();
    let kind: &'static str = if word == "issue" || word == "bug" {
        "issue"
    } else {
        "pr"
    };
    let ref_ = m[2].to_string();
    if !TRACKER_REF_RE.is_match(&ref_) {
        return LinkSpec::Err(format!("invalid tracker ref: {}", ref_));
    }
    LinkSpec::Ok {
        kind,
        flag: if kind == "pr" { "--pr" } else { "--issue" },
        ref_,
    }
}

pub enum SnoozeSpec {
    Ok(String),
    Err(String),
}

/// Validate a snooze `--until` spec before prompting for confirmation.
pub fn validate_snooze_spec(input: &str) -> SnoozeSpec {
    let spec = input.trim().to_string();
    if spec.is_empty() {
        return SnoozeSpec::Err("empty snooze spec".to_string());
    }
    if !SNOOZE_SPEC_RE.is_match(&spec) {
        return SnoozeSpec::Err(
            "expected +30m / +2h / +1d, 30m, or an ISO-8601 timestamp".to_string(),
        );
    }
    if ISO_PREFIX_RE.is_match(&spec) {
        // chrono's DateTime::parse_from_rfc3339 is stricter than TS
        // Date.parse (which accepts "2024-01-01 10:00"); accept both.
        let ok = chrono::DateTime::parse_from_rfc3339(&spec).is_ok()
            || chrono::NaiveDateTime::parse_from_str(&spec, "%Y-%m-%d %H:%M:%S")
                .map(|_| true)
                .unwrap_or(false)
            || chrono::NaiveDateTime::parse_from_str(&spec, "%Y-%m-%dT%H:%M:%S")
                .map(|_| true)
                .unwrap_or(false)
            || chrono::NaiveDate::parse_from_str(&spec, "%Y-%m-%d").is_ok();
        if !ok {
            return SnoozeSpec::Err(format!("cannot parse timestamp: {}", spec));
        }
    }
    SnoozeSpec::Ok(spec)
}

// --- Order placement ---------------------------------------------------------------

pub enum OrderMove {
    Ok {
        flag: &'static str,
        ref_pane_id: String,
    },
    Err(String),
}

/// Compute the `order` placement for moving `paneId` one step up or down
/// inside its display group.
pub fn order_move_target(items: &[TuiItem], pane_id: &str, dir: OrderDir) -> OrderMove {
    let moved = items.iter().find(|i| i.pane_id == pane_id && i.in_inbox);
    let Some(moved) = moved else {
        return OrderMove::Err(format!(
            "{} is not in the inbox (hidden panes cannot be ordered)",
            pane_id
        ));
    };
    let siblings: Vec<&TuiItem> = items
        .iter()
        .filter(|i| i.in_inbox && i.attention == moved.attention && i.pinned == moved.pinned)
        .collect();
    let idx = siblings
        .iter()
        .position(|i| i.pane_id == pane_id)
        .unwrap_or(0);
    let target_idx = match dir {
        OrderDir::Up => idx as i64 - 1,
        OrderDir::Down => idx as i64 + 1,
    };
    let Some(target) = siblings.get(target_idx as usize) else {
        return OrderMove::Err(format!(
            "already at the {} of its group",
            match dir {
                OrderDir::Up => "top",
                OrderDir::Down => "bottom",
            }
        ));
    };
    OrderMove::Ok {
        flag: match dir {
            OrderDir::Up => "--before",
            OrderDir::Down => "--after",
        },
        ref_pane_id: target.pane_id.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    Up,
    Down,
}

// --- Keymap -------------------------------------------------------------------------

pub struct KeyBinding {
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

/// Single source for dispatch docs + `?` help. Mirrors INBOX_TUI_KEYMAP.
pub const INBOX_TUI_KEYMAP: &[KeyBinding] = &[
    KeyBinding {
        keys: &["↑", "k", "↓", "j"],
        description: "move selection",
    },
    KeyBinding {
        keys: &["pgup", "pgdn"],
        description: "page",
    },
    KeyBinding {
        keys: &["home", "end"],
        description: "first / last",
    },
    KeyBinding {
        keys: &["/"],
        description: "filter (text search)",
    },
    KeyBinding {
        keys: &["f"],
        description: "cycle scope: inbox → all → hidden → attention",
    },
    KeyBinding {
        keys: &["R"],
        description: "refresh list (read-only)",
    },
    KeyBinding {
        keys: &["enter"],
        description: "item detail",
    },
    KeyBinding {
        keys: &["s"],
        description: "settle pane (confirm)",
    },
    KeyBinding {
        keys: &["n"],
        description: "snooze pane (confirm)",
    },
    KeyBinding {
        keys: &["r"],
        description: "restore pane (confirm)",
    },
    KeyBinding {
        keys: &["p"],
        description: "pin / unpin (confirm)",
    },
    KeyBinding {
        keys: &["a"],
        description: "archive / unarchive (confirm)",
    },
    KeyBinding {
        keys: &["[", "]"],
        description: "move up / down in order group (confirm)",
    },
    KeyBinding {
        keys: &["l"],
        description: "link PR/Issue (confirm)",
    },
    KeyBinding {
        keys: &["u"],
        description: "unlink PR/Issue (confirm)",
    },
    KeyBinding {
        keys: &["t"],
        description: "organize-transition log (read-only)",
    },
    KeyBinding {
        keys: &["c"],
        description: "run reconcile now (explicit, confirm)",
    },
];
