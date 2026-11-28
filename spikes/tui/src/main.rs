//! SPIKE for Issue #289 — minimal native TUI prototype.
//!
//! Deliberately small: list + search/filter + detail + confirm dialog +
//! one cancellable async operation, on ratatui/crossterm. Purpose is to
//! exercise the capability set required by docs/contracts/tui-v1.md and
//! the program #286 TUI section, not to design the Inbox screens (#291).
//!
//! Evidence hooks:
//!   SPIKE_STATS=/path   writes render/latency counters on exit
//!   Non-TTY stdout       prints a plain list and exits (honest fallback)

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Terminal;

// ---------------------------------------------------------------------------
// Fixture data: deliberately hostile strings for width/Unicode exercise.
// ---------------------------------------------------------------------------

struct Item {
    id: &'static str,
    title: String,
    body: String,
}

fn fixtures() -> Vec<Item> {
    vec![
        Item { id: "inbox-1", title: "レビュー依頼: Rust TUI スパイク (#289)".into(),
               body: "CJK 確認用。日本語のタイトルと本文。絵文字 🚀 mixed with ASCII.".into() },
        Item { id: "inbox-2", title: "café au lait (combining)".into(),
               body: "Combining char: cafe\u{301} vs precomposed café. Width must stay 1 cell.".into() },
        Item { id: "inbox-3", title: "Ambiguous width: ・αβγ・".into(),
               body: "Greek + katakana middle dot are ambiguous-width; box chars ┌─┐ │ └─┘ too.".into() },
        Item { id: "inbox-4", title: "Emoji ZWJ: 👨‍👩‍👧‍👦 and flags 🇯🇵".into(),
               body: "Family ZWJ sequence and regional-indicator flag must render as single glyphs.".into() },
        Item { id: "inbox-5", title: "Long line for truncation/scroll".into(),
               body: format!("{} tail-end-marker", "x".repeat(500)) },
        Item { id: "inbox-6", title: "Escape probe </span>\u{1b}[31mRED\u{1b}[0m".into(),
               body: "Untrusted text must not inject CSI. If 'RED' is literal text, sanitize works.".into() },
        Item { id: "inbox-7", title: "キーボード操作の確認".into(),
               body: "j/k 移動、Enter 詳細、/ 検索、r 非同期更新、d 削除確認、q 終了。".into() },
        Item { id: "inbox-8", title: "マウス: click select, wheel scroll".into(),
               body: "SGR mouse events should select rows and scroll the detail pane.".into() },
    ]
}

// ---------------------------------------------------------------------------
// Terminal guard: restore on every exit path incl. panic.
// ---------------------------------------------------------------------------

struct TermGuard {
    stats_path: Option<String>,
    stats: Arc<Mutex<Stats>>,
}

#[derive(Default)]
struct Stats {
    frames: u64,
    draw_total_us: u128,
    draw_max_us: u128,
    draw_us_samples: Vec<u128>,
    key_to_draw_us: Vec<u128>,
    started: Option<Instant>,
    first_frame_us: Option<u128>,
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste, DisableMouseCapture);
        if let Some(path) = &self.stats_path {
            if let Ok(s) = self.stats.lock() {
                if let Ok(file) = std::fs::File::create(path) {
                    let mut f = io::BufWriter::new(file);
                    let _ = writeln!(f, "{}", s.render());
                }
            }
        }
    }
}

impl Stats {
    fn render(&self) -> String {
        let mut v = self.key_to_draw_us.clone();
        v.sort_unstable();
        let pct = |p: f64| -> u128 {
            if v.is_empty() { return 0; }
            let i = ((v.len() as f64 - 1.0) * p / 100.0).round() as usize;
            v[i.min(v.len() - 1)]
        };
        let mut dv = self.draw_us_samples.clone();
        dv.sort_unstable();
        let dpct = |p: f64| -> u128 {
            if dv.is_empty() { return 0; }
            let i = ((dv.len() as f64 - 1.0) * p / 100.0).round() as usize;
            dv[i.min(dv.len() - 1)]
        };
        format!(
            "frames={} first_frame_us={:?} draw_total_us={} draw_avg_us={} draw_p50={}us draw_p95={}us draw_max_us={} key_to_draw p50={}us p95={}us p99={}us max={}us samples={}",
            self.frames,
            self.first_frame_us,
            self.draw_total_us,
            if self.frames > 0 { self.draw_total_us / self.frames as u128 } else { 0 },
            dpct(50.0), dpct(95.0),
            self.draw_max_us,
            pct(50.0), pct(95.0), pct(99.0),
            v.last().copied().unwrap_or(0),
            v.len(),
        )
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum View {
    List,
    Detail,
    Confirm,
    Help,
}

struct AsyncOp {
    cancel: Arc<AtomicBool>,
}

struct App {
    items: Vec<Item>,
    view: View,
    selected: usize,
    filter: String,
    filter_focused: bool,
    detail_scroll: u16,
    op: Option<AsyncOp>,
    op_rx: Option<std::sync::mpsc::Receiver<String>>,
    status: String,
    notice: Option<String>,
}

impl App {
    fn filtered(&self) -> Vec<usize> {
        let q = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| q.is_empty() || it.title.to_lowercase().contains(&q) || it.body.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn selected_item(&self) -> Option<&Item> {
        self.filtered().get(self.selected).and_then(|&i| self.items.get(i))
    }
}

fn sanitize(s: &str) -> String {
    // Strip C0/C1 controls and ESC so untrusted strings cannot inject CSI/OSC.
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status line
            Constraint::Min(3),    // content
            Constraint::Length(1), // help line
        ])
        .split(area);

    // status line
    let status = match app.view {
        View::List => format!(" {} [{}] {}", app.status, app.view_name(), app.notice.clone().unwrap_or_default()),
        _ => format!(" {} [{}] {}", app.status, app.view_name(), app.notice.clone().unwrap_or_default()),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(sanitize(&status), Style::default().add_modifier(Modifier::BOLD)))),
        chunks[0],
    );

    match app.view {
        View::List => draw_list(frame, app, chunks[1]),
        View::Detail => draw_detail(frame, app, chunks[1]),
        View::Confirm => {
            draw_detail(frame, app, chunks[1]);
            let popup = centered_rect(50, 30, area);
            frame.render_widget(Clear, popup);
            let confirm = Paragraph::new(vec![
                Line::from("Delete item?"),
                Line::from(""),
                Line::from(format!("Target: {}", app.selected_item().map(|i| i.id).unwrap_or("?"))),
                Line::from("Consequence: permanent removal (spike demo)"),
                Line::from(""),
                Line::from(vec![
                    Span::styled(" y ", Style::default().bg(Color::DarkGray)),
                    Span::raw(" confirm   "),
                    Span::styled(" n/Esc ", Style::default().bg(Color::DarkGray)),
                    Span::raw(" cancel"),
                ]),
            ])
            .block(Block::default().borders(Borders::ALL).title("Confirm"));
            frame.render_widget(confirm, popup);
        }
        View::Help => {
            let help = Paragraph::new(vec![
                Line::from("Keys: j/k or ↑/↓ move   Enter detail   / filter   r async refresh   d delete(confirm)"),
                Line::from("      Esc/q back/close   ? help   Ctrl-C cancel op / exit   mouse: click select, wheel scroll"),
                Line::from("IME: composition is handled by the terminal/OS; this app only sees committed UTF-8."),
            ])
            .block(Block::default().borders(Borders::ALL).title("Help"));
            frame.render_widget(help, chunks[1]);
        }
    }

    let help = Paragraph::new("? help  / filter  r refresh  d delete  Enter detail  q quit");
    frame.render_widget(help, chunks[2]);
}

impl App {
    fn view_name(&self) -> &'static str {
        match self.view {
            View::List => "LIST",
            View::Detail => "DETAIL",
            View::Confirm => "CONFIRM",
            View::Help => "HELP",
        }
    }
}

fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let pw = area.width * pct_x / 100;
    let ph = area.height * pct_y / 100;
    let x = area.x + (area.width.saturating_sub(pw)) / 2;
    let y = area.y + (area.height.saturating_sub(ph)) / 2;
    Rect::new(x, y, pw, ph)
}

fn draw_list(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let filtered = app.filtered();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(row, &idx)| {
            let it = &app.items[idx];
            let style = if row == app.selected {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {:<10} ", it.id), Style::default().fg(Color::Cyan)),
                Span::styled(sanitize(&it.title), style),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Inbox spike — {} items (filtered {}) ", app.items.len(), filtered.len())))
        .highlight_style(Style::default().bg(Color::DarkGray));
    frame.render_stateful_widget(list, area, &mut state);

    // filter input line at bottom of the list block
    if app.filter_focused {
        let w = area.width.saturating_sub(4).max(10);
        let input_area = Rect::new(area.x + 2, area.y + area.height.saturating_sub(2), w, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/ ", Style::default().fg(Color::Yellow)),
                Span::raw(sanitize(&app.filter)),
                Span::styled("▌", Style::default().fg(Color::Yellow)),
            ]))
            .style(Style::default().bg(Color::Black)),
            input_area,
        );
    }
}

fn draw_detail(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(match app.view {
            View::Confirm => " Detail ",
            _ => " Detail ",
        });
    let content = inner.inner(area);
    frame.render_widget(inner, area);

    if let Some(item) = app.selected_item() {
        let text = format!("{}\n\n{}", item.title, item.body);
        let para = Paragraph::new(sanitize(&text))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0));
        frame.render_widget(para, content);

        let mut scrollbar_state = ScrollbarState::default()
            .content_length(text.lines().count())
            .position(app.detail_scroll as usize)
            .viewport_content_length(content.height as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            content,
            &mut scrollbar_state,
        );
    } else {
        frame.render_widget(Paragraph::new("(empty)"), content);
    }
}

// ---------------------------------------------------------------------------
// Async op: fake refresh with progress + cancel, polled via channel.
// ---------------------------------------------------------------------------

fn start_refresh(app: &mut App) {
    if app.op.is_some() {
        app.status = "op already running".into();
        return;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let cancel2 = cancel.clone();
    std::thread::spawn(move || {
        for i in 1..=10 {
            if cancel2.load(Ordering::Relaxed) {
                let _ = tx.send("cancelled".into());
                return;
            }
            std::thread::sleep(Duration::from_millis(120));
            let _ = tx.send(format!("refresh {i}/10"));
        }
        let _ = tx.send("done".into());
    });
    app.op = Some(AsyncOp { cancel });
    app.op_rx = Some(rx);
    app.status = "refreshing…".into();
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    // returns true => quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.op.is_some() {
            if let Some(op) = &app.op { op.cancel.store(true, Ordering::Relaxed); }
            app.status = "cancelling…".into();
            return false;
        }
        return true;
    }

    if app.filter_focused {
        match key.code {
            KeyCode::Esc => { app.filter_focused = false; app.filter.clear(); app.status.clear(); }
            KeyCode::Enter => { app.filter_focused = false; app.selected = 0; }
            KeyCode::Backspace => { app.filter.pop(); app.selected = 0; }
            KeyCode::Char(c) => { app.filter.push(c); app.selected = 0; }
            _ => {}
        }
        return false;
    }

    match app.view {
        View::List => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('?') => app.view = View::Help,
            KeyCode::Char('/') => { app.filter_focused = true; app.filter.clear(); }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = app.filtered().len();
                if n > 0 { app.selected = (app.selected + 1).min(n - 1); }
            }
            KeyCode::Char('k') | KeyCode::Up => { app.selected = app.selected.saturating_sub(1); }
            KeyCode::Char('g') => app.selected = 0,
            KeyCode::Char('G') => { let n = app.filtered().len(); if n > 0 { app.selected = n - 1; } }
            KeyCode::Enter => {
                if app.selected_item().is_some() {
                    app.view = View::Detail;
                    app.detail_scroll = 0;
                }
            }
            KeyCode::Char('d') => app.view = View::Confirm,
            KeyCode::Char('r') => start_refresh(app),
            _ => {}
        },
        View::Detail | View::Help => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => { app.view = View::List; app.status.clear(); }
            KeyCode::Char('?') => app.view = View::List,
            KeyCode::Down | KeyCode::Char('j') => app.detail_scroll = app.detail_scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => app.detail_scroll = app.detail_scroll.saturating_sub(1),
            _ => {}
        },
        View::Confirm => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(item) = app.selected_item() {
                    app.notice = Some(format!("deleted {}", item.id));
                    // spike: just mark, don't actually remove
                }
                app.view = View::Detail;
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Enter => app.view = View::Detail,
            _ => {}
        },
    }
    false
}

fn handle_mouse(app: &mut App, m: MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollUp => {
            match app.view {
                View::Detail => app.detail_scroll = app.detail_scroll.saturating_sub(3),
                View::List => app.selected = app.selected.saturating_sub(1),
                _ => {}
            }
        }
        MouseEventKind::ScrollDown => {
            match app.view {
                View::Detail => app.detail_scroll = app.detail_scroll.saturating_add(3),
                View::List => {
                    let n = app.filtered().len();
                    if n > 0 { app.selected = (app.selected + 1).min(n - 1); }
                }
                _ => {}
            }
        }
        MouseEventKind::Down(event::MouseButton::Left) if app.view == View::List => {
            // rows start at area.y+1 (border) + block title offset ≈ +2; approximate
            let row = m.row.saturating_sub(2) as usize;
            let n = app.filtered().len();
            if row < n { app.selected = row; }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> io::Result<()> {
    let stats_path = std::env::var("SPIKE_STATS").ok();
    let stats = Arc::new(Mutex::new(Stats { started: Some(Instant::now()), ..Default::default() }));

    // Honest non-TTY fallback: plain list, no fake TUI.
    if !io::stdout().is_terminal() {
        for it in fixtures() {
            println!("{}\t{}", it.id, it.title);
        }
        return Ok(());
    }

    // Panic safety: restore terminal even on crash.
    let stats_for_panic = stats.clone();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste, DisableMouseCapture);
        prev_hook(info);
    }));
    let _ = stats_for_panic;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let _guard = TermGuard { stats_path, stats: stats.clone() };

    let mut app = App {
        items: fixtures(),
        view: View::List,
        selected: 0,
        filter: String::new(),
        filter_focused: false,
        detail_scroll: 0,
        op: None,
        op_rx: None,
        status: String::new(),
        notice: None,
    };
    app.status = "ready".into();

    let mut last_key: Option<Instant> = None;

    loop {
        // Pump async-op channel before drawing.
        let mut msgs = Vec::new();
        if let Some(rx) = &app.op_rx {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            if msg == "done" {
                app.status = "refresh complete".into();
                app.op = None;
                app.op_rx = None;
            } else if msg == "cancelled" {
                app.status = "refresh cancelled".into();
                app.op = None;
                app.op_rx = None;
            } else {
                app.status = msg;
            }
        }

        let t0 = Instant::now();
        terminal.draw(|f| draw(f, &mut app))?;
        let draw_done = Instant::now();
        let draw_us = t0.elapsed().as_micros();
        {
            let mut s = stats.lock().unwrap();
            s.frames += 1;
            s.draw_total_us += draw_us;
            s.draw_max_us = s.draw_max_us.max(draw_us);
            s.draw_us_samples.push(draw_us);
            if s.first_frame_us.is_none() {
                if let Some(st) = s.started { s.first_frame_us = Some(st.elapsed().as_micros()); }
            }
            if let Some(k) = last_key.take() {
                s.key_to_draw_us.push(draw_done.duration_since(k).as_micros());
            }
        }

        // Event-driven: block until input when idle; poll at 50 ms only
        // while an async op needs progress repaints.
        let timeout = if app.op.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(3600)
        };
        if !event::poll(timeout)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => {
                last_key = Some(Instant::now());
                if handle_key(&mut app, key) { break; }
            }
            Event::Mouse(m) => { last_key = Some(Instant::now()); handle_mouse(&mut app, m); }
            Event::Paste(text) => {
                // Bracketed paste: insert as filter text when focused.
                if app.filter_focused {
                    app.filter.push_str(&sanitize(&text));
                    app.selected = 0;
                }
            }
            Event::Resize(_, _) => { /* ratatui re-queries size each draw */ }
            _ => {}
        }
    }

    Ok(())
}
