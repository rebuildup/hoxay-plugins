//! rebuildup.usage interactive TUI (contract `herdr-plugins.tui/v1`,
//! ADR-0031/0032/0033, program #228 ticket #244).
//! Rust port of `plugins/usage/src/tui.ts` (retired; git history
//! preserves it).
//!
//! Browses the read-only `usage` / `quota` / `cost` / `divergence`
//! reports with in-TUI provider and period filters. The plugin has no
//! mutation commands (`mutation_commands: []`); the only stateful knob
//! is the explicit `--refresh` cache refresh on quota/cost, which is
//! user-triggered, never implicit on open.
//!
//! All data flows through this plugin's own domain command cores
//! (`cli::run_domain`) — the same public surface agents and external
//! clients use. Quota/hub detail views redact credential-shaped keys
//! before display (ADR-0014/0015 boundary). Loads run on worker
//! threads so the UI keeps rendering honest `loading…` states.
//!
//! Entrypoints (herdr-plugin.toml):
//!   `usage tui main`    — pane entrypoint (the UI)
//!   `usage tui --open`  — action: opens the `main` pane via
//!                         `plugin.pane.open` (HERDR_BIN_PATH) and
//!                         returns promptly

use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

// Shared safety helpers (ADR-0036): every untrusted string that can
// reach the renderer goes through `sanitize_display` first.
use hoxay_tui::safety::sanitize_display;

const PLUGIN_ID: &str = "rebuildup.usage";
const ENTRYPOINT_ID: &str = "main";
const HERDR_BIN: &str = "herdr";
/// Wall-clock bound for one report load — parity with the TS TUI's
/// 90 s SIGKILL + 120 s task deadline. The worker thread is detached
/// on expiry; only the UI state is bounded.
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);

// --- CLI bridge -----------------------------------------------------------------

/// Result of one in-process domain invocation (mirrors TS `CliResult`).
#[derive(Clone)]
struct CliResult {
    ok: bool,
    data: Option<Value>,
    error: Option<String>,
}

/// Run one domain command in-process — the same `cmd_*` cores the CLI
/// dispatches to. No self-respawn: reads share this process's view of
/// state and the pinned herdr session/socket targeting from the pane
/// environment.
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

// --- filters ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowPreset {
    All,
    H24,
    D7,
    D30,
}

const WINDOW_ORDER: [WindowPreset; 4] = [
    WindowPreset::All,
    WindowPreset::H24,
    WindowPreset::D7,
    WindowPreset::D30,
];

impl WindowPreset {
    fn label(self) -> &'static str {
        match self {
            WindowPreset::All => "all time",
            WindowPreset::H24 => "last 24h",
            WindowPreset::D7 => "last 7d",
            WindowPreset::D30 => "last 30d",
        }
    }

    fn span_ms(self) -> Option<i64> {
        match self {
            WindowPreset::All => None,
            WindowPreset::H24 => Some(24 * 3_600_000),
            WindowPreset::D7 => Some(7 * 24 * 3_600_000),
            WindowPreset::D30 => Some(30 * 24 * 3_600_000),
        }
    }
}

/// `--from <ISO>` args for a window preset (mirrors TS `windowArgs`).
fn window_args(w: WindowPreset) -> Vec<String> {
    let Some(span) = w.span_ms() else {
        return Vec::new();
    };
    let from = chrono::Utc::now() - chrono::Duration::milliseconds(span);
    vec![
        "--from".to_string(),
        from.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    ]
}

fn provider_args(provider: &str) -> Vec<String> {
    if provider.is_empty() {
        Vec::new()
    } else {
        vec!["--provider".to_string(), provider.to_string()]
    }
}

/// Provider ids observed in a report payload (for the `p` cycle).
fn providers_in_payload(payload: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut push = |id: Option<&str>| {
        if let Some(id) = id {
            if !id.is_empty() && !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }
    };
    let breakdowns = payload.get("breakdowns");
    if let Some(rows) = breakdowns
        .and_then(|b| b.get("by_provider"))
        .and_then(|v| v.as_array())
    {
        for row in rows {
            push(row.get("key").and_then(|v| v.as_str()));
        }
    }
    if let Some(rows) = payload
        .get("coverage")
        .and_then(|c| c.get("providers"))
        .and_then(|v| v.as_array())
    {
        for row in rows {
            push(row.get("provider").and_then(|v| v.as_str()));
        }
    }
    if let Some(rows) = payload.get("providers").and_then(|v| v.as_array()) {
        for row in rows {
            push(row.get("provider").and_then(|v| v.as_str()));
        }
    }
    if let Some(rows) = payload.get("comparisons").and_then(|v| v.as_array()) {
        for row in rows {
            push(row.get("provider").and_then(|v| v.as_str()));
        }
    }
    ids.sort();
    ids
}

/// Cycle provider filter: "" (all) → each observed id → "".
fn next_provider(current: &str, ids: &[String]) -> String {
    let mut cycle: Vec<&str> = vec![""];
    cycle.extend(ids.iter().map(|s| s.as_str()));
    let i = cycle.iter().position(|p| *p == current).unwrap_or(0);
    cycle[(i + 1) % cycle.len()].to_string()
}

// --- secret redaction (ADR-0014/0015 display boundary) ----------------------------

fn secret_key_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("(?i)key|token|secret|credential|authorization|password").unwrap())
}

/// Deep-copy `value` dropping object keys that look like credential material.
fn redact_secrets(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(redact_secrets).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if secret_key_re().is_match(k) {
                    continue;
                }
                out.insert(k.clone(), redact_secrets(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

// --- small formatting helpers ------------------------------------------------------

fn num(v: Option<&Value>) -> f64 {
    v.and_then(|n| n.as_f64())
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

fn num_or_null(v: Option<&Value>) -> Option<f64> {
    v.and_then(|n| n.as_f64()).filter(|n| n.is_finite())
}

fn jstr(v: Option<&Value>) -> Option<String> {
    v.and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn jbool(v: Option<&Value>) -> bool {
    v.and_then(|b| b.as_bool()).unwrap_or(false)
}

/// JS `toLocaleString("en-US")` equivalent: thousands-grouped integer.
fn fmt_int(n: f64) -> String {
    let n = n.round() as i64;
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if neg {
        format!("-{}", grouped)
    } else {
        grouped
    }
}

fn fmt_usd(n: Option<f64>) -> String {
    match n {
        None => "(not reported)".to_string(),
        Some(v) => {
            if v < 0.01 {
                format!("${:.6}", v)
            } else {
                format!("${:.4}", v)
            }
        }
    }
}

fn arr(v: Option<&Value>) -> Vec<&Value> {
    v.and_then(|a| a.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn rec(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    v.and_then(|o| o.as_object())
}

/// Pretty JSON lines for a DetailView, bounded (mirrors `jsonLines`).
fn json_lines(value: &Value, max_lines: usize) -> Vec<String> {
    let text = serde_json::to_string_pretty(&redact_secrets(value))
        .unwrap_or_else(|_| value.to_string());
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    if lines.len() > max_lines {
        let mut out = lines[..max_lines].to_vec();
        out.push(format!("… {} more lines", lines.len() - max_lines));
        out
    } else {
        lines
    }
}

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

/// Word-wrap a plain line to `width` cells (mirrors `wrapPlain`).
fn wrap_plain(s: &str, width: usize) -> Vec<String> {
    if display_width(s) <= width {
        return vec![s.to_string()];
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for w in words {
        let candidate = if cur.is_empty() {
            w.to_string()
        } else {
            format!("{} {}", cur, w)
        };
        if display_width(&candidate) <= width {
            cur = candidate;
        } else {
            if !cur.is_empty() {
                out.push(cur);
            }
            cur = w.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    let mut final_out: Vec<String> = Vec::new();
    for l in out {
        if display_width(&l) <= width {
            final_out.push(l);
        } else {
            let mut rest = l;
            while display_width(&rest) > width {
                final_out.push(clip(&rest, width));
                // byte-slice by chars to `width` columns (ASCII-safe approximation,
                // mirroring the TS `rest.slice(width)`).
                let mut idx = 0usize;
                let mut used = 0usize;
                for (i, ch) in rest.char_indices() {
                    let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if used + w > width {
                        break;
                    }
                    used += w;
                    idx = i + ch.len_utf8();
                }
                rest = rest[idx..].to_string();
                if idx == 0 {
                    break;
                }
            }
            if !rest.is_empty() {
                final_out.push(rest);
            }
        }
    }
    if final_out.is_empty() {
        vec![String::new()]
    } else {
        final_out
    }
}

// --- report view model --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportKind {
    Usage,
    Cost,
    Quota,
    Divergence,
}

/// One selectable row in a quota/divergence list or a `Select` screen.
#[derive(Clone)]
struct ListItem {
    label: String,
    detail: String,
    data: Value,
}

struct ReportView {
    kind: ReportKind,
    windowed: bool,
    refreshable: bool,
    provider: String,
    window: WindowPreset,
    provider_ids: Vec<String>,
    payload: Option<Value>,
    error: Option<String>,
    loading: bool,
    rx: Option<Receiver<CliResult>>,
    /// When the in-flight load started; enforces the wall-clock bound
    /// the TS version had via its 90 s SIGKILL + task deadline.
    load_started: Option<Instant>,
    scroll: usize,
    /// Selection index for list-mode views (quota / divergence).
    sel: usize,
    /// `/` filter for list-mode views.
    query: String,
    filter_mode: bool,
    items: Vec<ListItem>,
}

impl ReportView {
    fn new(kind: ReportKind) -> Self {
        let mut v = ReportView {
            kind,
            windowed: matches!(kind, ReportKind::Usage | ReportKind::Cost | ReportKind::Divergence),
            refreshable: matches!(kind, ReportKind::Cost | ReportKind::Quota),
            provider: String::new(),
            window: WindowPreset::All,
            provider_ids: Vec::new(),
            payload: None,
            error: None,
            loading: false,
            rx: None,
            load_started: None,
            scroll: 0,
            sel: 0,
            query: String::new(),
            filter_mode: false,
            items: Vec::new(),
        };
        v.start_load(false);
        v
    }

    fn title(&self) -> &'static str {
        match self.kind {
            ReportKind::Usage => "usage",
            ReportKind::Cost => "cost estimate",
            ReportKind::Quota => "quota",
            ReportKind::Divergence => "quota vs measured divergence",
        }
    }

    fn command(&self) -> &'static str {
        match self.kind {
            ReportKind::Usage => "usage",
            ReportKind::Cost => "cost",
            ReportKind::Quota => "quota",
            ReportKind::Divergence => "divergence",
        }
    }

    fn list_mode(&self) -> bool {
        matches!(self.kind, ReportKind::Quota | ReportKind::Divergence)
    }

    fn args(&self, refresh: bool) -> Vec<String> {
        let mut args = vec![self.command().to_string()];
        if self.windowed {
            args.extend(window_args(self.window));
        }
        args.extend(provider_args(&self.provider));
        if refresh {
            args.push("--refresh".to_string());
        }
        args
    }

    fn start_load(&mut self, refresh: bool) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.error = None;
        self.load_started = Some(Instant::now());
        let args = self.args(refresh);
        let (tx, rx) = channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let res = run_cli(&args);
            let _ = tx.send(res);
        });
    }

    /// Non-blocking: pick up a finished load, if any. A single
    /// `try_recv` — probing with one call and re-receiving would
    /// consume the result and misreport every success as a failure.
    /// A load that outlives `LOAD_TIMEOUT` is reported as timed out;
    /// the worker thread keeps running detached and its result is
    /// discarded (Rust cannot kill a thread).
    fn poll_load(&mut self) {
        if self.loading
            && self
                .load_started
                .map(|t| t.elapsed() >= LOAD_TIMEOUT)
                .unwrap_or(false)
        {
            self.rx = None;
            self.load_started = None;
            self.loading = false;
            self.error = Some("load timed out".to_string());
            self.payload = None;
            return;
        }
        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok(res) => {
                self.rx = None;
                self.load_started = None;
                self.finish_load(res);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // Sender dropped without a value: treat as failure.
                self.rx = None;
                self.load_started = None;
                self.loading = false;
                self.error = Some("load failed".to_string());
                self.payload = None;
            }
        }
    }

    fn finish_load(&mut self, res: CliResult) {
        self.loading = false;
        match (res.ok, res.data) {
            (true, Some(data)) => {
                self.provider_ids = providers_in_payload(&data);
                self.items = match self.kind {
                    ReportKind::Quota => quota_items(&data),
                    ReportKind::Divergence => divergence_items(&data),
                    _ => Vec::new(),
                };
                self.payload = Some(data);
                self.error = None;
                self.sel = self
                    .sel
                    .min(self.filtered_indices().len().saturating_sub(1));
                self.scroll = 0;
            }
            _ => {
                self.error = Some(res.error.unwrap_or_else(|| "report failed".to_string()));
                self.payload = None;
            }
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                q.is_empty()
                    || format!("{} {}", i.label, i.detail)
                        .to_lowercase()
                        .contains(&q)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn clamp_sel(&mut self) {
        let n = self.filtered_indices().len();
        if self.sel >= n {
            self.sel = n.saturating_sub(1);
        }
    }

    /// Returns true when the key was consumed.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != event::KeyEventKind::Press {
            return true;
        }
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter_mode = false;
                    self.query.clear();
                }
                KeyCode::Enter => {
                    self.filter_mode = false;
                    self.clamp_sel();
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.query.push(c);
                }
                _ => {}
            }
            return true;
        }
        if self.list_mode() {
            return self.handle_list_key(key);
        }
        match key.code {
            KeyCode::Char('r') => {
                self.start_load(false);
                true
            }
            KeyCode::Char('f') if self.refreshable => {
                self.start_load(true);
                true
            }
            KeyCode::Char('p') => {
                self.provider = next_provider(&self.provider, &self.provider_ids);
                self.start_load(false);
                true
            }
            KeyCode::Char('w') if self.windowed => {
                let i = WINDOW_ORDER
                    .iter()
                    .position(|w| *w == self.window)
                    .unwrap_or(0);
                self.window = WINDOW_ORDER[(i + 1) % WINDOW_ORDER.len()];
                self.start_load(false);
                true
            }
            KeyCode::Char('b') if matches!(self.kind, ReportKind::Usage | ReportKind::Cost) => {
                // Handled by the app (needs the screen stack for selects);
                // report "consumed" so an empty breakdown never pops.
                true
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll += 1;
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                true
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll += 10;
                true
            }
            KeyCode::Home => {
                self.scroll = 0;
                true
            }
            KeyCode::End => {
                self.scroll = usize::MAX / 2;
                true
            }
            _ => false,
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> bool {
        let n = self.filtered_indices().len();
        match key.code {
            KeyCode::Esc => false, // back to the previous screen
            KeyCode::Char('r') => {
                self.start_load(false);
                true
            }
            KeyCode::Char('f') if self.refreshable => {
                self.start_load(true);
                true
            }
            KeyCode::Char('p') => {
                self.provider = next_provider(&self.provider, &self.provider_ids);
                self.start_load(false);
                true
            }
            KeyCode::Char('w') if self.windowed => {
                let i = WINDOW_ORDER
                    .iter()
                    .position(|w| *w == self.window)
                    .unwrap_or(0);
                self.window = WINDOW_ORDER[(i + 1) % WINDOW_ORDER.len()];
                self.start_load(false);
                true
            }
            KeyCode::Char('/') => {
                self.filter_mode = true;
                true
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if n > 0 {
                    self.sel = (self.sel + 1).min(n - 1);
                }
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.sel = self.sel.saturating_sub(1);
                true
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                if n > 0 {
                    self.sel = (self.sel + 10).min(n - 1);
                }
                true
            }
            KeyCode::PageUp => {
                self.sel = self.sel.saturating_sub(10);
                true
            }
            KeyCode::Home => {
                self.sel = 0;
                true
            }
            KeyCode::End => {
                if n > 0 {
                    self.sel = n - 1;
                }
                true
            }
            KeyCode::Enter => false, // app resolves selection → detail
            _ => true,
        }
    }
}

// --- payload → list items -----------------------------------------------------------

/// `quotaItems` from tui.ts.
fn quota_items(payload: &Value) -> Vec<ListItem> {
    let mut items = Vec::new();
    for p in arr(payload.get("providers")) {
        let provider = jstr(p.get("provider")).unwrap_or_else(|| "?".to_string());
        let status = jstr(p.get("status")).unwrap_or_else(|| "?".to_string());
        let used = window_used(p.get("windows"));
        let stale = if jbool(p.get("stale")) { " · STALE" } else { "" };
        let detail_str = jstr(p.get("detail"));
        let detail = format!(
            "{}{}{}{}",
            status,
            stale,
            if used.is_empty() { String::new() } else { format!(" · {}", used) },
            detail_str.map(|d| format!(" · {}", d)).unwrap_or_default(),
        );
        items.push(ListItem {
            label: provider,
            detail,
            data: p.clone(),
        });
    }
    for hub in arr(payload.get("hubs")) {
        let hub_id = jstr(hub.get("id")).unwrap_or_else(|| "hub".to_string());
        let accounts = arr(hub.get("accounts"));
        for acct in &accounts {
            let provider = jstr(acct.get("provider")).unwrap_or_else(|| "?".to_string());
            let status = jstr(acct.get("status")).unwrap_or_else(|| "?".to_string());
            let used = window_used(acct.get("windows"));
            let mut data = serde_json::Map::new();
            data.insert("hub".to_string(), Value::String(hub_id.clone()));
            data.insert("account".to_string(), (*acct).clone());
            items.push(ListItem {
                label: format!(
                    "{} · {}",
                    hub_id,
                    jstr(acct.get("account")).unwrap_or_else(|| "account".to_string())
                ),
                detail: format!(
                    "{}: {}{}",
                    provider,
                    status,
                    if used.is_empty() { String::new() } else { format!(" · {}", used) },
                ),
                data: Value::Object(data),
            });
        }
        if accounts.is_empty() {
            let hub_status = jstr(hub.get("status")).unwrap_or_else(|| "?".to_string());
            let hub_detail = jstr(hub.get("detail"));
            let mut data = serde_json::Map::new();
            data.insert("hub".to_string(), Value::String(hub_id.clone()));
            data.insert(
                "hub_status".to_string(),
                hub.get("status").cloned().unwrap_or(Value::Null),
            );
            data.insert(
                "detail".to_string(),
                hub.get("detail").cloned().unwrap_or(Value::Null),
            );
            items.push(ListItem {
                label: hub_id,
                detail: format!(
                    "hub status: {}{}",
                    hub_status,
                    hub_detail.map(|d| format!(" · {}", d)).unwrap_or_default(),
                ),
                data: Value::Object(data),
            });
        }
    }
    items
}

fn window_used(windows: Option<&Value>) -> String {
    arr(windows)
        .iter()
        .filter_map(|w| {
            let pct = w.get("used_percent").and_then(|v| v.as_f64())?;
            let label = jstr(w.get("label"))
                .or_else(|| jstr(w.get("id")))
                .unwrap_or_else(|| "window".to_string());
            Some(format!("{} {:.0}%", label, pct))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `divergenceItems` from tui.ts.
fn divergence_items(payload: &Value) -> Vec<ListItem> {
    arr(payload.get("comparisons"))
        .iter()
        .map(|c| {
            let provider = jstr(c.get("provider")).unwrap_or_else(|| "?".to_string());
            let verdict = jstr(c.get("verdict")).unwrap_or_else(|| "?".to_string());
            let detail = jstr(c.get("detail"));
            let mut bits: Vec<String> = vec![verdict.clone()];
            if let Some(d) = detail {
                bits.push(d);
            }
            if let Some(quota) = rec(c.get("quota")) {
                bits.push(format!(
                    "quota:{}",
                    jstr(quota.get("status")).unwrap_or_else(|| "?".to_string())
                ));
            }
            if let Some(measured) = rec(c.get("measured")) {
                bits.push(format!(
                    "measured total_tokens={}",
                    fmt_int(num(measured.get("total_tokens")))
                ));
            }
            ListItem {
                label: provider,
                detail: bits.join(" · "),
                data: (*c).clone(),
            }
        })
        .collect()
}

// --- usage / cost report lines ---------------------------------------------------------

fn breakdown_lines(breakdowns: Option<&Value>, dims: &[&str], max_rows: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(breakdowns) = breakdowns else {
        return lines;
    };
    for dim in dims {
        let rows = arr(breakdowns.get(*dim));
        if rows.is_empty() {
            continue;
        }
        lines.push(format!("  {}:", dim));
        for r in rows.iter().take(max_rows) {
            let key = jstr(r.get("key")).unwrap_or_else(|| "(none)".to_string());
            let cost = r
                .get("provider_reported_cost_usd")
                .and_then(|v| v.as_f64())
                .map(|c| format!(" · cost {}", fmt_usd(Some(c))))
                .unwrap_or_default();
            lines.push(format!(
                "    {:<28} ev={:>7} in={:>10} out={:>10} total={:>12}{}",
                clip(&key, 28),
                fmt_int(num(r.get("events"))),
                fmt_int(num(r.get("input_tokens"))),
                fmt_int(num(r.get("output_tokens"))),
                fmt_int(num(r.get("total_tokens"))),
                cost,
            ));
        }
        if rows.len() > max_rows {
            lines.push(format!("    … {} more (b — browse)", rows.len() - max_rows));
        }
    }
    lines
}

/// `usageReportLines` from tui.ts.
fn usage_report_lines(payload: &Value, title_is_cost: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let totals = payload.get("totals").and_then(|v| v.as_object());
    let mut hints: Vec<String> = Vec::new();
    if let Some(hint) = jstr(payload.get("hint")) {
        hints.push(hint);
    }
    for w in arr(payload.get("warnings")) {
        if let Some(w) = jstr(Some(w)) {
            hints.push(format!("warning: {}", w));
        }
    }

    if let Some(totals) = totals {
        lines.push("totals:".to_string());
        lines.push(format!(
            "  events={} · sessions_with_usage={}",
            fmt_int(num(totals.get("events"))),
            fmt_int(num(totals.get("sessions_with_usage"))),
        ));
        let thinking = totals
            .get("thinking_tokens")
            .filter(|v| !v.is_null())
            .map(|v| format!(" (thinking={})", fmt_int(num(Some(v)))))
            .unwrap_or_default();
        lines.push(format!(
            "  tokens: in={} out={} cache_read={} cache_write={} total={}{}",
            fmt_int(num(totals.get("input_tokens"))),
            fmt_int(num(totals.get("output_tokens"))),
            fmt_int(num(totals.get("cache_read_input_tokens"))),
            fmt_int(num(totals.get("cache_creation_input_tokens"))),
            fmt_int(num(totals.get("total_tokens"))),
            thinking,
        ));
        lines.push(format!(
            "  provider-reported cost: {} over {} event(s) — not a bill",
            fmt_usd(num_or_null(totals.get("provider_reported_cost_usd"))),
            fmt_int(num(totals.get("provider_reported_cost_events"))),
        ));
        let excluded = num(totals.get("excluded_untimestamped_events"));
        let dups = num(totals.get("duplicate_events_skipped"));
        let no_id = num(totals.get("events_without_event_id"));
        if excluded + dups + no_id > 0.0 {
            lines.push(format!(
                "  excluded: untimestamped={} duplicates={} no_event_id={}",
                excluded, dups, no_id
            ));
        }
    } else {
        lines.push("(no totals in payload)".to_string());
    }

    if title_is_cost {
        if let Some(cost) = payload.get("cost").and_then(|v| v.as_object()) {
            lines.push(String::new());
            lines.push("cost (ESTIMATE — list-price math, never a bill):".to_string());
            lines.push(format!(
                "  estimated_cost_usd: {}",
                fmt_usd(num_or_null(cost.get("estimated_cost_usd")))
            ));
            lines.push(format!(
                "  estimate_complete: {}",
                jbool(cost.get("estimate_complete"))
            ));
            lines.push(format!(
                "  priced_models={} · unpriced_models={}",
                fmt_int(num(cost.get("priced_models"))),
                arr(cost.get("unpriced_models")).len(),
            ));
            if let Some(pricing) = cost.get("pricing").and_then(|v| v.as_object()) {
                lines.push(format!(
                    "  pricing: {} · source={} · known_models={} · user_overrides={}",
                    jstr(pricing.get("status")).unwrap_or_else(|| "?".to_string()),
                    jstr(pricing.get("source")).unwrap_or_else(|| "?".to_string()),
                    fmt_int(num(pricing.get("known_models"))),
                    fmt_int(num(pricing.get("user_overrides"))),
                ));
                if let Some(detail) = jstr(pricing.get("detail")) {
                    lines.push(format!("  pricing detail: {}", detail));
                }
            }
            if let Some(disclaimer) = jstr(cost.get("disclaimer")) {
                lines.push(format!("  {}", disclaimer));
            }
        }
    }

    if let Some(cache_tokens) = payload.get("cache_tokens").and_then(|v| v.as_object()) {
        let providers = arr(cache_tokens.get("providers"));
        if !providers.is_empty() {
            lines.push(String::new());
            lines.push("cache tokens:".to_string());
            for p in providers {
                let detail = jstr(p.get("detail"))
                    .map(|d| format!(" · {}", d))
                    .unwrap_or_default();
                lines.push(format!(
                    "  {}: {} · read={} write={}{}",
                    jstr(p.get("provider")).unwrap_or_else(|| "?".to_string()),
                    jstr(p.get("status")).unwrap_or_else(|| "?".to_string()),
                    fmt_int(num(p.get("cache_read_input_tokens"))),
                    fmt_int(num(p.get("cache_creation_input_tokens"))),
                    detail,
                ));
            }
        }
    }

    if let Some(coverage) = payload.get("coverage").and_then(|v| v.as_object()) {
        lines.push(String::new());
        lines.push(format!(
            "coverage: sessions={} ok={} issues={} stale={} events_missing={}",
            fmt_int(num(coverage.get("sessions_total"))),
            fmt_int(num(coverage.get("sessions_ok"))),
            fmt_int(num(coverage.get("sessions_with_issues"))),
            fmt_int(num(coverage.get("sessions_stale"))),
            fmt_int(num(coverage.get("sessions_events_missing"))),
        ));
        let cov_providers = arr(coverage.get("providers"));
        if !cov_providers.is_empty() {
            lines.push("  providers:".to_string());
            for p in cov_providers {
                lines.push(format!(
                    "    {}: status={} sessions={} ok={} issues={} usage_events={}",
                    jstr(p.get("provider")).unwrap_or_else(|| "?".to_string()),
                    jstr(p.get("provider_status")).unwrap_or_else(|| "?".to_string()),
                    fmt_int(num(p.get("sessions"))),
                    fmt_int(num(p.get("sessions_ok"))),
                    fmt_int(num(p.get("sessions_with_issues"))),
                    fmt_int(num(p.get("usage_events"))),
                ));
            }
        }
    }

    let bd = breakdown_lines(
        payload.get("breakdowns"),
        &["by_provider", "by_model"],
        12,
    );
    if !bd.is_empty() {
        lines.push(String::new());
        lines.push("breakdowns:".to_string());
        lines.extend(bd);
    }

    let sessions = arr(payload.get("sessions"));
    if !sessions.is_empty() {
        lines.push(String::new());
        lines.push(format!("sessions contributing: {}", sessions.len()));
    }

    if let Some(source) = payload.get("source").and_then(|v| v.as_object()) {
        lines.push(format!(
            "source: {} via {} · pages={} complete={}",
            jstr(source.get("plugin")).unwrap_or_else(|| "?".to_string()),
            jstr(source.get("resolved_via")).unwrap_or_else(|| "?".to_string()),
            fmt_int(num(source.get("pages"))),
            jbool(source.get("complete")),
        ));
    }
    for h in hints {
        lines.push(h);
    }
    lines
}

// --- breakdown browsing (usage / cost `b` key) ----------------------------------------

const BREAKDOWN_DIMS: [&str; 6] = [
    "by_provider",
    "by_model",
    "by_host",
    "by_project",
    "by_session",
    "by_pane",
];

/// A modal select list pushed on the screen stack.
struct SelectView {
    title: String,
    items: Vec<ListItem>,
    filterable: bool,
    cursor: usize,
    query: String,
    filter_mode: bool,
    kind: SelectKind,
}

#[derive(Clone)]
enum SelectKind {
    /// Top-level report picker.
    Reports,
    /// Breakdown dimension picker (needs parent payload — found on the stack).
    Breakdowns,
    /// Rows of one breakdown dimension.
    BreakdownRows { dim: String },
}

impl SelectView {
    fn filtered(&self) -> Vec<&ListItem> {
        let q = self.query.to_lowercase();
        self.items
            .iter()
            .filter(|i| {
                q.is_empty()
                    || format!("{} {}", i.label, i.detail)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect()
    }

    fn handle_key(&mut self, key: KeyEvent) -> SelectOutcome {
        if key.kind != event::KeyEventKind::Press {
            return SelectOutcome::Consumed;
        }
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter_mode = false;
                    self.query.clear();
                }
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.query.push(c);
                }
                _ => {}
            }
            return SelectOutcome::Consumed;
        }
        let n = self.filtered().len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => SelectOutcome::Pop,
            KeyCode::Enter => {
                let n = self.filtered().len();
                let idx = self.cursor.min(n.saturating_sub(1));
                let item = self.filtered().get(idx).map(|i| (*i).clone());
                match item {
                    Some(item) => SelectOutcome::Choose(item),
                    None => SelectOutcome::Consumed,
                }
            }
            KeyCode::Char('/') if self.filterable => {
                self.filter_mode = true;
                SelectOutcome::Consumed
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if n > 0 {
                    self.cursor = (self.cursor + 1).min(n - 1);
                }
                SelectOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                SelectOutcome::Consumed
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                if n > 0 {
                    self.cursor = (self.cursor + 10).min(n - 1);
                }
                SelectOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(10);
                SelectOutcome::Consumed
            }
            KeyCode::Home => {
                self.cursor = 0;
                SelectOutcome::Consumed
            }
            KeyCode::End => {
                if n > 0 {
                    self.cursor = n - 1;
                }
                SelectOutcome::Consumed
            }
            _ => SelectOutcome::Consumed,
        }
    }
}

enum SelectOutcome {
    Consumed,
    Pop,
    Choose(ListItem),
}

// --- screens -------------------------------------------------------------------------

enum Screen {
    Reports(SelectView),
    Report(ReportView),
    Select(SelectView),
    Detail {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
}

struct App {
    screens: Vec<Screen>,
    notice: String,
}

const REPORTS: [(&str, &str, ReportKind); 4] = [
    ("usage", "token totals by provider/model/host/project/session/pane", ReportKind::Usage),
    ("quota", "subscription quota & reset windows (direct + hubs)", ReportKind::Quota),
    ("cost", "estimated API cost (list-price estimate, never a bill)", ReportKind::Cost),
    ("divergence", "quota vs measured usage, side by side", ReportKind::Divergence),
];

impl App {
    fn new() -> Self {
        let items = REPORTS
            .iter()
            .map(|(id, detail, _)| ListItem {
                label: id.to_string(),
                detail: detail.to_string(),
                data: serde_json::json!({ "report": id }),
            })
            .collect();
        App {
            screens: vec![Screen::Reports(SelectView {
                title: "rebuildup.usage".to_string(),
                items,
                filterable: false,
                cursor: 0,
                query: String::new(),
                filter_mode: false,
                kind: SelectKind::Reports,
            })],
            notice: String::new(),
        }
    }

    fn push(&mut self, screen: Screen) {
        self.screens.push(screen);
    }

    fn poll_loads(&mut self) {
        if let Some(Screen::Report(v)) = self.screens.last_mut() {
            v.poll_load();
        }
    }

    /// Resolve a completed select (`Choose`) into follow-up screens.
    fn resolve_select(&mut self, item: ListItem, kind: SelectKind) {
        match kind {
            SelectKind::Reports => {
                let id = item.label.as_str();
                let kind = REPORTS
                    .iter()
                    .find(|(rid, _, _)| *rid == id)
                    .map(|(_, _, k)| *k);
                if let Some(kind) = kind {
                    self.push(Screen::Report(ReportView::new(kind)));
                }
            }
            SelectKind::Breakdowns => {
                let dim = item.label.clone();
                // Rows of this dimension from the nearest Report below.
                let rows = self
                    .screens
                    .iter()
                    .rev()
                    .find_map(|s| match s {
                        Screen::Report(v) => v.payload.as_ref().and_then(|p| {
                            p.get("breakdowns")
                                .and_then(|b| b.get(dim.as_str()))
                                .and_then(|v| v.as_array())
                                .cloned()
                        }),
                        _ => None,
                    })
                    .unwrap_or_default();
                let items = rows
                    .iter()
                    .map(|r| ListItem {
                        label: jstr(r.get("key")).unwrap_or_else(|| "(none)".to_string()),
                        detail: format!(
                            "events={} total_tokens={}",
                            fmt_int(num(r.get("events"))),
                            fmt_int(num(r.get("total_tokens"))),
                        ),
                        data: r.clone(),
                    })
                    .collect();
                self.push(Screen::Select(SelectView {
                    title: dim.clone(),
                    items,
                    filterable: true,
                    cursor: 0,
                    query: String::new(),
                    filter_mode: false,
                    kind: SelectKind::BreakdownRows { dim },
                }));
            }
            SelectKind::BreakdownRows { dim } => {
                let key = jstr(item.data.get("key")).unwrap_or_else(|| "(none)".to_string());
                self.push(Screen::Detail {
                    title: format!("{} — {}", dim, key),
                    lines: json_lines(&item.data, 400),
                    scroll: 0,
                });
            }
        }
    }

    /// Returns true to keep running, false to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != event::KeyEventKind::Press {
            return true;
        }
        // Take the top screen out to satisfy the borrow checker, then
        // put it back (or replace it) once handled.
        let Some(mut screen) = self.screens.pop() else {
            return false;
        };
        enum Action {
            Keep,
            Pop,
            Quit,
            Choose { item: ListItem, kind: SelectKind },
        }
        let action = match &mut screen {
            Screen::Reports(sel) | Screen::Select(sel) => {
                let kind = sel.kind.clone();
                match sel.handle_key(key) {
                    SelectOutcome::Consumed => Action::Keep,
                    SelectOutcome::Pop => {
                        if matches!(kind, SelectKind::Reports) {
                            Action::Quit
                        } else {
                            Action::Pop
                        }
                    }
                    SelectOutcome::Choose(item) => Action::Choose { item, kind },
                }
            }
            Screen::Report(view) => {
                // 'b' on usage/cost opens the breakdown-dimension picker.
                if key.code == KeyCode::Char('b')
                    && matches!(view.kind, ReportKind::Usage | ReportKind::Cost)
                {
                    let dims: Vec<ListItem> = BREAKDOWN_DIMS
                        .iter()
                        .filter_map(|d| {
                            let rows = view
                                .payload
                                .as_ref()
                                .and_then(|p| p.get("breakdowns"))
                                .and_then(|b| b.get(*d))
                                .and_then(|v| v.as_array())?;
                            if rows.is_empty() {
                                return None;
                            }
                            Some(ListItem {
                                label: d.to_string(),
                                detail: format!("{} rows", rows.len()),
                                data: serde_json::json!({ "dim": d }),
                            })
                        })
                        .collect();
                    if dims.is_empty() {
                        self.notice = "no breakdown rows in current report".to_string();
                    } else {
                        self.screens.push(screen);
                        self.screens.push(Screen::Select(SelectView {
                            title: "breakdowns".to_string(),
                            items: dims,
                            filterable: true,
                            cursor: 0,
                            query: String::new(),
                            filter_mode: false,
                            kind: SelectKind::Breakdowns,
                        }));
                        return true;
                    }
                }
                // Enter on a list view (quota / divergence) opens detail.
                if key.code == KeyCode::Enter && view.list_mode() {
                    let idx = view.filtered_indices().get(view.sel).copied();
                    if let Some(idx) = idx {
                        let item = view.items[idx].clone();
                        self.screens.push(screen);
                        self.screens.push(Screen::Detail {
                            title: item.label.clone(),
                            lines: json_lines(&item.data, 400),
                            scroll: 0,
                        });
                        return true;
                    }
                }
                if view.handle_key(key) {
                    Action::Keep
                } else if key.code == KeyCode::Esc {
                    Action::Pop
                } else {
                    // Unbound keys are swallowed (TS: report views own
                    // their keymap; unhandled keys never navigate).
                    Action::Keep
                }
            }
            Screen::Detail { scroll, lines, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => Action::Pop,
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = (*scroll + 1).min(lines.len().saturating_sub(1));
                    Action::Keep
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *scroll = scroll.saturating_sub(1);
                    Action::Keep
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    *scroll = (*scroll + 10).min(lines.len().saturating_sub(1));
                    Action::Keep
                }
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(10);
                    Action::Keep
                }
                KeyCode::Home => {
                    *scroll = 0;
                    Action::Keep
                }
                KeyCode::End => {
                    *scroll = lines.len().saturating_sub(1);
                    Action::Keep
                }
                _ => Action::Keep,
            },
        };
        match action {
            Action::Keep => {
                self.screens.push(screen);
            }
            Action::Pop => {
                // Drop the top screen; the parent is already below it.
            }
            Action::Quit => return false,
            Action::Choose { item, kind } => {
                self.screens.push(screen);
                self.resolve_select(item, kind);
            }
        }
        true
    }
}

// --- render --------------------------------------------------------------------------

fn render(app: &mut App, frame: &mut ratatui::Frame) {
    app.poll_loads();
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Header.
    let title = match app.screens.last() {
        Some(Screen::Report(v)) => {
            let filter = if v.windowed {
                format!(
                    " — provider: {} · window: {}",
                    if v.provider.is_empty() { "all" } else { &v.provider },
                    v.window.label(),
                )
            } else {
                format!(
                    " — provider: {}",
                    if v.provider.is_empty() { "all" } else { &v.provider },
                )
            };
            format!(" {}{}", v.title(), filter)
        }
        Some(Screen::Detail { title, .. }) => format!(" {}", title),
        Some(Screen::Select(sel)) => format!(" {}", sel.title),
        Some(Screen::Reports(_)) => " rebuildup.usage".to_string(),
        None => String::new(),
    };
    frame.render_widget(
        Paragraph::new(sanitize_display(&title))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    match app.screens.last_mut() {
        Some(Screen::Reports(sel)) | Some(Screen::Select(sel)) => {
            render_select(sel, frame, chunks[1]);
        }
        Some(Screen::Report(view)) => render_report(view, frame, chunks[1]),
        Some(Screen::Detail {
            title,
            lines,
            scroll,
        }) => {
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
        None => {}
    }

    // Footer.
    let hint = match app.screens.last() {
        Some(Screen::Reports(_)) => " ↑↓/jk move · enter open".to_string(),
        Some(Screen::Select(sel)) if sel.filterable => {
            " ↑↓/jk move · / filter · enter select · esc back".to_string()
        }
        Some(Screen::Select(_)) => " ↑↓/jk move · enter open · esc back".to_string(),
        Some(Screen::Detail { .. }) => " ↑↓/jk scroll · pgup/pgdn page · esc back".to_string(),
        Some(Screen::Report(v)) => {
            let mut h = String::from(" ↑↓/jk scroll · r reload");
            if v.windowed {
                h.push_str(" · w window");
            }
            h.push_str(" · p provider");
            if v.refreshable {
                h.push_str(" · f refresh");
            }
            if matches!(v.kind, ReportKind::Usage | ReportKind::Cost) {
                h.push_str(" · b breakdowns");
            }
            h.push_str(" · esc back");
            h
        }
        None => String::new(),
    };
    let hint = if app.notice.is_empty() {
        hint
    } else {
        format!(" {} · {}", sanitize_display(&app.notice), hint)
    };
    frame.render_widget(
        Paragraph::new(sanitize_display(&hint)).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_select(sel: &mut SelectView, frame: &mut ratatui::Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    let filtered = sel.filtered();
    let visible = height.saturating_sub(1).max(1);
    let n = filtered.len();
    let mut top = 0usize;
    // Keep the selection visible.
    if n > 0 {
        if sel.cursor >= top + visible {
            top = sel.cursor + 1 - visible;
        }
        if sel.cursor < top {
            top = sel.cursor;
        }
    }
    let mut lines: Vec<Line> = Vec::new();
    if n == 0 {
        lines.push(Line::from(Span::styled(
            "  (empty)",
            Style::default().add_modifier(Modifier::DIM),
        )));
        lines.push(Line::from(if sel.query.is_empty() {
            "  no items".to_string()
        } else {
            format!("  no items match \"{}\"", sel.query)
        }));
    } else {
        for i in 0..visible.min(n.saturating_sub(top)) {
            let item = filtered[top + i];
            let is_sel = top + i == sel.cursor;
            let style = if is_sel {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let prefix = if is_sel { "❯ " } else { "  " };
            let mut spans = vec![
                Span::styled(prefix, style),
                Span::styled(sanitize_display(&item.label), style),
            ];
            if !item.detail.is_empty() {
                let used = prefix.chars().count()
                    + UnicodeWidthStr::width(item.label.as_str())
                    + 2;
                if used < width {
                    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
                    spans.push(Span::styled(
                        clip(&sanitize_display(&item.detail), width.saturating_sub(used)),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
            }
            lines.push(Line::from(spans));
        }
        if n > visible {
            lines.push(Line::from(Span::styled(
                format!("  {}/{}", sel.cursor + 1, n),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }
    if sel.filter_mode {
        lines.push(Line::from(Span::styled(
            format!("  filter: /{}█", sanitize_display(&sel.query)),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    while lines.len() < height {
        lines.push(Line::from(""));
    }
    lines.truncate(height.max(1));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_report(view: &mut ReportView, frame: &mut ratatui::Frame, area: Rect) {
    let width = area.width as usize;
    let height = area.height as usize;
    let mut out: Vec<Line> = Vec::new();

    if view.list_mode() {
        // Header rows (notes / hub count) then the selectable list.
        if let Some(payload) = &view.payload {
            if view.kind == ReportKind::Divergence {
                for n in arr(payload.get("notes")) {
                    if let Some(n) = jstr(Some(n)) {
                        out.push(Line::from(Span::styled(
                            format!("  note: {}", clip(&n, width.saturating_sub(8))),
                            Style::default().add_modifier(Modifier::DIM),
                        )));
                    }
                }
                let hubs = arr(payload.get("hubs"));
                if !hubs.is_empty() {
                    out.push(Line::from(Span::styled(
                        format!("  hub readings (excluded from comparisons): {}", hubs.len()),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
            }
            if view.kind == ReportKind::Quota {
                if let Some(cache) = payload.get("cache").and_then(|v| v.as_object()) {
                    out.push(Line::from(Span::styled(
                        format!(
                            "  cache ttl={}s · providers={} · f — refresh sources",
                            fmt_int(num(cache.get("ttl_seconds"))),
                            view.items.len(),
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
            }
        }
        if view.loading && view.payload.is_none() {
            out.push(Line::from(Span::styled(
                "  loading…",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else if let Some(err) = &view.error {
            out.push(Line::from(vec![
                Span::styled(
                    "  error: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    clip(err, width.saturating_sub(10)),
                    Style::default().fg(Color::Red),
                ),
            ]));
        } else if view.payload.is_some() {
            let indices = view.filtered_indices();
            let inner_h = height.saturating_sub(out.len() + 2).max(1);
            let visible = inner_h;
            let n = indices.len();
            if view.sel >= n {
                view.sel = n.saturating_sub(1);
            }
            let mut top = 0usize;
            if n > 0 {
                if view.sel >= top + visible {
                    top = view.sel + 1 - visible;
                }
                if view.sel < top {
                    top = view.sel;
                }
            }
            if n == 0 {
                out.push(Line::from(Span::styled(
                    match view.kind {
                        ReportKind::Quota => "  (no quota providers — f to refresh sources)",
                        _ => "  (no comparisons)",
                    },
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                for i in 0..visible.min(n.saturating_sub(top)) {
                    let item = &view.items[indices[top + i]];
                    let is_sel = top + i == view.sel;
                    let style = if is_sel {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let prefix = if is_sel { "❯ " } else { "  " };
                    let mut spans = vec![
                        Span::styled(prefix, style),
                        Span::styled(sanitize_display(&item.label), style),
                    ];
                    if !item.detail.is_empty() {
                        let used = prefix.chars().count()
                            + UnicodeWidthStr::width(item.label.as_str())
                            + 2;
                        if used < width {
                            spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
                            spans.push(Span::styled(
                                clip(&sanitize_display(&item.detail), width.saturating_sub(used)),
                                Style::default().add_modifier(Modifier::DIM),
                            ));
                        }
                    }
                    out.push(Line::from(spans));
                }
                if n > visible {
                    out.push(Line::from(Span::styled(
                        format!("  {}/{}", view.sel + 1, n),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
            }
        } else {
            out.push(Line::from(Span::styled(
                match view.kind {
                    ReportKind::Quota => "  no quota providers reported",
                    ReportKind::Divergence => "  no divergence data",
                    _ => "  (no data)",
                },
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    } else {
        // Text report (usage / cost): loading / error / body.
        let body: Vec<String> = if view.loading && view.payload.is_none() {
            vec!["loading…".to_string()]
        } else if let Some(err) = &view.error {
            vec![
                format!("error: {}", err),
                String::new(),
                "r — retry · esc — back".to_string(),
            ]
        } else if let Some(payload) = &view.payload {
            usage_report_lines(payload, view.kind == ReportKind::Cost)
        } else {
            vec!["no usage data — run `rebuildup.conversations index` first".to_string()]
        };
        // Wrap each source line into display rows (bounded).
        let mut wrapped: Vec<String> = Vec::new();
        for l in &body {
            for wl in wrap_plain(l, width.saturating_sub(2).max(10)) {
                wrapped.push(wl);
            }
            if wrapped.len() > 2000 {
                break;
            }
        }
        let body_h = height.saturating_sub(2).max(1);
        let max_scroll = wrapped.len().saturating_sub(body_h);
        view.scroll = view.scroll.min(max_scroll);
        for v in wrapped
            .iter()
            .skip(view.scroll)
            .take(body_h)
        {
            out.push(Line::from(clip(v, width)));
        }
    }

    while out.len() < height {
        out.push(Line::from(""));
    }
    out.truncate(height.max(1));
    frame.render_widget(Paragraph::new(out), area);
}

// --- entrypoint ------------------------------------------------------------------------

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
                "usage: failed to open TUI pane (herdr {}): {}",
                bin, detail
            );
            1
        }
        Err(e) => {
            eprintln!("usage: failed to open TUI pane (herdr {}): {}", bin, e);
            1
        }
    }
}

const NON_TTY_NOTICE: [&str; 2] = [
    "rebuildup.usage TUI requires an interactive terminal.",
    "CLI equivalent: usage <capability|usage|quota|cost|divergence>",
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

    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("usage tui failed: could not enable raw mode");
        return 1;
    }
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("usage tui failed: {}", err);
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
            if event::poll(Duration::from_millis(50))? {
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
            let _ = writeln!(io::stderr(), "usage TUI closed");
            0
        }
        Err(err) => {
            eprintln!("usage tui failed: {}", err);
            1
        }
    }
}

/// `usage tui [main|--open]` entrypoint; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    tui_main(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_open_args_opens_main_as_tab() {
        let args = pane_open_args();
        assert!(args.iter().any(|a| a == "rebuildup.usage"));
        let ep = args.iter().position(|a| a == "--entrypoint").unwrap();
        assert_eq!(args[ep + 1], "main");
        let pl = args.iter().position(|a| a == "--placement").unwrap();
        assert_eq!(args[pl + 1], "tab");
    }

    #[test]
    fn window_and_provider_args() {
        assert!(window_args(WindowPreset::All).is_empty());
        for w in [WindowPreset::H24, WindowPreset::D7, WindowPreset::D30] {
            let args = window_args(w);
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "--from");
            // ISO-8601 with Z suffix, parseable by chrono.
            assert!(args[1].ends_with('Z'));
            assert!(chrono::DateTime::parse_from_rfc3339(&args[1]).is_ok());
        }
        assert!(provider_args("").is_empty());
        assert_eq!(provider_args("claude"), vec!["--provider", "claude"]);
        assert_eq!(WindowPreset::All.label(), "all time");
        assert_eq!(WindowPreset::D7.label(), "last 7d");
    }

    #[test]
    fn providers_in_payload_collects_and_sorts_ids() {
        let ids = providers_in_payload(&json!({
            "breakdowns": {"by_provider": [{"key": "claude"}, {"key": "codex"}]},
            "coverage": {"providers": [{"provider": "opencode"}]},
            "providers": [{"provider": "claude"}],
            "comparisons": [{"provider": "codex"}],
        }));
        assert_eq!(ids, vec!["claude", "codex", "opencode"]);
        assert!(providers_in_payload(&Value::Null).is_empty());
    }

    #[test]
    fn next_provider_cycles_all_ids_all() {
        let ids = vec!["claude".to_string(), "codex".to_string()];
        assert_eq!(next_provider("", &ids), "claude");
        assert_eq!(next_provider("claude", &ids), "codex");
        assert_eq!(next_provider("codex", &ids), "");
        assert_eq!(next_provider("claude", &[]), "");
    }

    #[test]
    fn redact_secrets_drops_credential_keys_recursively() {
        let out = redact_secrets(&json!({
            "provider": "claude",
            "api_key": "sk-secret",
            "nested": {"access_token": "t", "plan": "pro"},
            "list": [{"password": "p", "ok": 1}],
        }));
        let o = out.as_object().unwrap();
        assert_eq!(o.get("provider").unwrap(), &json!("claude"));
        assert!(!o.contains_key("api_key"));
        let nested = o.get("nested").unwrap().as_object().unwrap();
        assert!(!nested.contains_key("access_token"));
        assert_eq!(nested.get("plan").unwrap(), &json!("pro"));
        let list = o.get("list").unwrap().as_array().unwrap();
        let first = list[0].as_object().unwrap();
        assert!(!first.contains_key("password"));
        assert_eq!(first.get("ok").unwrap(), &json!(1));
    }

    #[test]
    fn json_lines_applies_redaction_and_bounds() {
        let lines = json_lines(&json!({"token": "x", "keep": 1}), 400);
        let joined = lines.join("\n");
        assert!(!joined.contains("\"token\""));
        assert!(joined.contains("\"keep\": 1"));
        let many: Vec<Value> = (0..500).map(|i| json!(i)).collect();
        let bounded = json_lines(&json!({"items": many}), 10);
        assert!(bounded.len() <= 11);
        assert!(bounded.last().unwrap().contains("more lines"));
    }

    #[test]
    fn quota_items_maps_providers_and_hub_accounts() {
        let items = quota_items(&json!({
            "providers": [
                {"provider": "claude", "status": "ok", "stale": true, "detail": null,
                 "windows": [{"id": "five_hour", "label": "5h", "used_percent": 42.4}]},
                {"provider": "opencode", "status": "unsupported", "windows": []},
            ],
            "hubs": [
                {"id": "hub1", "accounts": [
                    {"account": "a1", "provider": "claude", "status": "ok", "windows": []},
                ]},
                {"id": "hub2", "accounts": []},
            ],
        }));
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].label, "claude");
        assert!(items[0].detail.contains("42%"));
        assert!(items[0].detail.contains("STALE"));
        assert!(items[1].detail.contains("unsupported"));
        assert_eq!(items[2].label, "hub1 · a1");
        assert_eq!(items[3].label, "hub2");
        assert!(items[3].detail.contains("hub status"));
    }

    #[test]
    fn divergence_items_maps_comparisons_with_both_sides() {
        let items = divergence_items(&json!({
            "comparisons": [{
                "provider": "claude",
                "verdict": "different_units",
                "detail": null,
                "quota": {"status": "ok"},
                "measured": {"total_tokens": 1234},
            }],
        }));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "claude");
        assert!(items[0].detail.contains("different_units"));
        assert!(items[0].detail.contains("total_tokens=1,234"));
    }

    #[test]
    fn wrap_plain_wraps_long_lines_keeps_short() {
        assert_eq!(wrap_plain("short", 20), vec!["short".to_string()]);
        let wrapped = wrap_plain("aaa bbb ccc ddd eee fff ggg", 10);
        assert!(wrapped.len() > 1);
        for l in &wrapped {
            assert!(display_width(l) <= 10, "line too wide: {:?}", l);
        }
    }

    #[test]
    fn poll_load_times_out_a_stalled_load() {
        // A load older than LOAD_TIMEOUT reports an honest timeout and
        // clears the loading state; the detached worker's late result
        // is discarded (rx dropped).
        let mut view = ReportView {
            kind: ReportKind::Usage,
            windowed: true,
            refreshable: false,
            provider: String::new(),
            window: WindowPreset::All,
            provider_ids: Vec::new(),
            payload: None,
            error: None,
            loading: true,
            rx: None,
            load_started: Some(Instant::now() - LOAD_TIMEOUT - Duration::from_secs(1)),
            scroll: 0,
            sel: 0,
            query: String::new(),
            filter_mode: false,
            items: Vec::new(),
        };
        view.poll_load();
        assert!(!view.loading);
        assert_eq!(view.error.as_deref(), Some("load timed out"));
        assert!(view.payload.is_none());
    }

    #[test]
    fn poll_load_keeps_waiting_within_the_deadline() {
        let mut view = ReportView {
            kind: ReportKind::Usage,
            windowed: true,
            refreshable: false,
            provider: String::new(),
            window: WindowPreset::All,
            provider_ids: Vec::new(),
            payload: None,
            error: None,
            loading: true,
            rx: None,
            load_started: Some(Instant::now()),
            scroll: 0,
            sel: 0,
            query: String::new(),
            filter_mode: false,
            items: Vec::new(),
        };
        view.poll_load();
        assert!(view.loading);
        assert!(view.error.is_none());
    }
}
