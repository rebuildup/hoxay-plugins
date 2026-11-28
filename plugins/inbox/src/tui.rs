//! rebuildup.inbox TUI entrypoint — the interactive inbox surface
//! (contract `herdr-plugins.tui/v1`, ADR-0031/0032/0033, Issue #237).
//! Rust port of `plugins/inbox/src/tui.ts`.
//!
//! Modes:
//!   `inbox tui`          interactive pane UI (manifest [[panes]])
//!   `inbox tui --open`   `open` action helper: invokes `plugin.pane.open`
//!                        via HERDR_BIN_PATH, pins the origin context,
//!                        returns promptly
//!   (non-TTY stdin)      honest plain-text list, no fake TUI
//!
//! Design rules honored here:
//! - Read-only open: the first and every refresh run `list` only.
//!   `reconcile` runs solely via the explicit `c` key + confirmation.
//! - Target identity: the origin `HERDR_PLUGIN_CONTEXT_JSON` (or the
//!   `HERDR_TUI_CONTEXT_JSON` forwarded by the open helper) is pinned at
//!   launch; the UI pane's own HERDR_PANE_ID is surface identity only and
//!   is stripped from CLI child environments so it can never become an
//!   ambient mutation target.
//! - Stale generation: before every mutation the item is re-read through
//!   `list` and compared on agent_session + state_change_seq; mismatch ⇒
//!   abort with a stale_* message and a refreshed list.
//! - All mutations are confirm-gated and executed through the same
//!   domain command cores the CLI dispatches to, called in-process —
//!   the TUI never writes plugin state directly and never respawns
//!   itself. Cross-plugin calls (watch-pr → git) keep their process
//!   boundary per ADR-0013 and are not reachable from the TUI.

use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;

// Shared safety helpers (ADR-0036): every untrusted string that can
// reach the renderer goes through `sanitize_display` first.
use hoxay_tui::safety::{bound_text, sanitize_display};

use crate::tui_model::{
    self, cli_error_message, item_badges, item_detail_lines, item_title, next_scope,
    order_move_target, parse_link_spec, parse_list_output, scope_label, stale_check,
    validate_snooze_spec, visible_items, InboxScope, ListPayload, OrderDir, StaleVerdict, TuiItem,
};

const PLUGIN_ID: &str = "rebuildup.inbox";
const ENTRYPOINT_ID: &str = "main";

// --- pinned launch context ---------------------------------------------------------

#[allow(dead_code)] // origin/workspace/pane fields are pinned for parity with tui.ts
struct PinnedContext {
    origin_json: Option<String>,
    workspace_id: Option<String>,
    focused_pane_id: Option<String>,
    /// HERDR_PANE_ID of the UI pane itself — surface identity only.
    #[allow(dead_code)]
    surface_pane_id: Option<String>,
    socket_path: Option<String>,
    session: Option<String>,
}

fn capture_context() -> PinnedContext {
    // The open helper forwards the action's origin context explicitly;
    // otherwise the pane-open context herdr generated is the origin.
    let origin_json = std::env::var("HERDR_TUI_CONTEXT_JSON")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
                .ok()
                .filter(|s| !s.is_empty())
        });
    let mut workspace_id = None;
    let mut focused_pane_id = None;
    if let Some(raw) = &origin_json {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            workspace_id = parsed
                .get("workspace_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            focused_pane_id = parsed
                .get("focused_pane_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    PinnedContext {
        origin_json,
        workspace_id,
        focused_pane_id,
        surface_pane_id: std::env::var("HERDR_PANE_ID").ok(),
        socket_path: std::env::var("HERDR_SOCKET_PATH").ok(),
        session: std::env::var("HERDR_SESSION").ok(),
    }
}

// --- domain invocation (in-process) ----------------------------------------------------

struct CliResult {
    status: i32,
    stdout: String,
    stderr: String,
}

/// Run one inbox domain command in-process — the same `cmd_*` cores the
/// CLI dispatches to. No self-respawn: mutations share this process's
/// state lock and pinned session/socket targeting, and pane-scoped
/// commands always carry an explicit positional pane id so ambient
/// HERDR_PANE_ID / context JSON can never become a mutation target.
/// Process-level commands (`watch`, `watch-pr`, `_wake`) are never
/// invoked from the TUI; socket calls carry their own timeouts.
fn run_domain(args: &[String]) -> CliResult {
    use crate::cli::{OrganizeCommand, RunError};
    let mut options = crate::cli::parse_args(args);
    // Args are internally generated and well-formed; `parse_args` only
    // exits the process on malformed global flags, which cannot occur
    // for TUI-built argv.
    let result: Result<Value, RunError> = match options.command.as_str() {
        "list" => crate::cli::cmd_list(&options),
        "settle" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Settle),
        "snooze" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Snooze),
        "restore" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Restore),
        "pin" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Pin),
        "unpin" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Unpin),
        "archive" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Archive),
        "unarchive" => crate::cli::cmd_organize(&mut options, OrganizeCommand::Unarchive),
        "order" => crate::cli::cmd_order(&mut options),
        "link" => crate::cli::cmd_link(&mut options),
        "unlink" => crate::cli::cmd_unlink(&mut options),
        "reconcile" => crate::cli::cmd_reconcile(&mut options),
        "transitions" => crate::cli::cmd_transitions(&options),
        other => Err(RunError::usage(format!(
            "command not available to the TUI: {}",
            other
        ))),
    };
    match result {
        Ok(payload) => CliResult {
            status: 0,
            // Same pretty-printed document the CLI `emit` writes.
            stdout: serde_json::to_string_pretty(&payload).unwrap_or_default(),
            stderr: String::new(),
        },
        Err(e) => CliResult {
            status: e.code(),
            stdout: String::new(),
            stderr: e.stderr_doc(),
        },
    }
}

enum ListOutcome {
    Ok { payload: ListPayload },
    Err(String),
}

fn run_list() -> ListOutcome {
    let result = run_domain(&["list".to_string()]);
    if result.status != 0 {
        return ListOutcome::Err(cli_error_message(
            result.status,
            &result.stdout,
            &result.stderr,
        ));
    }
    match parse_list_output(&result.stdout) {
        tui_model::CliJsonResult::Ok(p) => ListOutcome::Ok { payload: p },
        tui_model::CliJsonResult::Err(e) => ListOutcome::Err(e),
    }
}

// --- app state ------------------------------------------------------------------------

#[derive(Clone)]
enum ViewState {
    Loading,
    Error(String),
    Ready,
}

/// A pending confirm-gated mutation.
struct PendingMutation {
    title: String,
    target_lines: Vec<String>,
    consequence: String,
    danger: bool,
    args: Vec<String>,
    ok_message: String,
}

/// Modal prompt kind. (The `/` list filter uses `filter_mode` on the
/// main view instead of this prompt.)
enum PromptKind {
    /// Snooze spec input for a pane.
    Snooze { pane_id: String },
    /// Link spec input for a pane.
    Link { pane_id: String },
    /// Unlink spec input for a pane.
    Unlink { pane_id: String, hints: Vec<String> },
}

enum Screen {
    Main,
    Detail {
        title: String,
        lines: Vec<String>,
    },
    Prompt {
        kind: PromptKind,
        draft: String,
        placeholder: String,
        label: String,
    },
    Confirm {
        mutation: PendingMutation,
    },
    Help,
    Transitions {
        lines: Vec<String>,
    },
}

struct App {
    ctx: PinnedContext,
    screen: Screen,
    // main-list state
    items: Vec<TuiItem>,
    generated_at: Option<String>,
    state: ViewState,
    scope: InboxScope,
    query: String,
    cursor: usize,
    scroll_top: usize,
    filter_mode: bool,
    filter_draft: String,
    status: String,
}

impl App {
    fn new(ctx: PinnedContext) -> Self {
        App {
            ctx,
            screen: Screen::Main,
            items: Vec::new(),
            generated_at: None,
            state: ViewState::Loading,
            scope: InboxScope::Inbox,
            query: String::new(),
            cursor: 0,
            scroll_top: 0,
            filter_mode: false,
            filter_draft: String::new(),
            status: "loading…".to_string(),
        }
    }

    fn session_label(&self) -> String {
        self.ctx
            .session
            .clone()
            .or_else(|| self.ctx.socket_path.clone())
            .unwrap_or_else(|| "unknown session".to_string())
    }

    fn filtered(&self) -> Vec<TuiItem> {
        visible_items(&self.items, self.scope, &self.query)
    }

    fn clamp_cursor(&mut self) {
        let n = self.filtered().len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
        if self.scroll_top > n.saturating_sub(1) {
            self.scroll_top = n.saturating_sub(1);
        }
    }

    fn current(&self) -> Option<TuiItem> {
        self.filtered().get(self.cursor).cloned()
    }

    fn refresh(&mut self) {
        self.state = ViewState::Loading;
        match run_list() {
            ListOutcome::Err(e) => {
                self.state = ViewState::Error(e);
                self.status = "list failed — R to retry".to_string();
            }
            ListOutcome::Ok { payload } => {
                self.items = payload.items.unwrap_or_default();
                self.generated_at = payload.generated_at;
                self.state = ViewState::Ready;
                self.clamp_cursor();
                let mut bits: Vec<String> = Vec::new();
                if let Some(c) = payload.counts {
                    bits.push(format!("{} inbox", c.inbox));
                    bits.push(format!("{} attention", c.attention));
                }
                self.status = if bits.is_empty() {
                    String::new()
                } else {
                    bits.join(" · ")
                };
            }
        }
    }

    fn show_detail(&mut self, item: &TuiItem) {
        let lines = item_detail_lines(item, self.generated_at.as_deref());
        self.screen = Screen::Detail {
            title: format!("Inbox item — {}", item.pane_id),
            lines,
        };
    }

    // --- verbs ---------------------------------------------------------------------

    fn verb(&mut self, key: &str) {
        match key {
            "t" => self.show_transitions(),
            "c" => self.mutate_reconcile(),
            _ => {
                let Some(item) = self.current() else { return };
                match key {
                    "s" => self.mutate_organize(
                        "settle",
                        &item,
                        "Settle pane?",
                        "Hides the pane from the inbox until its agent state changes. Reversible via restore; attention signals are never hidden.",
                        false,
                    ),
                    "n" => {
                        self.screen = Screen::Prompt {
                            kind: PromptKind::Snooze { pane_id: item.pane_id.clone() },
                            draft: "+30m".to_string(),
                            placeholder: "+30m".to_string(),
                            label: "Snooze until — +30m / +2h / +1d or ISO-8601".to_string(),
                        };
                    }
                    "r" => self.mutate_organize(
                        "restore",
                        &item,
                        "Restore pane?",
                        "Clears all organize state (settled/snoozed/archived/pin/order) for this pane. Declared PR/Issue links are kept.",
                        false,
                    ),
                    "p" => {
                        let cmd = if item.pinned { "unpin" } else { "pin" };
                        self.mutate_organize(
                            cmd,
                            &item,
                            if item.pinned { "Unpin pane?" } else { "Pin pane?" },
                            if item.pinned {
                                "Removes the pin; the pane returns to normal ordering."
                            } else {
                                "Pins the pane to the top of the inbox. Layout hint only — never hides the pane."
                            },
                            false,
                        );
                    }
                    "a" => {
                        let archived = item.organize.as_ref().map(|o| o.kind == "archived").unwrap_or(false);
                        let cmd = if archived { "unarchive" } else { "archive" };
                        self.mutate_organize(
                            cmd,
                            &item,
                            if archived { "Unarchive pane?" } else { "Archive pane?" },
                            "Archive hides the pane from the default inbox view (like settle) but is cleared by the same generation rules. It never stops or closes the agent.",
                            false,
                        );
                    }
                    "[" | "]" => self.mutate_order(if key == "[" { OrderDir::Up } else { OrderDir::Down }, &item),
                    "l" => {
                        self.screen = Screen::Prompt {
                            kind: PromptKind::Link { pane_id: item.pane_id.clone() },
                            draft: String::new(),
                            placeholder: "pr owner/repo#123".to_string(),
                            label: "Link tracker — pr <owner>/<repo>#<n>  or  issue <owner>/<repo>#<n>".to_string(),
                        };
                    }
                    "u" => {
                        if item.links.is_empty() {
                            self.status = format!("{} has no declared links", item.pane_id);
                            return;
                        }
                        let hints: Vec<String> = item.links.iter().map(|l| l.ref_.clone()).collect();
                        let placeholder = hints.first().cloned().unwrap_or_default();
                        let label = format!("Unlink — {}", hints.join(", "));
                        self.screen = Screen::Prompt {
                            kind: PromptKind::Unlink { pane_id: item.pane_id.clone(), hints },
                            draft: String::new(),
                            placeholder,
                            label,
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    /// Fresh read + stale check + confirm-gate. Returns the mutation to
    /// confirm, or reports the failure in the status line.
    fn build_mutation(
        &mut self,
        item: &TuiItem,
        title: &str,
        consequence: &str,
        danger: bool,
        args: Vec<String>,
        ok_message: String,
    ) {
        // 1. Re-read before mutating: stale generation ⇒ abort + refresh.
        let fresh = run_list();
        let fresh_items: Vec<TuiItem> = match &fresh {
            ListOutcome::Ok { payload } => payload.items.clone().unwrap_or_default(),
            ListOutcome::Err(e) => {
                self.status = format!("revalidate failed: {}", e);
                return;
            }
        };
        let after = fresh_items.iter().find(|i| i.pane_id == item.pane_id);
        let verdict = stale_check(Some(item), after);
        match verdict {
            StaleVerdict::Ok => {}
            StaleVerdict::Gone { reason } => {
                self.stale_abort(fresh_items, &item.pane_id, "stale_pane", &reason);
                return;
            }
            StaleVerdict::Stale { reason } => {
                self.stale_abort(fresh_items, &item.pane_id, "stale_generation", &reason);
                return;
            }
        }
        // 2. Explicit confirmation with the exact target (contract item 5).
        let mut target_lines = vec![format!("pane: {}", item.pane_id)];
        if let Some(t) = &item.title {
            target_lines.push(format!("title: {}", t));
        }
        if let Some(c) = &item.cwd {
            target_lines.push(format!("cwd: {}", c));
        }
        target_lines.push(format!("cli: inbox {}", args.join(" ")));
        self.screen = Screen::Confirm {
            mutation: PendingMutation {
                title: title.to_string(),
                target_lines,
                consequence: consequence.to_string(),
                danger,
                args,
                ok_message,
            },
        };
    }

    /// Abort a mutation on a stale/gone generation: refresh the list and
    /// report honestly (contract §3.3).
    fn stale_abort(&mut self, fresh_items: Vec<TuiItem>, pane_id: &str, kind: &str, reason: &str) {
        self.items = fresh_items;
        self.state = ViewState::Ready;
        self.clamp_cursor();
        self.status = format!("{}: {} — list refreshed", kind, reason);
        let _ = pane_id;
    }

    fn mutate_organize(
        &mut self,
        command: &str,
        item: &TuiItem,
        title: &str,
        consequence: &str,
        danger: bool,
    ) {
        let args = vec![command.to_string(), item.pane_id.clone()];
        self.build_mutation(
            item,
            title,
            consequence,
            danger,
            args.clone(),
            format!("{} {}", command, item.pane_id),
        );
        let _ = args;
    }

    fn run_confirmed(&mut self, mutation: PendingMutation) {
        self.status = format!(
            "running: {}…",
            mutation.args.first().map(|s| s.as_str()).unwrap_or("?")
        );
        let result = run_domain(&mutation.args);
        self.screen = Screen::Main;
        if result.status != 0 {
            self.status = format!(
                "error: {}",
                cli_error_message(result.status, &result.stdout, &result.stderr)
            );
            return;
        }
        let mut warning_note = String::new();
        let mut summary_override: Option<String> = None;
        if let Ok(doc) = serde_json::from_str::<Value>(&result.stdout) {
            if let Some(w) = doc.get("warnings").and_then(|v| v.as_array()) {
                if !w.is_empty() {
                    warning_note = format!(
                        " ({} warning{})",
                        w.len(),
                        if w.len() == 1 { "" } else { "s" }
                    );
                }
            }
            // Session-scoped reconcile reports a count summary.
            if mutation.args == ["reconcile"] {
                let count = |k: &str| {
                    doc.get(k)
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0)
                };
                summary_override = Some(format!(
                    "reconcile: cleared={} pruned={} projected={}",
                    count("cleared"),
                    count("pruned"),
                    count("projected")
                ));
            }
        }
        self.refresh();
        let base = summary_override.unwrap_or_else(|| mutation.ok_message.clone());
        self.status = format!("{}{}", base, warning_note);
    }

    fn mutate_order(&mut self, dir: OrderDir, item: &TuiItem) {
        let fresh = run_list();
        let fresh_items: Vec<TuiItem> = match &fresh {
            ListOutcome::Ok { payload } => payload.items.clone().unwrap_or_default(),
            ListOutcome::Err(e) => {
                self.status = format!("revalidate failed: {}", e);
                return;
            }
        };
        match order_move_target(&fresh_items, &item.pane_id, dir) {
            tui_model::OrderMove::Err(e) => {
                self.status = e;
            }
            tui_model::OrderMove::Ok { flag, ref_pane_id } => {
                let Some(shown) = fresh_items
                    .iter()
                    .find(|i| i.pane_id == item.pane_id)
                    .cloned()
                else {
                    self.status =
                        format!("stale_pane: {} is no longer in the session", item.pane_id);
                    return;
                };
                let dir_name = match dir {
                    OrderDir::Up => "up",
                    OrderDir::Down => "down",
                };
                self.build_mutation(
                    &shown,
                    &format!("Move pane {}?", dir_name),
                    &format!(
                        "Rewrites manual order ranks so {} moves {} within its pinned/attention group. Layout hint only.",
                        item.pane_id, dir_name
                    ),
                    false,
                    vec!["order".to_string(), item.pane_id.clone(), flag.to_string(), ref_pane_id],
                    format!("moved {} {}", item.pane_id, dir_name),
                );
            }
        }
    }

    fn prompt_submit(&mut self) {
        let screen = std::mem::replace(&mut self.screen, Screen::Main);
        let Screen::Prompt { kind, draft, .. } = screen else {
            return;
        };
        match kind {
            PromptKind::Snooze { pane_id } => match validate_snooze_spec(&draft) {
                tui_model::SnoozeSpec::Err(e) => {
                    self.status = format!("invalid snooze spec: {}", e);
                }
                tui_model::SnoozeSpec::Ok(spec) => {
                    if let Some(item) = self.items.iter().find(|i| i.pane_id == pane_id).cloned() {
                        let ok_message = format!("snoozed {} until {}", pane_id, spec);
                        self.build_mutation(
                                &item,
                                "Snooze pane?",
                                &format!("Hides the pane until {}, then it returns automatically. Reversible via restore.", spec),
                                false,
                                vec!["snooze".to_string(), pane_id, "--until".to_string(), spec],
                                ok_message,
                            );
                    }
                }
            },
            PromptKind::Link { pane_id } => {
                if draft.trim().is_empty() {
                    self.status = "cancelled".to_string();
                    return;
                }
                match parse_link_spec(&draft) {
                    tui_model::LinkSpec::Err(e) => {
                        self.status = format!("invalid link: {}", e);
                    }
                    tui_model::LinkSpec::Ok { kind, flag, ref_ } => {
                        if let Some(item) =
                            self.items.iter().find(|i| i.pane_id == pane_id).cloned()
                        {
                            let ok_message = format!("linked {} → {}", ref_, pane_id);
                            self.build_mutation(
                                &item,
                                &format!("Link {}?", kind.to_uppercase()),
                                &format!(
                                    "Declares {} as a user-owned {} reference on {}. Verified via gh when available; recorded either way.",
                                    ref_, kind, pane_id
                                ),
                                false,
                                vec!["link".to_string(), pane_id, flag.to_string(), ref_, "--verify".to_string()],
                                ok_message,
                            );
                        }
                    }
                }
            }
            PromptKind::Unlink { pane_id, hints } => {
                if draft.trim().is_empty() {
                    self.status = "cancelled".to_string();
                    return;
                }
                let mut spec = parse_link_spec(&draft);
                if matches!(spec, tui_model::LinkSpec::Err(_)) {
                    let bare = draft.trim().to_string();
                    if let Some(existing) = self
                        .items
                        .iter()
                        .find(|i| i.pane_id == pane_id)
                        .and_then(|i| i.links.iter().find(|l| l.ref_ == bare))
                        .cloned()
                    {
                        spec = tui_model::LinkSpec::Ok {
                            kind: match existing.kind.as_str() {
                                "issue" => "issue",
                                _ => "pr",
                            },
                            flag: if existing.kind == "issue" {
                                "--issue"
                            } else {
                                "--pr"
                            },
                            ref_: existing.ref_.clone(),
                        };
                    }
                }
                match spec {
                    tui_model::LinkSpec::Err(e) => {
                        self.status = format!("invalid unlink: {}", e);
                    }
                    tui_model::LinkSpec::Ok { kind, flag, ref_ } => {
                        let _ = hints;
                        if let Some(item) =
                            self.items.iter().find(|i| i.pane_id == pane_id).cloned()
                        {
                            let ok_message = format!("unlinked {} from {}", ref_, pane_id);
                            self.build_mutation(
                                &item,
                                "Remove link?",
                                &format!(
                                    "Removes the declared {} link {} from {}. The pane's organize state is unaffected.",
                                    kind, ref_, pane_id
                                ),
                                false,
                                vec!["unlink".to_string(), pane_id, flag.to_string(), ref_],
                                ok_message,
                            );
                        }
                    }
                }
            }
        }
    }

    fn show_transitions(&mut self) {
        self.status = "reading transitions…".to_string();
        let result = run_domain(&[
            "transitions".to_string(),
            "--limit".to_string(),
            "50".to_string(),
        ]);
        if result.status != 0 {
            self.status = format!(
                "transitions failed: {}",
                cli_error_message(result.status, &result.stdout, &result.stderr)
            );
            return;
        }
        let mut lines: Vec<String> = Vec::new();
        match serde_json::from_str::<Value>(&result.stdout) {
            Ok(doc) => {
                let head = doc
                    .get("head_seq")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let oldest = doc
                    .get("oldest_seq")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let dropped = doc
                    .get("dropped")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "0".to_string());
                lines.push(format!(
                    "head_seq={} oldest={} dropped={}",
                    head, oldest, dropped
                ));
                lines.push(String::new());
                let items = doc
                    .get("transitions")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if items.is_empty() {
                    lines.push("(no transitions recorded)".to_string());
                }
                for t in &items {
                    let seq = t
                        .get("seq")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let kind = t.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                    let pane = t.get("pane_id").and_then(|v| v.as_str()).unwrap_or("?");
                    let at = t.get("at").and_then(|v| v.as_str()).unwrap_or("?");
                    let cause = t
                        .get("cause")
                        .and_then(|v| v.as_str())
                        .map(|c| format!(" ({})", c))
                        .unwrap_or_default();
                    let ws = t
                        .get("workspace_id")
                        .and_then(|v| v.as_str())
                        .map(|w| format!(" ws={}", w))
                        .unwrap_or_default();
                    lines.push(format!(
                        "#{} {} {} @ {}{}{}",
                        seq, kind, pane, at, cause, ws
                    ));
                }
            }
            Err(_) => lines.push("could not parse transitions output".to_string()),
        }
        self.screen = Screen::Transitions { lines };
        self.status = String::new();
    }

    /// Explicit reconcile — the ONLY mutation that is not pane-scoped.
    fn mutate_reconcile(&mut self) {
        self.screen = Screen::Confirm {
            mutation: PendingMutation {
                title: "Run reconcile now?".to_string(),
                target_lines: vec![
                    format!("session: {}", self.session_label()),
                    "cli: inbox reconcile".to_string(),
                ],
                consequence: "Resolves expired snoozes and stale generation marks, prunes absent-pane entries, and re-projects pane metadata tokens. This is an explicit mutation — it never runs on open.".to_string(),
                danger: false,
                args: vec!["reconcile".to_string()],
                ok_message: "reconcile".to_string(),
            },
        };
    }

    // --- key handling ----------------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match &mut self.screen {
            Screen::Main => self.handle_main_key(key),
            Screen::Detail { .. } | Screen::Help | Screen::Transitions { .. } => {
                self.handle_overlay_key(key)
            }
            Screen::Prompt { .. } => self.handle_prompt_key(key),
            Screen::Confirm { .. } => self.handle_confirm_key(key),
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                self.screen = Screen::Main;
                true
            }
            _ => false,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Main;
                self.status = "cancelled".to_string();
                true
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let screen = std::mem::replace(&mut self.screen, Screen::Main);
                if let Screen::Confirm { mutation } = screen {
                    // Reconcile gets a summary suffix; pane mutations get
                    // refresh + ok message (run_confirmed handles both).
                    self.run_confirmed(mutation);
                }
                true
            }
            _ => true, // swallow other keys while confirming
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> bool {
        let Screen::Prompt { draft, .. } = &mut self.screen else {
            return true;
        };
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Main;
                true
            }
            KeyCode::Enter => {
                self.prompt_submit();
                true
            }
            KeyCode::Backspace => {
                draft.pop();
                true
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.push(c);
                true
            }
            _ => true,
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> bool {
        // Filter draft mode.
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter_mode = false;
                    self.filter_draft.clear();
                }
                KeyCode::Enter => {
                    self.query = self.filter_draft.clone();
                    self.filter_mode = false;
                    self.cursor = 0;
                    self.scroll_top = 0;
                    self.clamp_cursor();
                }
                KeyCode::Backspace => {
                    self.filter_draft.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter_draft.push(c);
                }
                _ => {}
            }
            return true;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        let list_len = self.filtered().len();
        let move_by = |app: &mut App, delta: isize| {
            if list_len == 0 {
                return;
            }
            let n = list_len as isize;
            app.cursor = ((app.cursor as isize + delta).rem_euclid(n)) as usize;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => move_by(self, -1),
            KeyCode::Down | KeyCode::Char('j') => move_by(self, 1),
            KeyCode::PageUp => {
                for _ in 0..10 {
                    move_by(self, -1);
                }
            }
            KeyCode::PageDown => {
                for _ in 0..10 {
                    move_by(self, 1);
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = list_len.saturating_sub(1),
            KeyCode::Char('/') => {
                self.filter_mode = true;
                self.filter_draft = self.query.clone();
            }
            KeyCode::Char('f') => {
                self.scope = next_scope(self.scope);
                self.cursor = 0;
                self.scroll_top = 0;
                self.clamp_cursor();
            }
            KeyCode::Char('R') => self.refresh(),
            KeyCode::Enter => {
                if let Some(item) = self.current() {
                    self.show_detail(&item);
                }
            }
            KeyCode::Char('?') => self.screen = Screen::Help,
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char(c) => {
                if matches!(self.state, ViewState::Ready) {
                    if c == 't' || c == 'c' {
                        self.verb(&c.to_string());
                        return true;
                    }
                    if self.current().is_some()
                        && matches!(c, 's' | 'n' | 'r' | 'p' | 'a' | '[' | ']' | 'l' | 'u')
                    {
                        self.verb(&c.to_string());
                        return true;
                    }
                }
            }
            _ => {}
        }
        true
    }
}

// --- rendering -------------------------------------------------------------------------

fn render(app: &mut App, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Header — session label and query are untrusted/env-derived.
    let sess = sanitize_display(&app.session_label());
    let counts = match &app.state {
        ViewState::Ready => {
            let vis = app.filtered().len();
            format!("{}/{}", vis, app.items.len())
        }
        _ => String::new(),
    };
    let query = sanitize_display(&app.query);
    let query_part = if query.is_empty() {
        String::new()
    } else {
        format!(" · /{}", query)
    };
    let header = format!(
        " Inbox — {} · scope:{}{} {}",
        sess,
        scope_label(app.scope),
        query_part,
        counts
    );
    frame.render_widget(
        Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    // Body
    match &mut app.screen {
        Screen::Main => render_main(app, frame, chunks[1]),
        Screen::Detail { title, lines } => {
            let text = lines.join("\n"); // lines already sanitized in tui_model
            frame.render_widget(
                Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(sanitize_display(title)),
                    )
                    .wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
        Screen::Prompt {
            kind: _,
            draft,
            placeholder,
            label,
        } => {
            let mut lines = vec![
                Line::from(sanitize_display(label))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(format!("> {}█", sanitize_display(draft))),
                Line::from(Span::styled(
                    format!(
                        "(enter submit · esc cancel · placeholder: {})",
                        sanitize_display(placeholder)
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
            if let Screen::Prompt {
                kind: PromptKind::Unlink { hints, .. },
                ..
            } = &app.screen
            {
                if !hints.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("existing links: {}", sanitize_display(&hints.join(", "))),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
            }
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("inbox")),
                chunks[1],
            );
        }
        Screen::Confirm { mutation } => {
            let mut text = vec![
                Line::from(sanitize_display(&mutation.title))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(""),
            ];
            for l in &mutation.target_lines {
                text.push(Line::from(sanitize_display(l)));
            }
            text.push(Line::from(""));
            text.push(Line::from(sanitize_display(&mutation.consequence)));
            text.push(Line::from(Span::styled(
                "enter/y apply · esc cancel",
                Style::default().add_modifier(Modifier::DIM),
            )));
            let _ = danger_style(mutation.danger);
            frame.render_widget(
                Clear,
                Rect {
                    x: chunks[1].x + chunks[1].width / 4,
                    y: chunks[1].y + chunks[1].height / 4,
                    width: chunks[1].width / 2,
                    height: chunks[1].height / 2,
                },
            );
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("confirm")),
                Rect {
                    x: chunks[1].x + chunks[1].width / 4,
                    y: chunks[1].y + chunks[1].height / 4,
                    width: chunks[1].width / 2,
                    height: chunks[1].height / 2,
                },
            );
        }
        Screen::Help => {
            let mut text: Vec<Line> = Vec::new();
            text.push(
                Line::from("Inbox TUI keymap").style(Style::default().add_modifier(Modifier::BOLD)),
            );
            text.push(Line::from(""));
            for kb in tui_model::INBOX_TUI_KEYMAP {
                text.push(Line::from(format!(
                    "  {:<18} {}",
                    kb.keys.join("/"),
                    kb.description
                )));
            }
            text.push(Line::from(""));
            text.push(Line::from("Mutations go through the inbox CLI contract — the TUI never writes organize state directly."));
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("help")),
                chunks[1],
            );
        }
        Screen::Transitions { lines } => {
            // Transition fields are state-derived — bound + sanitize each
            // line (bounded output, tui-v1 §5 item 10).
            let safe: Vec<String> = lines
                .iter()
                .map(|l| bound_text(&sanitize_display(l), 500))
                .collect();
            frame.render_widget(
                Paragraph::new(safe.join("\n"))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Organize-transition log (read-only)"),
                    )
                    .wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
    }

    // Footer — status embeds pane ids/specs; the filter draft is raw
    // user input. Both are sanitized before display.
    let hint = if app.filter_mode {
        format!(
            " filter: /{}█ — enter apply · esc cancel",
            sanitize_display(&app.filter_draft)
        )
    } else {
        format!(" {} · ? help · q quit", sanitize_display(&app.status))
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn danger_style(danger: bool) -> Style {
    if danger {
        Style::default().fg(ratatui::style::Color::Red)
    } else {
        Style::default()
    }
}

fn render_main(app: &mut App, frame: &mut ratatui::Frame, area: Rect) {
    match &app.state {
        ViewState::Loading => {
            frame.render_widget(Paragraph::new("  loading…"), area);
        }
        ViewState::Error(e) => {
            let text = vec![
                Line::from(Span::styled(
                    "  error: ",
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(ratatui::style::Color::Red),
                )),
                Line::from(format!("  {}", e)),
                Line::from("  R — retry · q — quit"),
            ];
            frame.render_widget(Paragraph::new(text), area);
        }
        ViewState::Ready => {
            let list = app.filtered();
            if list.is_empty() {
                let msg = if app.items.is_empty() {
                    "  no agent panes in this session — open one, then R to refresh"
                } else {
                    "  no items match the current scope/filter"
                };
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(
                            "  (empty)",
                            Style::default().add_modifier(Modifier::DIM),
                        )),
                        Line::from(msg),
                    ]),
                    area,
                );
                return;
            }
            // Scroll window: reserve 1 line for the position indicator.
            let visible = (area.height as usize).saturating_sub(1).max(1);
            let n = list.len();
            if app.cursor < app.scroll_top {
                app.scroll_top = app.cursor;
            }
            if app.cursor >= app.scroll_top + visible {
                app.scroll_top = app.cursor + 1 - visible;
            }
            app.scroll_top = app.scroll_top.min(n.saturating_sub(visible));
            let mut lines: Vec<Line> = Vec::new();
            for i in 0..visible.min(n.saturating_sub(app.scroll_top)) {
                let item = &list[app.scroll_top + i];
                let is_sel = app.scroll_top + i == app.cursor;
                let badges = item_badges(item);
                let badge_text = if badges.is_empty() {
                    String::new()
                } else {
                    format!("[{}] ", badges)
                };
                let title = item_title(item);
                let status_text = item
                    .agent_status
                    .clone()
                    .map(|s| sanitize_display(&s))
                    .unwrap_or_default();
                let right = if status_text.is_empty() {
                    String::new()
                } else {
                    format!(" {}", status_text)
                };
                let pane_label = sanitize_display(&item.pane_id);
                let prefix = if is_sel { "❯ " } else { "  " };
                let sel_style = if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let used = prefix.chars().count()
                    + badge_text.chars().count()
                    + title.chars().count()
                    + right.chars().count()
                    + 2;
                let mut spans = vec![
                    Span::styled(prefix, sel_style),
                    Span::styled(badge_text, Style::default().add_modifier(Modifier::DIM)),
                    Span::styled(title, sel_style),
                ];
                let w = area.width as usize;
                if used < w {
                    let pad = w.saturating_sub(used);
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(
                        format!("{}{}", pane_label, right),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                } else {
                    spans.push(Span::styled(
                        format!(" {}", pane_label),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                lines.push(Line::from(spans));
            }
            if n > visible {
                lines.push(Line::from(Span::styled(
                    format!("  {}/{}", app.cursor + 1, n),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            frame.render_widget(Paragraph::new(lines), area);
        }
    }
}

// --- non-TTY fallback -------------------------------------------------------------------

/// Honest plain-text rendering when there is no TTY (pipes, CI).
pub fn plain_list(payload: &ListPayload) -> Vec<String> {
    let counts = payload.counts.clone().unwrap_or_default();
    let session_label = sanitize_display(
        &payload
            .session
            .as_ref()
            .and_then(|s| s.name.clone().or_else(|| s.socket.clone()))
            .unwrap_or_else(|| "?".to_string()),
    );
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "inbox — session={} total={} inbox={} attention={} organized={} pinned={} archived={}",
        session_label,
        counts.total,
        counts.inbox,
        counts.attention,
        counts.organized,
        counts.pinned,
        counts.archived
    ));
    for item in payload.items.iter().flatten() {
        let badges = item_badges(item);
        let status = item
            .agent_status
            .as_ref()
            .map(|s| sanitize_display(&format!(" [{}]", s)))
            .unwrap_or_default();
        let hidden = if item.in_inbox {
            String::new()
        } else {
            " (hidden)".to_string()
        };
        let title = sanitize_display(item.title.as_deref().unwrap_or(""));
        let badge_part = if badges.is_empty() {
            String::new()
        } else {
            format!(" <{}>", badges)
        };
        let links_part = if item.links.is_empty() {
            String::new()
        } else {
            let refs: Vec<String> = item
                .links
                .iter()
                .map(|l| sanitize_display(&l.ref_))
                .collect();
            format!(" links={}", refs.join(","))
        };
        lines.push(format!(
            "  {}{}{} {}{}{}",
            sanitize_display(&item.pane_id),
            status,
            hidden,
            title,
            badge_part,
            links_part
        ));
    }
    lines.push("interactive UI requires a TTY; use the CLI for scripting.".to_string());
    lines
}

fn plain_mode() -> i32 {
    match run_list() {
        ListOutcome::Err(e) => {
            let _ = writeln!(
                io::stderr(),
                "{}",
                serde_json::json!({"ok": false, "error": e})
            );
            1
        }
        ListOutcome::Ok { payload } => {
            for line in plain_list(&payload) {
                let _ = writeln!(io::stdout(), "{}", line);
            }
            0
        }
    }
}

// --- `open` action helper -----------------------------------------------------------------

/// `open` action: capture the invocation origin once, then ask herdr to
/// open the `main` pane entrypoint as a tab and return promptly.
fn run_open_action() -> i32 {
    let bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let origin_json = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default();
    let workspace_id = serde_json::from_str::<Value>(&origin_json)
        .ok()
        .and_then(|v| {
            v.get("workspace_id")
                .and_then(|w| w.as_str())
                .map(|s| s.to_string())
        });

    let mut argv: Vec<String> = vec![
        "plugin".into(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        PLUGIN_ID.into(),
        "--entrypoint".into(),
        ENTRYPOINT_ID.into(),
        "--placement".into(),
        "tab".into(),
    ];
    if let Some(ws) = &workspace_id {
        argv.push("--workspace".into());
        argv.push(ws.clone());
    }
    if !origin_json.is_empty() {
        argv.push("--env".into());
        argv.push(format!("HERDR_TUI_CONTEXT_JSON={}", origin_json));
    }

    let result = Command::new(&bin)
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match result {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stdout.trim().is_empty() {
                print!("{}", stdout);
                let _ = io::stdout().flush();
            }
            if !out.status.success() {
                let code = out.status.code().unwrap_or(-1);
                if !stderr.trim().is_empty() {
                    eprintln!("{}", stderr.trim_end());
                } else {
                    eprintln!("plugin pane open failed ({})", code);
                }
                return if code == -1 { 1 } else { code };
            }
            0
        }
        Err(err) => {
            eprintln!("{}", err);
            1
        }
    }
}

// --- entry ----------------------------------------------------------------------------

const USAGE: &str = r#"rebuildup.inbox TUI (herdr-plugins.tui/v1)

Usage:
  inbox tui          interactive inbox browser (pane entrypoint)
  inbox tui --open   open action helper (plugin.pane.open)

Keys: see '?' inside the UI. Non-interactive fallback prints a plain list.
"#;

/// `inbox tui [--open]` entrypoint; returns the process exit code.
pub fn run(args: &[String]) -> ! {
    let code = tui_main(args);
    std::process::exit(code);
}

fn tui_main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--open") {
        return run_open_action();
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", USAGE);
        return 0;
    }

    let ctx = capture_context();

    // Non-TTY stdin → honest plain-text list.
    if !io::stdin().is_terminal() {
        return plain_mode();
    }

    let mut app = App::new(ctx);
    app.refresh();

    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("inbox tui failed: could not enable raw mode");
        return 1;
    }
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            // Raw mode and the alternate screen are already active —
            // restore the terminal before reporting the failure.
            let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
            crossterm::terminal::disable_raw_mode().ok();
            eprintln!("inbox tui failed: {}", err);
            return 1;
        }
    };
    // NOTE: no terminal.clear() here — ratatui-core's clear() queries the
    // cursor position (ESC[6n) and blocks ~2s on terminals that never
    // answer DSR (dumb PTYs, minimal emulators). EnterAlternateScreen
    // already gives a blank screen, and the first draw repaints fully.

    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| render(&mut app, frame))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == event::KeyEventKind::Press && !app.handle_key(key) {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();

    let _ = terminal.show_cursor();
    crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen).ok();
    crossterm::terminal::disable_raw_mode().ok();

    match result {
        Ok(()) => {
            let _ = writeln!(io::stderr(), "inbox TUI closed");
            0
        }
        Err(err) => {
            eprintln!("inbox tui failed: {}", err);
            1
        }
    }
}
