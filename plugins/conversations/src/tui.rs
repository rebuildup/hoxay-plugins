//! rebuildup.conversations interactive TUI (contract `herdr-plugins.tui/v1`,
//! ADR-0031/0032/0033, program #228 ticket #244).
//! Rust port of `plugins/conversations/src/tui.ts` (retired; git history
//! preserves it).
//!
//! Surfaces the read contract (`sessions` / `history` / `search` /
//! `show`) as a browsable terminal UI plus an explicit, confirm-gated
//! `index` re-run. All data flows through this plugin's own domain
//! command cores (`cli::run_domain`) — the same public surface agents
//! and external clients use; the TUI never reads state files or PTY
//! text directly and never respawns itself.
//!
//! Safety contract highlights:
//! - opening performs reads only; `index` runs only via `i` + confirm
//!   (mutation_commands gating from `capability`)
//! - untrusted transcript text is rendered through hoxay-tui's
//!   sanitizer (no CSI/OSC injection)
//! - honest empty/missing/unavailable states; non-TTY gets a plain notice
//!
//! Entrypoints (herdr-plugin.toml):
//!   `conversations tui main`   — pane entrypoint (the UI)
//!   `conversations tui --open` — action: opens the `main` pane via
//!                                `plugin.pane.open` (HERDR_BIN_PATH)
//!                                and returns promptly

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

// Shared safety helpers (ADR-0036): every untrusted string that can
// reach the renderer goes through `sanitize_display` first.
use hoxay_tui::safety::{bound_text, sanitize_display};

const PLUGIN_ID: &str = "rebuildup.conversations";
const ENTRYPOINT_ID: &str = "main";
const HERDR_BIN: &str = "herdr";

// --- CLI bridge -----------------------------------------------------------------

/// Result of one in-process domain invocation (mirrors TS `CliResult`).
struct CliResult {
    ok: bool,
    data: Option<Value>,
    error: Option<String>,
}

/// Run one domain command in-process — the same `cmd_*` cores the CLI
/// dispatches to. No self-respawn: reads share this process's view of
/// state and the pinned herdr session/socket targeting from the pane
/// environment. `index` is reachable only behind the confirm gate.
fn run_cli(args: &[String]) -> CliResult {
    match crate::cli::run_domain(args) {
        Ok(payload) => CliResult {
            ok: true,
            data: Some(payload),
            error: None,
        },
        Err(e) => CliResult {
            ok: false,
            data: None,
            error: Some(e.message),
        },
    }
}

// --- payload → view models -------------------------------------------------------

#[derive(Clone)]
struct SessionEntry {
    key: String,
    provider: String,
    session_id: String,
    status: String,
    error: Option<String>,
    stale: bool,
    #[allow(dead_code)] // parsed for parity with tui.ts; STALE badge suffices
    stale_reason: Option<String>,
    event_count: f64,
    turn_count: f64,
    last_event_at: Option<String>,
    cwd: Option<String>,
    transcript_present: bool,
}

fn str_field(v: Option<&Value>) -> Option<String> {
    v.and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn num_field(v: Option<&Value>) -> f64 {
    v.and_then(|n| n.as_f64()).unwrap_or(0.0)
}

/// `sessions` command payload → selectable entries (order preserved).
fn session_entries(payload: &Value) -> Vec<SessionEntry> {
    let mut out = Vec::new();
    let Some(list) = payload.get("sessions").and_then(|v| v.as_array()) else {
        return out;
    };
    for raw in list {
        let Some(s) = raw.as_object() else { continue };
        let get = |k: &str| s.get(k);
        let Some(key) = str_field(get("key")) else {
            continue;
        };
        let identity_cwd = get("identity")
            .and_then(|i| i.get("cwd"))
            .and_then(|c| c.as_str())
            .map(|c| c.to_string());
        out.push(SessionEntry {
            key,
            provider: str_field(get("provider")).unwrap_or_else(|| "unknown".to_string()),
            session_id: str_field(get("session_id"))
                .unwrap_or_else(|| str_field(get("key")).unwrap_or_default()),
            status: str_field(get("status")).unwrap_or_else(|| "unknown".to_string()),
            error: str_field(get("error")),
            stale: get("stale").and_then(|v| v.as_bool()) == Some(true),
            stale_reason: str_field(get("stale_reason")),
            event_count: num_field(get("event_count")),
            turn_count: num_field(get("turn_count")),
            last_event_at: str_field(get("last_event_at")),
            cwd: str_field(get("cwd")).or(identity_cwd),
            transcript_present: get("transcript_present")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        });
    }
    out
}

/// One row in a paged event browser (history or search result).
#[derive(Clone)]
struct EventRow {
    session_key: String,
    event: Value,
}

struct EventPage {
    rows: Vec<EventRow>,
    next_cursor: Option<String>,
    has_more: bool,
    /// Honest caveats (malformed lines, duplicates, selector notes).
    notes: Vec<String>,
}

fn event_from_json(raw: Option<&Value>) -> Option<Value> {
    raw.filter(|e| e.get("type").and_then(|t| t.as_str()).is_some())
        .cloned()
}

/// `history` command payload → EventPage.
fn history_page(payload: &Value) -> EventPage {
    let mut notes: Vec<String> = Vec::new();
    let page = payload.get("page").cloned().unwrap_or(Value::Null);
    let malformed = num_field(page.get("malformed_lines"));
    let dups = num_field(page.get("duplicates_skipped"));
    if malformed > 0.0 {
        notes.push(format!("{} malformed line(s) skipped", malformed as u64));
    }
    if dups > 0.0 {
        notes.push(format!("{} duplicate event(s) skipped", dups as u64));
    }
    if let Some(note) = str_field(payload.get("selector_note")) {
        notes.push(format!("note: {}", note));
    }
    if let Some(hint) = str_field(payload.get("hint")) {
        notes.push(hint);
    }
    let session_key =
        str_field(payload.get("session").and_then(|s| s.get("key"))).unwrap_or_default();
    if let Some(err) = str_field(payload.get("session").and_then(|s| s.get("error"))) {
        notes.push(format!("session: {}", err));
    }
    let rows = payload
        .get("events")
        .and_then(|v| v.as_array())
        .map(|events| {
            events
                .iter()
                .filter_map(|e| event_from_json(Some(e)))
                .map(|event| EventRow {
                    session_key: session_key.clone(),
                    event,
                })
                .collect()
        })
        .unwrap_or_default();
    EventPage {
        rows,
        next_cursor: str_field(page.get("next_cursor")),
        has_more: page.get("has_more").and_then(|v| v.as_bool()) == Some(true),
        notes,
    }
}

/// `search` command payload → EventPage (next_cursor is opaque).
fn search_page(payload: &Value) -> EventPage {
    let mut notes: Vec<String> = Vec::new();
    let page = payload.get("page").cloned().unwrap_or(Value::Null);
    let malformed = num_field(page.get("malformed_lines"));
    let dups = num_field(page.get("duplicates_skipped"));
    let restarted = page
        .get("restarted_sessions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if malformed > 0.0 {
        notes.push(format!("{} malformed line(s) skipped", malformed as u64));
    }
    if dups > 0.0 {
        notes.push(format!("{} duplicate event(s) skipped", dups as u64));
    }
    if restarted > 0 {
        notes.push(format!("{} session(s) restarted during scan", restarted));
    }
    if let Some(note) = str_field(payload.get("selector_note")) {
        notes.push(format!("note: {}", note));
    }
    if let Some(hint) = str_field(payload.get("hint")) {
        notes.push(hint);
    }
    let rows = payload
        .get("results")
        .and_then(|v| v.as_array())
        .map(|results| {
            results
                .iter()
                .filter_map(|r| {
                    let event = event_from_json(r.get("event"))?;
                    Some(EventRow {
                        session_key: str_field(r.get("session_key")).unwrap_or_default(),
                        event,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let has_more = page.get("has_more").and_then(|v| v.as_bool()) == Some(true);
    EventPage {
        rows,
        next_cursor: if has_more {
            str_field(page.get("next_cursor"))
        } else {
            None
        },
        has_more,
        notes,
    }
}

// --- formatting -------------------------------------------------------------------

/// `clip` equivalent: truncate to a display-cell budget, appending `…`.
fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn ts_short(ts: Option<&str>) -> String {
    let Some(ts) = ts else {
        return "----".to_string();
    };
    // ISO-8601 → "MM-DD HH:MM" (fixed-shape provider data).
    let b = ts.as_bytes();
    if b.len() >= 16
        && b[4] == b'-'
        && b[7] == b'-'
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
    {
        return format!(
            "{}-{} {}:{}",
            &ts[5..7],
            &ts[8..10],
            &ts[11..13],
            &ts[14..16]
        );
    }
    bound_text(ts, 16)
}

fn event_field_str<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(|v| v.as_str())
}

fn event_preview(event: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = event_field_str(event, "text") {
        if !t.is_empty() {
            parts.push(t.to_string());
        }
    }
    if let Some(t) = event_field_str(event, "tool_name") {
        parts.push(format!("[tool:{}]", t));
    }
    if let Some(t) = event_field_str(event, "subtype") {
        parts.push(format!("[{}]", t));
    }
    if let Some(t) = event_field_str(event, "model") {
        parts.push(format!("<{}>", t));
    }
    if event.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("(error)".to_string());
    }
    let joined = parts.join(" ");
    let joined: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        event_field_str(event, "event_id").unwrap_or("").to_string()
    } else {
        joined
    }
}

/// Detail lines for one event (read-only inspection; sanitized at render).
fn event_detail_lines(row: &EventRow) -> Vec<String> {
    let e = &row.event;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "type:        {}",
        event_field_str(e, "type").unwrap_or("")
    ));
    lines.push(format!(
        "event_id:    {}",
        event_field_str(e, "event_id").unwrap_or("")
    ));
    if !row.session_key.is_empty() {
        lines.push(format!("session:     {}", row.session_key));
    }
    if let Some(t) = e.get("turn_index").filter(|v| !v.is_null()) {
        lines.push(format!("turn:        {}", t));
    }
    let ts = event_field_str(e, "timestamp").unwrap_or("(none)");
    lines.push(format!("timestamp:   {}", ts));
    if e.get("sidechain").and_then(|v| v.as_bool()) == Some(true) {
        lines.push("sidechain:   true".to_string());
    }
    if let Some(m) = event_field_str(e, "model") {
        lines.push(format!("model:       {}", m));
    }
    if let Some(v) = event_field_str(e, "tool_use_id") {
        lines.push(format!("tool_use_id: {}", v));
    }
    if let Some(v) = event_field_str(e, "tool_name") {
        lines.push(format!("tool_name:   {}", v));
    }
    if let Some(v) = e.get("is_error").filter(|v| !v.is_null()) {
        lines.push(format!("is_error:    {}", v));
    }
    if let Some(v) = event_field_str(e, "subtype") {
        lines.push(format!("subtype:     {}", v));
    }
    if let Some(u) = e.get("usage").filter(|v| v.is_object()) {
        let g = |k: &str| num_field(u.get(k));
        let mut line = format!(
            "usage:       in={} out={} cache_read={} cache_write={}",
            g("input_tokens") as u64,
            g("output_tokens") as u64,
            g("cache_read_input_tokens") as u64,
            g("cache_creation_input_tokens") as u64,
        );
        if u.get("thinking_tokens").is_some() {
            line.push_str(&format!(" thinking={}", g("thinking_tokens") as u64));
        }
        lines.push(line);
    }
    if let Some(c) = e.get("cost_usd").filter(|v| !v.is_null()) {
        lines.push(format!("cost_usd:    {}", c));
    }
    if let Some(text) = event_field_str(e, "text") {
        if !text.is_empty() {
            lines.push(String::new());
            lines.push("text:".to_string());
            lines.push(bound_text(text, 8000));
            if e.get("text_truncated").and_then(|v| v.as_bool()) == Some(true) {
                lines.push("(truncated)".to_string());
            }
        }
    }
    if let Some(tool_input) = e.get("tool_input").filter(|v| !v.is_null()) {
        let rendered =
            serde_json::to_string_pretty(tool_input).unwrap_or_else(|_| tool_input.to_string());
        lines.push(String::new());
        lines.push("tool_input:".to_string());
        lines.push(bound_text(&rendered, 4000));
        if e.get("input_truncated").and_then(|v| v.as_bool()) == Some(true) {
            lines.push("(truncated)".to_string());
        }
    }
    lines
}

fn short_id(id: &str) -> String {
    if id.chars().count() > 12 {
        let prefix: String = id.chars().take(8).collect();
        format!("{}…", prefix)
    } else {
        id.to_string()
    }
}

// --- paged event browser ---------------------------------------------------------

/// Which domain command feeds this event browser.
enum Loader {
    /// `history --session-id KEY --limit 200 [--cursor C]`
    History { session_key: String },
    /// `search QUERY --limit 200 [--cursor C]`
    Search { query: String },
}

impl Loader {
    fn args(&self, cursor: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = match self {
            Loader::History { session_key } => vec![
                "history".to_string(),
                "--session-id".to_string(),
                session_key.clone(),
                "--limit".to_string(),
                "200".to_string(),
            ],
            Loader::Search { query } => vec![
                "search".to_string(),
                query.clone(),
                "--limit".to_string(),
                "200".to_string(),
            ],
        };
        if let Some(c) = cursor {
            args.push("--cursor".to_string());
            args.push(c.to_string());
        }
        args
    }
}

/// Scrollable, paged event list shared by `history` (per session) and
/// `search` (cross-session). Read-only; `n`/`p` page through opaque
/// cursors, `enter` opens a sanitized detail view.
struct EventList {
    title: String,
    empty_text: String,
    loader: Loader,
    /// Cursor stack: index 0 is the first page (no cursor).
    cursors: Vec<Option<String>>,
    pos: usize,
    rows: Vec<EventRow>,
    notes: Vec<String>,
    has_more: bool,
    next_cursor: Option<String>,
    error: Option<String>,
    scroll: usize,
}

impl EventList {
    fn new(title: String, empty_text: String, loader: Loader) -> Self {
        let mut list = EventList {
            title,
            empty_text,
            loader,
            cursors: vec![None],
            pos: 0,
            rows: Vec::new(),
            notes: Vec::new(),
            has_more: false,
            next_cursor: None,
            error: None,
            scroll: 0,
        };
        list.refresh();
        list
    }

    /// (Re)load the current page.
    fn refresh(&mut self) {
        self.error = None;
        let cursor = self.cursors.get(self.pos).cloned().flatten();
        let res = run_cli(&self.loader.args(cursor.as_deref()));
        match res {
            CliResult {
                ok: true,
                data: Some(payload),
                ..
            } => {
                let page = match &self.loader {
                    Loader::History { .. } => history_page(&payload),
                    Loader::Search { .. } => search_page(&payload),
                };
                self.rows = page.rows;
                self.notes = page.notes;
                self.has_more = page.has_more;
                self.next_cursor = page.next_cursor;
                self.scroll = 0;
            }
            other => {
                self.error = Some(other.error.unwrap_or_else(|| "load failed".to_string()));
                self.rows = Vec::new();
                self.notes = Vec::new();
                self.has_more = false;
                self.next_cursor = None;
            }
        }
    }

    fn goto(&mut self, delta: isize) {
        let next = self.pos as isize + delta;
        if next < 0 || next as usize >= self.cursors.len() {
            return;
        }
        self.pos = next as usize;
        self.refresh();
    }

    fn next_page(&mut self) {
        if !self.has_more {
            return;
        }
        let Some(cursor) = self.next_cursor.clone() else {
            return;
        };
        self.cursors.truncate(self.pos + 1);
        self.cursors.push(Some(cursor));
        self.pos += 1;
        self.refresh();
    }
}

// --- app state --------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum ViewState {
    Loading,
    Error(String),
    Ready,
}

enum Screen {
    /// Top-level session list.
    Sessions,
    /// Paged event browser (history or search).
    Events(EventList),
    /// Scrollable read-only detail.
    Detail {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    /// Search query input.
    Prompt {
        draft: String,
    },
    /// Confirm-gated `index` re-run.
    ConfirmIndex,
    Help,
}

struct App {
    screen: Screen,
    // sessions state
    entries: Vec<SessionEntry>,
    state: ViewState,
    cursor: usize,
    scroll_top: usize,
    filter_mode: bool,
    filter_draft: String,
    query: String,
    notice: String,
    caps_ok: bool,
    mutation_commands: HashSet<String>,
    caps_error: Option<String>,
}

impl App {
    fn new() -> Self {
        App {
            screen: Screen::Sessions,
            entries: Vec::new(),
            state: ViewState::Loading,
            cursor: 0,
            scroll_top: 0,
            filter_mode: false,
            filter_draft: String::new(),
            query: String::new(),
            notice: String::new(),
            caps_ok: false,
            mutation_commands: HashSet::new(),
            caps_error: None,
        }
    }

    fn filtered(&self) -> Vec<&SessionEntry> {
        let q = self.query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                if q.is_empty() {
                    return true;
                }
                let hay = format!(
                    "{}:{} {} {} {}",
                    e.provider,
                    e.session_id,
                    e.status,
                    e.cwd.clone().unwrap_or_default(),
                    e.key
                )
                .to_lowercase();
                hay.contains(&q)
            })
            .collect()
    }

    fn clamp_cursor(&mut self) {
        let n = self.filtered().len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }

    fn current(&self) -> Option<&SessionEntry> {
        self.filtered().get(self.cursor).copied()
    }

    /// Boot: capability gate + sessions load (reads only).
    fn boot(&mut self) {
        let caps = run_cli(&["capability".to_string()]);
        match caps {
            CliResult {
                ok: true,
                data: Some(data),
                ..
            } => {
                let cmds = data
                    .get("mutation_commands")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.as_str().map(|s| s.to_string()))
                            .collect::<HashSet<_>>()
                    })
                    .unwrap_or_default();
                self.caps_ok = true;
                self.mutation_commands = cmds;
                self.caps_error = None;
            }
            other => {
                self.caps_ok = false;
                self.caps_error = Some(
                    other
                        .error
                        .unwrap_or_else(|| "capability unavailable".to_string()),
                );
            }
        }
        self.reload();
    }

    fn reload(&mut self) {
        self.notice = String::new();
        let res = run_cli(&["sessions".to_string()]);
        match res {
            CliResult {
                ok: true,
                data: Some(data),
                ..
            } => {
                self.entries = session_entries(&data);
                self.state = ViewState::Ready;
                self.clamp_cursor();
            }
            other => {
                self.state =
                    ViewState::Error(other.error.unwrap_or_else(|| "sessions failed".to_string()));
            }
        }
    }

    fn open_session(&mut self, entry: &SessionEntry) {
        let key = entry.key.clone();
        self.screen = Screen::Events(EventList::new(
            format!("history — {}", key),
            format!("no events for {} (status: {})", key, entry.status),
            Loader::History { session_key: key },
        ));
    }

    fn search(&mut self) {
        self.screen = Screen::Prompt {
            draft: String::new(),
        };
    }

    fn submit_search(&mut self, query: String) {
        let q = query.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.screen = Screen::Events(EventList::new(
            format!("search — \"{}\"", q),
            format!("no matches for \"{}\"", q),
            Loader::Search { query: q },
        ));
    }

    /// `i` — capability-gated, confirm-gated `index` re-run.
    fn reindex(&mut self) {
        if !self.caps_ok {
            self.notice = format!(
                "reindex unavailable: capability check failed ({})",
                self.caps_error
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            );
            return;
        }
        if !self.mutation_commands.contains("index") {
            self.notice = "reindex unavailable: 'index' not in mutation_commands".to_string();
            return;
        }
        self.screen = Screen::ConfirmIndex;
    }

    fn run_index(&mut self) {
        self.notice = "indexing…".to_string();
        let res = run_cli(&["index".to_string()]);
        match res {
            CliResult {
                ok: true,
                data: Some(data),
                ..
            } => {
                let indexed = num_field(data.get("sessions_indexed")) as u64;
                let appended = num_field(data.get("events_appended")) as u64;
                let skipped = data
                    .get("skipped")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                self.notice = format!(
                    "indexed {} session(s), {} event(s) appended, {} skipped",
                    indexed, appended, skipped
                );
            }
            other => {
                self.notice = format!(
                    "index failed: {}",
                    other.error.unwrap_or_else(|| "unknown".to_string())
                );
            }
        }
        self.reload();
    }

    // --- key handling ---------------------------------------------------------

    /// Returns true when the key was consumed (app keeps running).
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != event::KeyEventKind::Press {
            return true;
        }
        if matches!(self.screen, Screen::Events(_)) {
            return self.handle_events_key(key);
        }
        match &mut self.screen {
            Screen::Sessions => self.handle_sessions_key(key),
            Screen::Events(_) => true, // handled before the match
            Screen::Detail { scroll, lines, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.screen = Screen::Sessions;
                    true
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = (*scroll + 1).min(lines.len().saturating_sub(1));
                    true
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    true
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    *scroll = (*scroll + 10).min(lines.len().saturating_sub(1));
                    true
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(10);
                    true
                }
                KeyCode::Home => {
                    *scroll = 0;
                    true
                }
                KeyCode::End => {
                    *scroll = lines.len().saturating_sub(1);
                    true
                }
                _ => false,
            },
            Screen::Prompt { draft } => match key.code {
                KeyCode::Esc => {
                    self.screen = Screen::Sessions;
                    true
                }
                KeyCode::Enter => {
                    let q = draft.clone();
                    self.screen = Screen::Sessions;
                    self.submit_search(q);
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
            },
            Screen::ConfirmIndex => match key.code {
                KeyCode::Esc => {
                    self.screen = Screen::Sessions;
                    self.notice = "cancelled".to_string();
                    true
                }
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.screen = Screen::Sessions;
                    self.run_index();
                    true
                }
                _ => true, // swallow other keys while confirming
            },
            Screen::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('?') => {
                    self.screen = Screen::Sessions;
                    true
                }
                _ => false,
            },
        }
    }

    fn handle_sessions_key(&mut self, key: KeyEvent) -> bool {
        // Filter draft mode gets first refusal.
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter_mode = false;
                    self.filter_draft.clear();
                }
                KeyCode::Enter => {
                    self.query = self.filter_draft.clone();
                    self.filter_mode = false;
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

        let list = self.filtered();
        let n = list.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => false, // quit
            KeyCode::Char('?') => {
                self.screen = Screen::Help;
                true
            }
            KeyCode::Char('/') => {
                self.filter_mode = true;
                self.filter_draft.clear();
                true
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if n > 0 {
                    self.cursor = (self.cursor + 1).min(n - 1);
                }
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                if n > 0 {
                    self.cursor = (self.cursor + 10).min(n - 1);
                }
                true
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(10);
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                if n > 0 {
                    self.cursor = n - 1;
                }
                true
            }
            KeyCode::Enter => {
                if let Some(entry) = self.current() {
                    let entry = entry.clone();
                    self.open_session(&entry);
                }
                true
            }
            KeyCode::Char('s') => {
                self.search();
                true
            }
            KeyCode::Char('i') => {
                self.reindex();
                true
            }
            KeyCode::Char('r') => {
                self.reload();
                true
            }
            _ => true, // swallow unbound keys on the main screen
        }
    }

    fn handle_events_key(&mut self, key: KeyEvent) -> bool {
        // What to do after releasing the `self.screen` borrow.
        enum Act {
            Back,
            Detail(EventRow),
            None,
        }
        let mut act = Act::None;
        {
            let Screen::Events(list) = &mut self.screen else {
                return true;
            };
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => act = Act::Back,
                KeyCode::Char('j') | KeyCode::Down => list.scroll += 1,
                KeyCode::Char('k') | KeyCode::Up => {
                    list.scroll = list.scroll.saturating_sub(1);
                }
                KeyCode::PageUp => list.scroll = list.scroll.saturating_sub(10),
                KeyCode::PageDown | KeyCode::Char(' ') => list.scroll += 10,
                KeyCode::Home => list.scroll = 0,
                KeyCode::End => list.scroll = usize::MAX / 2,
                KeyCode::Enter => {
                    if let Some(row) = list.rows.get(list.scroll) {
                        act = Act::Detail(row.clone());
                    }
                }
                KeyCode::Char('n') => list.next_page(),
                KeyCode::Char('p') => list.goto(-1),
                KeyCode::Char('r') => list.refresh(),
                _ => {}
            }
        }
        match act {
            Act::Back => self.screen = Screen::Sessions,
            Act::Detail(row) => {
                self.screen = Screen::Detail {
                    title: format!(
                        "{} · {}",
                        event_field_str(&row.event, "type").unwrap_or(""),
                        event_field_str(&row.event, "event_id").unwrap_or("")
                    ),
                    lines: event_detail_lines(&row),
                    scroll: 0,
                };
            }
            Act::None => {}
        }
        true
    }
}

// --- render -----------------------------------------------------------------------

const KEYMAP: &[(&str, &str)] = &[
    ("↑↓/jk", "move / scroll"),
    ("pgup/pgdn/space", "page"),
    ("enter", "open / detail"),
    ("/", "filter sessions"),
    ("s", "search all sessions"),
    ("i", "reindex (confirm)"),
    ("n/p", "next/prev page (events)"),
    ("r", "reload"),
    ("?", "help"),
    ("esc/q", "back / quit"),
];

fn render(app: &mut App, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Header — session-derived text is untrusted/env-derived.
    let title = match &app.screen {
        Screen::Events(list) => {
            format!(
                " {} — page {}/{}",
                list.title,
                list.pos + 1,
                list.cursors.len().max(1)
            )
        }
        _ => " rebuildup.conversations — sessions".to_string(),
    };
    frame.render_widget(
        Paragraph::new(sanitize_display(&title))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    match &mut app.screen {
        Screen::Sessions => render_sessions(app, frame, chunks[1]),
        Screen::Events(list) => render_events(list, frame, chunks[1]),
        Screen::Detail {
            title,
            lines,
            scroll,
        } => {
            // Clamp scroll into range, then render a window of lines.
            let max_scroll = lines.len().saturating_sub(1);
            *scroll = (*scroll).min(max_scroll);
            let height = chunks[1].height as usize;
            let start = (*scroll).saturating_sub(height.saturating_sub(1));
            let window: Vec<String> = lines
                .iter()
                .skip(start)
                .take(height)
                .map(|l| sanitize_display(l))
                .collect();
            frame.render_widget(
                Paragraph::new(window.join("\n"))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(sanitize_display(title)),
                    )
                    .wrap(Wrap { trim: false }),
                chunks[1],
            );
        }
        Screen::Prompt { draft } => {
            let lines = vec![
                Line::from("Search all indexed sessions")
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(format!("> {}█", sanitize_display(draft))),
                Line::from(Span::styled(
                    "substring over text/tool/model fields · enter submit · esc cancel",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("search")),
                chunks[1],
            );
        }
        Screen::ConfirmIndex => {
            let text = vec![
                Line::from("Reindex conversations")
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(""),
                Line::from("scope: every agent pane transcript visible to herdr in this session"),
                Line::from(
                    "writes: <stateDir>/events/*.jsonl + state.json (opencode logs are rewritten)",
                ),
                Line::from(""),
                Line::from(
                    "Re-scans provider transcripts and rewrites normalized event logs. \
                     This is the plugin's only mutation; it never touches PTY or agent state.",
                ),
                Line::from(Span::styled(
                    "enter/y apply · esc cancel",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
            let area = centered(chunks[1], 60, 50);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("confirm")),
                area,
            );
        }
        Screen::Help => {
            let mut lines: Vec<Line> = vec![
                Line::from("conversations TUI keymap")
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(""),
            ];
            for (keys, desc) in KEYMAP {
                lines.push(Line::from(format!("  {:<18} {}", keys, desc)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Reads go through the conversations CLI contract; `index` is the only mutation and is confirm-gated.",
            ));
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("help")),
                chunks[1],
            );
        }
    }

    // Footer.
    let hint = match &app.screen {
        Screen::Sessions if app.filter_mode => format!(
            " filter: /{}█ — enter apply · esc cancel",
            sanitize_display(&app.filter_draft)
        ),
        Screen::Events(_) => {
            " ↑↓/jk scroll · enter detail · n/p page · r reload · esc back".to_string()
        }
        Screen::Detail { .. } => " ↑↓/jk scroll · pgup/pgdn page · esc back".to_string(),
        Screen::Prompt { .. } => String::new(),
        Screen::ConfirmIndex => String::new(),
        Screen::Help => String::new(),
        Screen::Sessions => {
            let status = if app.notice.is_empty() {
                app.caps_error.clone().unwrap_or_default()
            } else {
                app.notice.clone()
            };
            format!(" {} · ? help · q quit", sanitize_display(&status))
        }
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn session_label(entry: &SessionEntry) -> String {
    format!("{}:{}", entry.provider, short_id(&entry.session_id))
}

fn session_detail(entry: &SessionEntry) -> String {
    let mut d = format!(
        "{} · {} events · {} turns · last {} · {}",
        entry.status,
        entry.event_count as u64,
        entry.turn_count as u64,
        ts_short(entry.last_event_at.as_deref()),
        entry.cwd.clone().unwrap_or_else(|| "(no cwd)".to_string())
    );
    if entry.stale {
        d.push_str(" · STALE");
    }
    if let Some(err) = &entry.error {
        d.push_str(&format!(" · error: {}", err));
    }
    if !entry.transcript_present {
        d.push_str(" · transcript missing");
    }
    d
}

/// Case-insensitive substring filter over label+detail (SelectList `/`).
fn session_matches(entry: &SessionEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    format!("{} {}", session_label(entry), session_detail(entry))
        .to_lowercase()
        .contains(&q)
}

fn render_sessions(app: &mut App, frame: &mut ratatui::Frame, area: Rect) {
    match &app.state {
        ViewState::Loading => {
            frame.render_widget(Paragraph::new("  loading…"), area);
        }
        ViewState::Error(e) => {
            let text = vec![
                Line::from(Span::styled(
                    "  error: ",
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
                )),
                Line::from(format!("  {}", sanitize_display(e))),
                Line::from("  R — retry · q — quit"),
            ];
            frame.render_widget(Paragraph::new(text), area);
        }
        ViewState::Ready => {
            // Filtered list honoring the `/` query.
            let list: Vec<&SessionEntry> = app
                .entries
                .iter()
                .filter(|e| session_matches(e, &app.query))
                .collect();
            if list.is_empty() {
                let msg = if app.entries.is_empty() {
                    "  (no indexed sessions — press i to index, or run `conversations index`)"
                } else {
                    "  no sessions match the current filter"
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
                let entry = list[app.scroll_top + i];
                let is_sel = app.scroll_top + i == app.cursor;
                let sel_style = if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let prefix = if is_sel { "❯ " } else { "  " };
                let label = session_label(entry);
                let detail = session_detail(entry);
                let w = area.width as usize;
                let used = prefix.chars().count() + label.chars().count() + 2;
                let mut spans = vec![
                    Span::styled(prefix, sel_style),
                    Span::styled(sanitize_display(&label), sel_style),
                ];
                if used < w {
                    let pad = w.saturating_sub(used);
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(
                        clip(&sanitize_display(&detail), w.saturating_sub(used)),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                } else {
                    spans.push(Span::styled(
                        " ".to_string(),
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

fn render_events(list: &mut EventList, frame: &mut ratatui::Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut out: Vec<Line> = Vec::new();

    if let Some(err) = &list.error {
        out.push(Line::from(Span::styled(
            format!(
                "  error: {}",
                clip(&sanitize_display(err), width.saturating_sub(10))
            ),
            Style::default().fg(Color::Red),
        )));
        out.push(Line::from(Span::styled(
            "  r — retry",
            Style::default().add_modifier(Modifier::DIM),
        )));
    } else if list.rows.is_empty() {
        // empty_text embeds session keys/status or the raw search query —
        // untrusted/provider-derived, sanitize before render.
        out.push(Line::from(Span::styled(
            format!("  {}", sanitize_display(&list.empty_text)),
            Style::default().add_modifier(Modifier::DIM),
        )));
        for n in list.notes.iter().take(3) {
            out.push(Line::from(Span::styled(
                format!("  {}", clip(&sanitize_display(n), width.saturating_sub(4))),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    } else {
        let body_h = height.saturating_sub(out.len() + 2).max(1);
        let max_scroll = list.rows.len().saturating_sub(body_h);
        list.scroll = list.scroll.min(max_scroll);
        for row in list.rows.iter().skip(list.scroll).take(body_h) {
            let e = &row.event;
            let event_type = event_field_str(e, "type").unwrap_or("");
            let badge = format!("[{}]", event_type);
            let badge = format!("{:<18}", badge);
            let turn = e
                .get("turn_index")
                .filter(|v| !v.is_null())
                .map(|v| format!("t{}", v))
                .unwrap_or_else(|| "-".to_string());
            let turn = format!("{:<5}", turn);
            let ts = ts_short(event_field_str(e, "timestamp"));
            let left = format!("{} {} {:<11} ", badge, turn, ts);
            let preview = clip(
                &event_preview(e),
                width.saturating_sub(display_width(&left) + 4).max(10),
            );
            out.push(Line::from(vec![
                Span::styled(
                    "  ".to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    sanitize_display(&left),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::raw(sanitize_display(&preview)),
            ]));
        }
        if list.rows.len() > body_h {
            out.push(Line::from(Span::styled(
                format!(
                    "  {}-{}/{}",
                    list.scroll + 1,
                    (list.scroll + body_h).min(list.rows.len()),
                    list.rows.len()
                ),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        let note_budget = height.saturating_sub(out.len() + 2);
        for n in list.notes.iter().take(note_budget) {
            out.push(Line::from(Span::styled(
                format!("  {}", clip(&sanitize_display(n), width.saturating_sub(4))),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }

    // Pad/truncate to the body height.
    while out.len() < height {
        out.push(Line::from(""));
    }
    out.truncate(height.max(1));
    frame.render_widget(Paragraph::new(out), area);
}

// --- entrypoint --------------------------------------------------------------------

/// argv for `plugin.pane.open` (exported for tests).
pub fn pane_open_args() -> Vec<String> {
    vec![
        "plugin".to_string(),
        "pane".to_string(),
        "open".to_string(),
        "--plugin".to_string(),
        PLUGIN_ID.to_string(),
        "--entrypoint".to_string(),
        ENTRYPOINT_ID.to_string(),
        "--placement".to_string(),
        "tab".to_string(),
    ]
}

/// `--open` action: ask herdr to open the `main` pane, then exit promptly.
fn open_pane() -> i32 {
    let bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HERDR_BIN.to_string());
    let result = Command::new(&bin)
        .args(pane_open_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match result {
        Ok(out) if out.status.success() => 0,
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = if stderr.trim().is_empty() {
                format!("exit {}", out.status.code().unwrap_or(-1))
            } else {
                stderr.trim().to_string()
            };
            eprintln!(
                "conversations: failed to open TUI pane (herdr {}): {}",
                bin, detail
            );
            1
        }
        Err(e) => {
            eprintln!(
                "conversations: failed to open TUI pane (herdr {}): {}",
                bin, e
            );
            1
        }
    }
}

const NON_TTY_NOTICE: [&str; 2] = [
    "rebuildup.conversations TUI requires an interactive terminal.",
    "CLI equivalent: conversations <capability|index|refresh|sessions|history|search|show|usage>",
];

fn tui_main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--open") {
        return open_pane();
    }

    // Non-TTY stdin → honest plain notice, exit 0 (mirrors TS main()).
    if !io::stdin().is_terminal() {
        for line in NON_TTY_NOTICE {
            println!("{}", line);
        }
        return 0;
    }

    let mut app = App::new();
    app.boot();

    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("conversations tui failed: could not enable raw mode");
        return 1;
    }
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("conversations tui failed: {}", err);
            return 1;
        }
    };
    // NOTE: no terminal.clear() here — ratatui's clear() queries the
    // cursor position (ESC[6n) and blocks ~2s on terminals that never
    // answer DSR. EnterAlternateScreen already gives a blank screen,
    // and the first draw repaints fully.

    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| render(&mut app, frame))?;
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if !app.handle_key(key) {
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
            let _ = writeln!(io::stderr(), "conversations TUI closed");
            0
        }
        Err(err) => {
            eprintln!("conversations tui failed: {}", err);
            1
        }
    }
}

/// `conversations tui [main|--open]` entrypoint; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    tui_main(args)
}
