//! Interactive TUI surfaces (`git tui [main|forge] [--open]`).
//!
//! `main` — repository status surface. Rust port of
//! `plugins/git/src/tui.ts` + `tui/app.ts` (contract herdr-plugins.tui/v1,
//! ADR-0031/0033, Issue #238).
//!
//! Design rules honored here (same as the TS surface):
//! - target identity pinned at launch (origin pane/cwd + repo root +
//!   HEAD sha); `HERDR_PANE_ID` is never used as the git target (it is
//!   the UI pane itself, tui-v1 §3.2);
//! - read-only open: the home view performs reads only;
//! - every mutation revalidates pane→cwd→repo-root and HEAD sha first —
//!   a moved HEAD or re-rooted pane aborts with a stale_target state and
//!   a refresh affordance, never a silent retarget (tui-v1 §3.3);
//! - mutations (stage/unstage/commit/push/fetch/branch ops) go through a
//!   confirm dialog with the exact target + consequence; bulk
//!   stage/unstage and push are always confirmed;
//! - long ops run on a worker thread; the event loop stays responsive
//!   and Esc cancels the in-flight git child via a cooperative cancel
//!   flag checked by the spawn wait loops (tui-v1 TaskRunner semantics);
//! - honest states for not-a-repo / bare / detached / unborn / no
//!   upstream / missing remote — never fabricated success.
//!
//! `forge` — GitHub PR/issue browser (Issue #239), ported in
//! `tui_forge.rs`.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use hoxay_herdr_client::commands::{self, CallOptions};
use hoxay_herdr_client::snapshot::SessionSnapshot;
use hoxay_tui::safety::{bound_text, sanitize_display};

use crate::git::{
    self, BranchEntry, DiffRequest, FileChange, FileChangeKind, LogCommit, RemoteInfo, RunOptions,
    StatusReport,
};
use crate::tui_model::{
    self, branch_label, commit_label, describe_error, diff_scope_for, file_detail, file_label,
    format_branch_line, format_change_counts, short_sha7, FileDiffScope, HonestError, PinnedTarget,
    TuiError, TuiTargetSpec,
};

const GIT_TIMEOUT_MS: u64 = 10_000;
const MUTATION_TIMEOUT_MS: u64 = 60_000;
const DIFF_MAX_BYTES: usize = 256 * 1024;

fn git_opts(timeout_ms: u64) -> RunOptions {
    RunOptions {
        timeout_ms: Some(timeout_ms),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Target resolution (port of resolvePinnedTarget / assertTargetFresh)
// ---------------------------------------------------------------------------

fn pane_cwd_of(snapshot: &SessionSnapshot, pane_id: &str) -> Result<String, TuiError> {
    let pane = snapshot
        .panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .ok_or_else(|| TuiError::Kind {
            kind: "pane_not_found",
            message: format!("pane not found in session snapshot: {pane_id}"),
        })?;
    pane.cwd
        .clone()
        .or_else(|| {
            pane.extra
                .get("foreground_cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TuiError::Kind {
            kind: "no_pane_cwd",
            message: format!("pane has no cwd in session snapshot: {pane_id}"),
        })
}

fn same_dir(a: &str, b: &str) -> bool {
    let canon = |p: &str| -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| Path::new(p).to_path_buf())
    };
    canon(a) == canon(b)
}

/// Resolve the pinned git target: pane (via session snapshot) or explicit
/// cwd → repo root → HEAD sha + branch name. Errors classify via
/// `describe_error` (TuiTargetError kinds, GitError kinds,
/// TargetResolutionError codes).
pub fn resolve_pinned_target(
    spec: &TuiTargetSpec,
    call: &CallOptions,
) -> Result<PinnedTarget, TuiError> {
    let mut cwd = spec.cwd.clone();
    if cwd.is_none() {
        if let Some(pane_id) = &spec.pane_id {
            let snapshot = commands::session_snapshot(call)?;
            cwd = Some(pane_cwd_of(&snapshot, pane_id)?);
        }
    }
    let Some(cwd) = cwd else {
        return Err(TuiError::Kind {
            kind: "target_required",
            message: "no target: launch from a pane context or pass --cwd <path>".to_string(),
        });
    };
    let repo = git::resolve_repo(Path::new(&cwd), &git_opts(GIT_TIMEOUT_MS))?;
    let head_sha = git::head_sha_or_null(Path::new(&repo.root), &git_opts(GIT_TIMEOUT_MS));
    let branch_name = git::current_branch_name(Path::new(&repo.root), &git_opts(GIT_TIMEOUT_MS));
    let detached = branch_name.as_deref() == Some("HEAD");
    Ok(PinnedTarget {
        pane_id: spec.pane_id.clone(),
        workspace_id: spec.workspace_id.clone(),
        cwd,
        repo_root: repo.root,
        head_sha,
        branch_name: if detached || branch_name.is_none() {
            None
        } else {
            branch_name
        },
        detached,
    })
}

/// Pre-mutation revalidation (tui-v1 §3.3): the pinned pane must still
/// resolve to the same repo root and HEAD must not have moved. Any drift
/// returns a stale error — callers abort with refresh, never retarget.
pub fn assert_target_fresh(target: &PinnedTarget, call: &CallOptions) -> Result<(), TuiError> {
    if let Some(pane_id) = &target.pane_id {
        let mut c = call.clone();
        c.mutation = true;
        let snapshot = commands::session_snapshot(&c)?;
        let cwd = pane_cwd_of(&snapshot, pane_id)?;
        let repo = git::resolve_repo(Path::new(&cwd), &git_opts(GIT_TIMEOUT_MS))?;
        if !same_dir(&repo.root, &target.repo_root) {
            return Err(tui_model::stale_target(format!(
                "stale target: pane {pane_id} now resolves to {} (was {}); reopen the surface",
                repo.root, target.repo_root
            )));
        }
    }
    let sha = git::head_sha_or_null(Path::new(&target.repo_root), &git_opts(GIT_TIMEOUT_MS));
    if sha != target.head_sha {
        return Err(tui_model::stale_target(format!(
            "stale target: HEAD moved {} -> {}; refresh before mutating",
            target
                .head_sha
                .as_deref()
                .map(short_sha7)
                .unwrap_or_else(|| "(unborn)".to_string()),
            sha.as_deref()
                .map(short_sha7)
                .unwrap_or_else(|| "(unborn)".to_string()),
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// View model types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeAction {
    Files,
    Diff,
    Log,
    Compare,
    Branches,
    Remotes,
    StageAll,
    UnstageAll,
    Commit,
    Fetch,
    Push,
    Upstream,
    Refresh,
}

const HOME_ITEMS: &[(HomeAction, &str, &str)] = &[
    (
        HomeAction::Files,
        "Changed files",
        "per-file stage / unstage / diff",
    ),
    (
        HomeAction::Diff,
        "Diff",
        "worktree / staged / HEAD patch viewer",
    ),
    (HomeAction::Log, "Commit history", "log + per-commit show"),
    (
        HomeAction::Compare,
        "Compare",
        "branch vs upstream (merge-base diff)",
    ),
    (
        HomeAction::Branches,
        "Branches",
        "list / switch / create / set upstream",
    ),
    (
        HomeAction::Remotes,
        "Remotes",
        "fetch/push URLs (credentials stripped)",
    ),
    (
        HomeAction::StageAll,
        "Stage all changes",
        "git add -A (confirmed)",
    ),
    (
        HomeAction::UnstageAll,
        "Unstage all changes",
        "git reset (confirmed)",
    ),
    (HomeAction::Commit, "Commit…", "message form + confirm"),
    (HomeAction::Fetch, "Fetch…", "pick remote, run git fetch"),
    (HomeAction::Push, "Push…", "push current branch (confirmed)"),
    (
        HomeAction::Upstream,
        "Set upstream…",
        "branch --set-upstream-to",
    ),
    (HomeAction::Refresh, "Refresh", "re-read status (r)"),
];

#[derive(Debug, Clone)]
enum ListValue {
    Home(HomeAction),
    File(FileChange),
    Commit(LogCommit),
    Branch(BranchEntry),
}

#[derive(Debug, Clone)]
struct ListItem {
    label: String,
    detail: String,
    disabled: bool,
    disabled_reason: Option<String>,
    value: ListValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Home,
    Files,
    Log,
    Branches,
}

#[derive(Debug, Clone)]
struct ListScreen {
    kind: ListKind,
    items: Vec<ListItem>,
    cursor: usize,
    scroll_top: usize,
    filter_active: bool,
    filter_draft: String,
    committed_filter: String,
}

impl ListScreen {
    fn new(kind: ListKind, items: Vec<ListItem>) -> Self {
        ListScreen {
            kind,
            items,
            cursor: 0,
            scroll_top: 0,
            filter_active: false,
            filter_draft: String::new(),
            committed_filter: String::new(),
        }
    }

    /// Case-insensitive AND-of-terms substring filter over
    /// "label detail" (port of filterItems).
    fn filtered(&self) -> Vec<usize> {
        let q = if self.filter_active {
            self.filter_draft.clone()
        } else {
            self.committed_filter.clone()
        };
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        let terms: Vec<&str> = q.split_whitespace().collect();
        (0..self.items.len())
            .filter(|&i| {
                let item = &self.items[i];
                let hay = format!("{} {}", item.label, item.detail).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .collect()
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

    /// Cursor move skipping disabled items (port of SelectList.move).
    fn move_cursor(&mut self, delta: isize) {
        let list = self.filtered();
        if list.is_empty() {
            return;
        }
        let n = list.len();
        let mut i = self.cursor;
        for _ in 0..n {
            i = ((i as isize + delta).rem_euclid(n as isize)) as usize;
            if !self.items[list[i]].disabled {
                break;
            }
        }
        self.cursor = i;
    }

    fn current(&self) -> Option<&ListItem> {
        self.filtered().get(self.cursor).map(|&i| &self.items[i])
    }

    fn set_items(&mut self, items: Vec<ListItem>) {
        self.items = items;
        self.cursor = 0;
        self.scroll_top = 0;
        self.clamp_cursor();
    }
}

// --- pager ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffCycle {
    Worktree,
    Staged,
    Head,
}

impl DiffCycle {
    fn next(self) -> Self {
        match self {
            DiffCycle::Worktree => DiffCycle::Staged,
            DiffCycle::Staged => DiffCycle::Head,
            DiffCycle::Head => DiffCycle::Worktree,
        }
    }
    fn label(self) -> &'static str {
        match self {
            DiffCycle::Worktree => "worktree",
            DiffCycle::Staged => "staged",
            DiffCycle::Head => "HEAD",
        }
    }
    /// File-diff scopes use lowercase names in titles.
    fn file_label(self) -> &'static str {
        match self {
            DiffCycle::Worktree => "worktree",
            DiffCycle::Staged => "staged",
            DiffCycle::Head => "head",
        }
    }
}

/// What a pager loads. Mutable per-screen state (diff scope, compare
/// base) lives beside it so `r`/`tab`/`b` reloads reuse the spec.
#[derive(Debug, Clone)]
enum PagerLoad {
    /// `diff` for one file; scope cycles worktree/staged/head.
    FileDiff {
        file: FileChange,
        scope: DiffCycle,
    },
    /// Whole-diff pager; scope cycles worktree/staged/HEAD.
    Diff {
        scope: DiffCycle,
    },
    CommitShow {
        sha: String,
        subject: String,
    },
    Compare {
        base: Option<String>,
    },
    Remotes,
    /// Precomputed lines (notices, honest errors, no-diff messages).
    Static {
        lines: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct PagerScreen {
    id: u64,
    load: PagerLoad,
    /// Title for static pagers (error pages, notices); dynamic loads
    /// compute their title from the load spec.
    static_title: Option<String>,
    lines: Vec<String>,
    /// Honest error block rendered instead of lines (title + body +
    /// optional CLI fallback), mirroring PagerScreen.error.
    error: Option<HonestBlock>,
    loading: bool,
    scroll: usize,
}

#[derive(Debug, Clone)]
struct HonestBlock {
    title: String,
    lines: Vec<String>,
}

impl PagerScreen {
    fn title(&self) -> String {
        if let Some(t) = &self.static_title {
            return t.clone();
        }
        match &self.load {
            PagerLoad::FileDiff { file, scope } => {
                let path = match &file.old_path {
                    Some(old) => format!("{} -> {}", old, file.path),
                    None => file.path.clone(),
                };
                format!("diff ({}) — {}", scope.file_label(), path)
            }
            PagerLoad::Diff { scope } => format!("diff — {}", scope.label()),
            PagerLoad::CommitShow { sha, subject } => {
                format!("commit {} — {}", short_sha7(sha), subject)
            }
            PagerLoad::Compare { base } => format!(
                "compare — {}",
                base.clone()
                    .unwrap_or_else(|| "upstream (auto)".to_string())
            ),
            PagerLoad::Remotes => "remotes".to_string(),
            PagerLoad::Static { .. } => self.static_title.clone().unwrap_or_default(),
        }
    }

    fn is_diff_scope_pager(&self) -> bool {
        matches!(
            self.load,
            PagerLoad::FileDiff { .. } | PagerLoad::Diff { .. }
        )
    }

    fn is_compare_pager(&self) -> bool {
        matches!(self.load, PagerLoad::Compare { .. })
    }

    fn is_static(&self) -> bool {
        matches!(self.load, PagerLoad::Static { .. })
    }
}

// --- prompts / confirm / select ---------------------------------------------------

#[derive(Debug, Clone)]
enum PromptKind {
    CommitSubject,
    CommitBody,
    BaseRevision,
    BranchName,
    /// `set upstream` for a specific branch.
    Upstream {
        branch: String,
    },
}

#[derive(Debug, Clone)]
struct PromptState {
    kind: PromptKind,
    draft: String,
    placeholder: String,
    label: String,
}

/// A pending confirm-gated mutation (exact target + consequence).
#[derive(Debug, Clone)]
struct PendingMutation {
    title: String,
    target_lines: Vec<String>,
    consequence: String,
    cli_label: String,
    /// What to run on confirm.
    action: MutationAction,
}

/// Mutation bodies are data, not closures — the worker thread replays
/// them through the same domain collectors the CLI uses.
#[derive(Debug, Clone)]
enum MutationAction {
    Stage {
        all: bool,
        paths: Vec<String>,
    },
    Unstage {
        all: bool,
        paths: Vec<String>,
    },
    Commit {
        messages: Vec<String>,
    },
    BranchSwitch {
        name: String,
    },
    BranchCreate {
        name: String,
    },
    SetUpstream {
        upstream: String,
    },
    Fetch {
        remote: String,
    },
    Push {
        remote: Option<String>,
        refspec: Option<String>,
        set_upstream: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectPurpose {
    FetchRemote,
    PushRemote,
}

#[derive(Debug, Clone)]
struct SelectItem {
    label: String,
    detail: String,
    remote: RemoteInfo,
}

#[derive(Debug, Clone)]
struct SelectScreen {
    title: String,
    items: Vec<SelectItem>,
    cursor: usize,
    scroll_top: usize,
    purpose: SelectPurpose,
    /// Branch name for PushRemote (refspec HEAD:<branch>).
    branch: Option<String>,
}

impl SelectScreen {
    fn move_cursor(&mut self, delta: isize) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        self.cursor = ((self.cursor as isize + delta).rem_euclid(n as isize)) as usize;
    }

    fn current(&self) -> Option<&SelectItem> {
        self.items.get(self.cursor)
    }
}

// --- background tasks ---------------------------------------------------------------

/// Result of a background git/domain operation.
#[derive(Debug)]
enum BgResult {
    /// Pager (re)load finished for `pager_id`.
    Pager {
        pager_id: u64,
        result: Result<Vec<String>, TuiError>,
    },
    /// Mutation worker finished (fresh-check + mutation).
    Mutation {
        result: Result<String, TuiError>,
        action: MutationAction,
    },
    /// Status refresh finished.
    Status(Result<StatusReport, TuiError>),
    /// Files list reload finished.
    Files(Result<Vec<FileChange>, TuiError>),
    /// Log list reload finished.
    Log(Result<Vec<LogCommit>, TuiError>),
    /// Branches list reload finished.
    Branches(Result<Vec<BranchEntry>, TuiError>),
    /// Remote list finished (for the fetch/push select screen).
    Remotes(Result<Vec<RemoteInfo>, TuiError>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgKind {
    Pager,
    List,
    Status,
    Mutation,
    RemotesSelect,
}

struct BgTask {
    kind: BgKind,
    label: String,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<BgResult>,
}

// --- screens ------------------------------------------------------------------------

#[derive(Debug)]
enum Screen {
    List(ListScreen),
    Pager(PagerScreen),
    Prompt(PromptState),
    Confirm(PendingMutation),
    Select(SelectScreen),
    Help,
}

// ---------------------------------------------------------------------------
// GitTui controller
// ---------------------------------------------------------------------------

pub struct GitTui {
    target: Option<PinnedTarget>,
    status: Option<StatusReport>,
    /// Transient one-line notice shown on the home screen/status line.
    notice: Option<String>,
    resolve_error: Option<TuiError>,
    last_spec: TuiTargetSpec,
    call: CallOptions,
    stack: Vec<Screen>,
    tasks: Vec<BgTask>,
    next_pager_id: u64,
    /// Commit subject captured by the first prompt, consumed by the body
    /// prompt (the TS flow is a two-step prompt chain).
    pending_commit_subject: Option<String>,
    /// Remote select purpose awaiting `BgResult::Remotes`.
    pending_select: Option<(SelectPurpose, Option<String>)>,
    status_left: String,
    status_right: String,
}

impl GitTui {
    fn new(call: CallOptions, spec: TuiTargetSpec) -> Self {
        GitTui {
            target: None,
            status: None,
            notice: None,
            resolve_error: None,
            last_spec: spec,
            call,
            stack: vec![Screen::List(ListScreen::new(ListKind::Home, Vec::new()))],
            tasks: Vec::new(),
            next_pager_id: 1,
            pending_commit_subject: None,
            pending_select: None,
            status_left: "Git".to_string(),
            status_right: String::new(),
        }
    }

    fn current(&self) -> &Screen {
        self.stack.last().expect("stack never empty")
    }

    fn current_mut(&mut self) -> &mut Screen {
        self.stack.last_mut().expect("stack never empty")
    }

    fn push(&mut self, screen: Screen) {
        self.stack.push(screen);
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    fn at_root(&self) -> bool {
        self.stack.len() == 1
    }

    // --- target attach / refresh -------------------------------------------------

    fn attach(&mut self, spec: TuiTargetSpec) {
        self.last_spec = spec.clone();
        match resolve_pinned_target(&spec, &self.call) {
            Ok(target) => {
                self.target = Some(target);
                self.resolve_error = None;
                self.refresh_status_task();
                self.show_home();
            }
            Err(err) => {
                self.target = None;
                self.status = None;
                self.resolve_error = Some(err);
                self.show_resolve_error();
            }
        }
    }

    fn home_items(&self) -> Vec<ListItem> {
        let changes = self.status.as_ref().map(|s| s.changes);
        HOME_ITEMS
            .iter()
            .map(|(id, label, detail)| {
                let mut item = ListItem {
                    label: label.to_string(),
                    detail: detail.to_string(),
                    disabled: false,
                    disabled_reason: None,
                    value: ListValue::Home(*id),
                };
                // Mutation gating (contract item 4): disable with reason,
                // not hidden.
                if *id == HomeAction::StageAll
                    && changes
                        .map(|c| c.staged + c.unstaged + c.untracked == 0)
                        .unwrap_or(false)
                {
                    item.disabled = true;
                    item.disabled_reason = Some("nothing to stage".to_string());
                }
                if *id == HomeAction::UnstageAll && changes.map(|c| c.staged == 0).unwrap_or(false)
                {
                    item.disabled = true;
                    item.disabled_reason = Some("nothing staged".to_string());
                }
                if *id == HomeAction::Commit && changes.map(|c| c.staged == 0).unwrap_or(false) {
                    item.disabled = true;
                    item.disabled_reason = Some("nothing staged".to_string());
                }
                item
            })
            .collect()
    }

    fn show_home(&mut self) {
        let items = self.home_items();
        self.stack = vec![Screen::List(ListScreen::new(ListKind::Home, items))];
        self.sync_status_line();
    }

    fn show_resolve_error(&mut self) {
        let honest = self
            .resolve_error
            .as_ref()
            .map(|e| describe_error(e, None))
            .unwrap_or_else(|| HonestError {
                title: "Error".to_string(),
                lines: vec!["unknown error".to_string()],
                cli_fallback: None,
            });
        let mut lines = vec![String::new()];
        lines.extend(honest.lines.clone());
        if let Some(fb) = &honest.cli_fallback {
            lines.push(String::new());
            lines.push(format!("CLI fallback: {fb}"));
        }
        self.push(Screen::Pager(PagerScreen {
            id: self.next_pager_id,
            load: PagerLoad::Static { lines },
            static_title: Some(format!("Git — {}", honest.title)),
            lines: Vec::new(),
            error: None,
            loading: false,
            scroll: 0,
        }));
        self.next_pager_id += 1;
        self.sync_status_line();
    }

    fn sync_status_line(&mut self) {
        if let (Some(status), Some(target)) = (&self.status, &self.target) {
            self.status_left = format!(
                "{} · {}",
                format_branch_line(&status.branch),
                format_change_counts(&status.changes)
            );
            self.status_right = repo_dir_name(&target.repo_root);
        } else if let Some(target) = &self.target {
            self.status_left = repo_dir_name(&target.repo_root);
            self.status_right = String::new();
        } else {
            self.status_left = "Git".to_string();
            self.status_right = String::new();
        }
    }

    fn refresh(&mut self) {
        self.notice = None;
        self.refresh_status_task();
        // Rebuild the home list in place when it is on top.
        if matches!(self.current(), Screen::List(l) if l.kind == ListKind::Home) {
            let items = self.home_items();
            if let Screen::List(list) = self.current_mut() {
                list.set_items(items);
            }
        }
        self.sync_status_line();
    }

    // --- background task plumbing -------------------------------------------------

    fn spawn_task<F, R>(&mut self, kind: BgKind, label: &str, f: F)
    where
        F: FnOnce(RunOptions) -> R + Send + 'static,
        R: Into<BgResult> + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<BgResult>();
        let cancel2 = cancel.clone();
        std::thread::spawn(move || {
            let mut opts = git_opts(MUTATION_TIMEOUT_MS);
            opts.cancel = Some(cancel2);
            let result = f(opts);
            let _ = tx.send(result.into());
        });
        self.tasks.push(BgTask {
            kind,
            label: label.to_string(),
            cancel,
            rx,
        });
    }

    /// Poll finished tasks; apply results. Returns true while any task
    /// is still running.
    fn poll_tasks(&mut self) -> bool {
        let mut i = 0;
        let mut any_running = false;
        while i < self.tasks.len() {
            match self.tasks[i].rx.try_recv() {
                Ok(result) => {
                    let _task = self.tasks.remove(i);
                    self.apply_task_result(result);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    any_running = true;
                    i += 1;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tasks.remove(i);
                }
            }
        }
        any_running
    }

    /// Cancel the most recently spawned cancellable task.
    fn cancel_top_task(&mut self) {
        if let Some(task) = self.tasks.iter().rev().find(|t| t.kind != BgKind::Status) {
            task.cancel.store(true, Ordering::SeqCst);
            self.notice = Some(format!("cancelling: {}…", task.label));
            self.sync_status_line();
        }
    }

    fn busy_label(&self) -> Option<String> {
        self.tasks.last().map(|t| t.label.clone())
    }

    fn apply_task_result(&mut self, result: BgResult) {
        match result {
            BgResult::Pager { pager_id, result } => self.apply_pager_result(pager_id, result),
            BgResult::Mutation { result, action } => self.apply_mutation_result(result, &action),
            BgResult::Status(result) => match result {
                Ok(mut report) => {
                    // Unborn branch: collectBranch mislabels HEAD as
                    // detached — present the symbolic branch name
                    // honestly instead.
                    if self
                        .target
                        .as_ref()
                        .and_then(|t| t.head_sha.as_ref())
                        .is_none()
                    {
                        if let Some(t) = &self.target {
                            report.branch = git::BranchInfo {
                                name: t.branch_name.clone().unwrap_or_else(|| "HEAD".to_string()),
                                detached: false,
                                upstream: None,
                                ahead: None,
                                behind: None,
                            };
                        }
                    }
                    self.status = Some(report);
                    self.resolve_error = None;
                    self.sync_status_line();
                    self.refresh_home_items();
                }
                Err(err) => {
                    self.status = None;
                    self.show_data_error(err);
                }
            },
            BgResult::Files(result) => {
                if let Screen::List(list) = self.current_mut() {
                    if list.kind == ListKind::Files {
                        match result {
                            Ok(files) => list.set_items(files.iter().map(file_list_item).collect()),
                            Err(err) => self.show_data_error(err),
                        }
                    }
                }
            }
            BgResult::Log(result) => {
                if let Screen::List(list) = self.current_mut() {
                    if list.kind == ListKind::Log {
                        match result {
                            Ok(commits) => {
                                list.set_items(commits.iter().map(commit_list_item).collect())
                            }
                            Err(err) => self.show_data_error(err),
                        }
                    }
                }
            }
            BgResult::Branches(result) => {
                if let Screen::List(list) = self.current_mut() {
                    if list.kind == ListKind::Branches {
                        match result {
                            Ok(entries) => {
                                list.set_items(entries.iter().map(branch_list_item).collect())
                            }
                            Err(err) => self.show_data_error(err),
                        }
                    }
                }
            }
            BgResult::Remotes(result) => {
                let pending = self.pending_select.take();
                match result {
                    Ok(remotes) => {
                        if let Some((purpose, branch)) = pending {
                            self.open_remote_select(purpose, branch, remotes);
                        }
                    }
                    Err(err) => self.show_data_error(err),
                }
            }
        }
    }

    fn apply_pager_result(&mut self, pager_id: u64, result: Result<Vec<String>, TuiError>) {
        let idx = self
            .stack
            .iter()
            .rposition(|s| matches!(s, Screen::Pager(p) if p.id == pager_id));
        let Some(idx) = idx else { return };
        let repo_root = self.target_repo_root().map(|s| s.to_string());
        if let Screen::Pager(p) = &mut self.stack[idx] {
            p.loading = false;
            match result {
                Ok(lines) => {
                    p.lines = lines;
                    p.error = None;
                }
                Err(err) => {
                    let honest = describe_error(&err, repo_root.as_deref());
                    let mut block_lines = vec![String::new()];
                    block_lines.extend(honest.lines.clone());
                    if let Some(fb) = &honest.cli_fallback {
                        block_lines.push(String::new());
                        block_lines.push(format!("CLI fallback: {fb}"));
                    }
                    p.error = Some(HonestBlock {
                        title: honest.title,
                        lines: block_lines,
                    });
                }
            }
            p.scroll = 0;
        }
    }

    fn target_repo_root(&self) -> Option<&str> {
        self.target.as_ref().map(|t| t.repo_root.as_str())
    }

    fn apply_mutation_result(&mut self, result: Result<String, TuiError>, action: &MutationAction) {
        match result {
            Ok(message) => {
                self.notice = Some(message);
                // Post-success pin refresh for branch ops / commit.
                if matches!(
                    action,
                    MutationAction::BranchSwitch { .. }
                        | MutationAction::BranchCreate { .. }
                        | MutationAction::Commit { .. }
                ) {
                    if let Some(t) = self.target.clone() {
                        let repo = t.repo_root.clone();
                        let opts = git_opts(GIT_TIMEOUT_MS);
                        let sha = git::head_sha_or_null(Path::new(&repo), &opts);
                        let bn = git::current_branch_name(Path::new(&repo), &opts);
                        if let Some(t) = self.target.as_mut() {
                            if let Some(sha) = sha {
                                t.head_sha = Some(sha);
                            }
                            let detached = bn.as_deref() == Some("HEAD");
                            t.branch_name = if detached { None } else { bn };
                            t.detached = detached;
                        }
                    }
                }
                self.refresh_status_task();
            }
            Err(TuiError::Stale(msg)) => {
                self.show_stale(&msg);
            }
            Err(TuiError::Git(g)) if g.kind == git::GitErrorKind::Cancelled => {
                self.notice = Some("cancelled".to_string());
            }
            Err(err) => {
                let honest = describe_error(&err, self.target_repo_root());
                self.notice = Some(format!(
                    "{}: {}",
                    honest.title,
                    honest.lines.first().cloned().unwrap_or_default()
                ));
            }
        }
        self.sync_status_line();
        self.refresh_home_items();
    }

    fn refresh_home_items(&mut self) {
        if matches!(self.current(), Screen::List(l) if l.kind == ListKind::Home) {
            let items = self.home_items();
            if let Screen::List(list) = self.current_mut() {
                list.set_items(items);
            }
        }
    }

    fn refresh_status_task(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.spawn_task(BgKind::Status, "refresh status", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            BgResult::Status(
                git::collect_status(Path::new(&target.repo_root), &o).map_err(TuiError::from),
            )
        });
    }

    // --- pager plumbing --------------------------------------------------------------

    fn push_pager(&mut self, load: PagerLoad) {
        let pager_id = self.next_pager_id;
        self.next_pager_id += 1;
        let mut pager = PagerScreen {
            id: pager_id,
            load: load.clone(),
            static_title: None,
            lines: Vec::new(),
            error: None,
            loading: true,
            scroll: 0,
        };
        if pager.is_static() {
            if let PagerLoad::Static { lines } = &pager.load {
                pager.lines = lines.clone();
            }
            pager.loading = false;
        } else {
            self.load_pager(pager_id, &pager.load);
        }
        self.push(Screen::Pager(pager));
    }

    fn push_static_pager(&mut self, title: String, lines: Vec<String>) {
        let id = self.next_pager_id;
        self.next_pager_id += 1;
        self.push(Screen::Pager(PagerScreen {
            id,
            load: PagerLoad::Static { lines: Vec::new() },
            static_title: Some(title),
            lines,
            error: None,
            loading: false,
            scroll: 0,
        }));
    }

    fn load_pager(&mut self, pager_id: u64, load: &PagerLoad) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let repo = target.repo_root.clone();
        let load = load.clone();
        self.spawn_task(BgKind::Pager, "load", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            let result = load_pager_lines(&repo, &load, &o);
            BgResult::Pager { pager_id, result }
        });
    }

    fn reload_top_pager(&mut self) {
        let Screen::Pager(pager) = self.current_mut() else {
            return;
        };
        if pager.is_static() {
            return;
        }
        let pager_id = pager.id;
        pager.loading = true;
        pager.error = None;
        pager.scroll = 0;
        let load = pager.load.clone();
        self.load_pager(pager_id, &load);
    }

    fn cycle_top_pager_scope(&mut self) {
        let Screen::Pager(pager) = self.current_mut() else {
            return;
        };
        match &mut pager.load {
            PagerLoad::FileDiff { scope, .. } | PagerLoad::Diff { scope } => {
                *scope = scope.next();
            }
            _ => {}
        }
        self.reload_top_pager();
    }

    // --- data error / stale ---------------------------------------------------------

    fn show_data_error(&mut self, err: TuiError) {
        let honest = describe_error(&err, self.target_repo_root());
        let mut lines = vec![String::new()];
        lines.extend(honest.lines.clone());
        if let Some(fb) = &honest.cli_fallback {
            lines.push(String::new());
            lines.push(format!("CLI fallback: {fb}"));
        }
        self.push_static_pager(format!("Git — {}", honest.title), lines);
    }

    fn show_stale(&mut self, message: &str) {
        let lines = vec![
            String::new(),
            message.to_string(),
            String::new(),
            "The repository moved since this surface opened. The mutation was".to_string(),
            "aborted; nothing was retargeted silently (tui-v1 §3.3).".to_string(),
            String::new(),
            "Press r to refresh status, or close and reopen for a fresh pin.".to_string(),
        ];
        self.push_static_pager("Git — stale target".to_string(), lines);
    }

    fn require_target(&mut self) -> Option<PinnedTarget> {
        if self.target.is_none() {
            self.notice = Some("no git target resolved".to_string());
            self.sync_status_line();
            return None;
        }
        self.target.clone()
    }

    // --- dispatch (home actions) -----------------------------------------------------

    fn dispatch_home(&mut self, action: HomeAction) {
        match action {
            HomeAction::Files => self.show_files(),
            HomeAction::Diff => self.show_diff(),
            HomeAction::Log => self.show_log(),
            HomeAction::Compare => self.show_compare(),
            HomeAction::Branches => self.show_branches(),
            HomeAction::Remotes => self.show_remotes(),
            HomeAction::StageAll => self.stage_all(),
            HomeAction::UnstageAll => self.unstage_all(),
            HomeAction::Commit => self.commit_flow_start(),
            HomeAction::Fetch => self.fetch_flow(),
            HomeAction::Push => self.push_flow(),
            HomeAction::Upstream => {
                let branch = self.target.as_ref().and_then(|t| t.branch_name.clone());
                self.set_upstream_for(branch);
            }
            HomeAction::Refresh => self.refresh(),
        }
    }

    // --- read views -------------------------------------------------------------------

    fn spawn_files_load(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.spawn_task(BgKind::List, "load files", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            let res = git::collect_files(Path::new(&target.repo_root), &[], &o)
                .map(|r| r.files)
                .map_err(TuiError::from);
            BgResult::Files(res)
        });
    }

    fn spawn_log_load(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.spawn_task(BgKind::List, "load log", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            let req = git::LogRequest {
                limit: Some(100),
                ..Default::default()
            };
            let res = git::collect_log(Path::new(&target.repo_root), &req, &o)
                .map(|r| r.commits)
                .map_err(TuiError::from);
            BgResult::Log(res)
        });
    }

    fn spawn_branches_load(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.spawn_task(BgKind::List, "load branches", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            let res =
                git::collect_branches(Path::new(&target.repo_root), &o).map_err(TuiError::from);
            BgResult::Branches(res)
        });
    }

    fn show_files(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.spawn_files_load();
        self.push(Screen::List(ListScreen::new(ListKind::Files, Vec::new())));
    }

    fn show_diff(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.push_pager(PagerLoad::Diff {
            scope: DiffCycle::Worktree,
        });
    }

    fn show_log(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.spawn_log_load();
        self.push(Screen::List(ListScreen::new(ListKind::Log, Vec::new())));
    }

    fn show_compare(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.push_pager(PagerLoad::Compare { base: None });
    }

    fn show_branches(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.spawn_branches_load();
        self.push(Screen::List(ListScreen::new(
            ListKind::Branches,
            Vec::new(),
        )));
    }

    fn show_remotes(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.push_pager(PagerLoad::Remotes);
    }

    // --- mutations ---------------------------------------------------------------------

    /// Confirm → task(fresh-check + action) → refresh. Stale targets abort
    /// via a dedicated stale screen; nothing retargets silently.
    fn run_mutation(&mut self, mutation: PendingMutation) {
        let Some(pinned) = self.target.clone() else {
            return;
        };
        let repo = pinned.repo_root.clone();
        let call = self.call.clone();
        let action = mutation.action.clone();
        let label = mutation.cli_label.clone();
        self.spawn_task(
            BgKind::Mutation,
            &format!("running: {label}"),
            move |mut opts| {
                opts.timeout_ms = Some(MUTATION_TIMEOUT_MS);
                // Fresh-check inside the worker so the UI stays live.
                let result = assert_target_fresh(&pinned, &call)
                    .and_then(|_| run_mutation_body(&repo, &action, &mut opts));
                BgResult::Mutation { result, action }
            },
        );
    }

    // --- flows -------------------------------------------------------------------------

    fn stage_all(&mut self) {
        let Some(t) = self.require_target() else {
            return;
        };
        let n = self
            .status
            .as_ref()
            .map(|s| s.changes.unstaged + s.changes.untracked + s.changes.conflicted)
            .unwrap_or(0);
        self.confirm_mutation(PendingMutation {
            title: "Stage all changes".to_string(),
            target_lines: vec![
                format!("repo: {}", t.repo_root),
                format!(
                    "branch: {}",
                    t.branch_name.as_deref().unwrap_or("HEAD (detached)")
                ),
                format!("~{n} unstaged/untracked file(s)"),
            ],
            consequence: "git add -A stages every change including untracked files.".to_string(),
            cli_label: "stage --all".to_string(),
            action: MutationAction::Stage {
                all: true,
                paths: Vec::new(),
            },
        });
    }

    fn unstage_all(&mut self) {
        let Some(t) = self.require_target() else {
            return;
        };
        self.confirm_mutation(PendingMutation {
            title: "Unstage all changes".to_string(),
            target_lines: vec![
                format!("repo: {}", t.repo_root),
                format!(
                    "branch: {}",
                    t.branch_name.as_deref().unwrap_or("HEAD (detached)")
                ),
            ],
            consequence: "git reset unstages everything; worktree files are untouched.".to_string(),
            cli_label: "unstage --all".to_string(),
            action: MutationAction::Unstage {
                all: true,
                paths: Vec::new(),
            },
        });
    }

    fn commit_flow_start(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.push(Screen::Prompt(PromptState {
            kind: PromptKind::CommitSubject,
            draft: String::new(),
            placeholder: String::new(),
            label: "commit subject:".to_string(),
        }));
    }

    fn fetch_flow(&mut self) {
        if self.require_target().is_none() {
            return;
        }
        self.pending_select = Some((SelectPurpose::FetchRemote, None));
        self.spawn_remotes_load();
    }

    fn push_flow(&mut self) {
        let Some(t) = self.require_target() else {
            return;
        };
        if t.detached || t.branch_name.is_none() {
            self.push_static_pager(
                "push — detached HEAD".to_string(),
                vec![
                    "Cannot push: HEAD is detached.".to_string(),
                    "Check out a branch first (Branches → switch).".to_string(),
                ],
            );
            return;
        }
        let branch = t.branch_name.clone().unwrap_or_default();
        let upstream = self.status.as_ref().and_then(|s| s.branch.upstream.clone());
        if let Some(upstream) = upstream {
            let remote = upstream.split('/').next().unwrap_or(&upstream).to_string();
            let repo = t.repo_root.clone();
            self.confirm_mutation(PendingMutation {
                title: "Push".to_string(),
                target_lines: vec![
                    format!("repo: {repo}"),
                    format!("branch: {branch}"),
                    format!("upstream: {upstream} (remote {remote})"),
                ],
                consequence: "publishes local commits to the upstream branch.".to_string(),
                cli_label: format!("push {remote}"),
                action: MutationAction::Push {
                    remote: Some(remote.clone()),
                    refspec: None,
                    set_upstream: false,
                },
            });
            return;
        }
        // No upstream: pick a remote, push -u HEAD:<branch>.
        self.pending_select = Some((SelectPurpose::PushRemote, Some(branch)));
        self.spawn_remotes_load();
    }

    fn spawn_remotes_load(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.spawn_task(BgKind::RemotesSelect, "load remotes", move |opts| {
            let mut o = opts;
            o.timeout_ms = Some(GIT_TIMEOUT_MS);
            let res =
                git::collect_remotes(Path::new(&target.repo_root), &o).map_err(TuiError::from);
            BgResult::Remotes(res)
        });
    }

    fn open_remote_select(
        &mut self,
        purpose: SelectPurpose,
        branch: Option<String>,
        remotes: Vec<RemoteInfo>,
    ) {
        if remotes.is_empty() {
            let (title, lines) = match purpose {
                SelectPurpose::FetchRemote => (
                    "fetch — no remotes".to_string(),
                    vec![
                        "This repository has no remotes configured.".to_string(),
                        "Add one with: git remote add <name> <url>".to_string(),
                    ],
                ),
                SelectPurpose::PushRemote => (
                    "push — no remotes".to_string(),
                    vec!["This repository has no remotes configured; nowhere to push.".to_string()],
                ),
            };
            self.push_static_pager(title, lines);
            return;
        }
        let items = remotes
            .iter()
            .map(|r| SelectItem {
                label: r.name.clone(),
                detail: r.url.clone(),
                remote: r.clone(),
            })
            .collect();
        let title = match purpose {
            SelectPurpose::FetchRemote => "fetch — choose remote".to_string(),
            SelectPurpose::PushRemote => "push — choose remote".to_string(),
        };
        self.push(Screen::Select(SelectScreen {
            title,
            items,
            cursor: 0,
            scroll_top: 0,
            purpose,
            branch,
        }));
    }

    fn set_upstream_for(&mut self, branch: Option<String>) {
        let Some(t) = self.require_target() else {
            return;
        };
        let Some(branch) = branch else {
            self.notice = Some("detached HEAD — no branch to set upstream on".to_string());
            self.sync_status_line();
            return;
        };
        if t.detached {
            self.notice = Some("detached HEAD — no branch to set upstream on".to_string());
            self.sync_status_line();
            return;
        }
        self.push(Screen::Prompt(PromptState {
            kind: PromptKind::Upstream {
                branch: branch.clone(),
            },
            draft: String::new(),
            placeholder: String::new(),
            label: format!("upstream for {branch} (e.g. origin/main):"),
        }));
    }

    fn confirm_mutation(&mut self, mutation: PendingMutation) {
        self.push(Screen::Confirm(mutation));
    }

    // --- key handling -------------------------------------------------------------------

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self.current_mut() {
            Screen::Prompt(_) => self.handle_prompt_key(key),
            Screen::Confirm(_) => self.handle_confirm_key(key),
            Screen::Select(_) => self.handle_select_key(key),
            Screen::Pager(_) => self.handle_pager_key(key),
            Screen::List(_) => self.handle_list_key(key),
            Screen::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.pop();
                    true
                }
                _ => false,
            },
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.pop();
                self.notice = Some("cancelled".to_string());
                self.sync_status_line();
                true
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let screen = self.stack.pop();
                if let Some(Screen::Confirm(mutation)) = screen {
                    self.run_mutation(mutation);
                }
                true
            }
            _ => true, // swallow other keys while confirming
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> bool {
        let Screen::Prompt(prompt) = self.current_mut() else {
            return true;
        };
        match key.code {
            KeyCode::Esc => {
                self.pop();
                true
            }
            KeyCode::Enter => {
                let draft = prompt.draft.clone();
                let kind = prompt.kind.clone();
                self.prompt_submit(kind, draft);
                true
            }
            KeyCode::Backspace => {
                prompt.draft.pop();
                true
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                prompt.draft.push(c);
                true
            }
            _ => true,
        }
    }

    fn prompt_submit(&mut self, kind: PromptKind, draft: String) {
        match kind {
            PromptKind::CommitSubject => {
                let subj = draft.trim().to_string();
                if subj.is_empty() {
                    self.notice = Some("empty commit message".to_string());
                    self.sync_status_line();
                    return;
                }
                self.pending_commit_subject = Some(subj);
                self.push(Screen::Prompt(PromptState {
                    kind: PromptKind::CommitBody,
                    draft: String::new(),
                    placeholder: "(empty)".to_string(),
                    label: "commit body (optional, Enter to skip):".to_string(),
                }));
            }
            PromptKind::CommitBody => {
                let subj = self.pending_commit_subject.take().unwrap_or_default();
                if subj.is_empty() {
                    return;
                }
                let body = draft.trim().to_string();
                let messages = if body.is_empty() {
                    vec![subj]
                } else {
                    vec![subj, body]
                };
                let Some(t) = self.require_target() else {
                    return;
                };
                let staged = self.status.as_ref().map(|s| s.changes.staged).unwrap_or(0);
                let subj_show = messages[0].clone();
                self.confirm_mutation(PendingMutation {
                    title: "Commit".to_string(),
                    target_lines: vec![
                        format!("repo: {}", t.repo_root),
                        format!(
                            "branch: {}",
                            t.branch_name.as_deref().unwrap_or("HEAD (detached)")
                        ),
                        format!("staged files: {staged}"),
                        format!("message: {subj_show}"),
                    ],
                    consequence: "creates a new commit on the current branch.".to_string(),
                    cli_label: "commit".to_string(),
                    action: MutationAction::Commit { messages },
                });
            }
            PromptKind::BaseRevision => {
                let trimmed = draft.trim().to_string();
                if trimmed.starts_with('-') {
                    self.notice = Some("invalid base revision".to_string());
                    self.sync_status_line();
                    return;
                }
                if let Screen::Pager(p) = self.current_mut() {
                    if let PagerLoad::Compare { base } = &mut p.load {
                        *base = if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        };
                    }
                }
                self.reload_top_pager();
            }
            PromptKind::BranchName => {
                let trimmed = draft.trim().to_string();
                if trimmed.is_empty() || trimmed.starts_with('-') {
                    self.notice = Some("invalid branch name".to_string());
                    self.sync_status_line();
                    return;
                }
                let Some(t) = self.require_target() else {
                    return;
                };
                let name = trimmed.clone();
                self.confirm_mutation(PendingMutation {
                    title: "Create and switch branch".to_string(),
                    target_lines: vec![
                        format!("repo: {}", t.repo_root),
                        format!("branch: {trimmed}"),
                    ],
                    consequence: "git switch -c creates the branch and moves HEAD to it."
                        .to_string(),
                    cli_label: format!("branch create {name}"),
                    action: MutationAction::BranchCreate {
                        name: trimmed.clone(),
                    },
                });
            }
            PromptKind::Upstream { branch } => {
                let trimmed = draft.trim().to_string();
                if trimmed.is_empty() || trimmed.starts_with('-') {
                    self.notice = Some("invalid upstream ref".to_string());
                    self.sync_status_line();
                    return;
                }
                let Some(t) = self.require_target() else {
                    return;
                };
                self.confirm_mutation(PendingMutation {
                    title: "Set upstream".to_string(),
                    target_lines: vec![
                        format!("repo: {}", t.repo_root),
                        format!("branch: {branch}"),
                        format!("upstream: {trimmed}"),
                    ],
                    consequence: "git branch --set-upstream-to records the tracking ref."
                        .to_string(),
                    cli_label: format!("upstream {trimmed}"),
                    action: MutationAction::SetUpstream {
                        upstream: trimmed.clone(),
                    },
                });
            }
        }
    }

    fn handle_select_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pop();
                self.pending_select = None;
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Screen::Select(sel) = self.current_mut() {
                    sel.move_cursor(-1);
                }
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Screen::Select(sel) = self.current_mut() {
                    sel.move_cursor(1);
                }
                true
            }
            KeyCode::Enter => {
                let chosen = if let Screen::Select(sel) = self.current() {
                    sel.current().cloned()
                } else {
                    None
                };
                let (purpose, branch) = if let Screen::Select(sel) = self.current() {
                    (sel.purpose, sel.branch.clone())
                } else {
                    return true;
                };
                self.pop();
                if let Some(item) = chosen {
                    self.on_remote_chosen(purpose, branch, item);
                }
                true
            }
            _ => true,
        }
    }

    fn handle_pager_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        // `r` reloads, `tab` cycles diff scope, `b` sets compare base —
        // checked before generic scroll keys.
        if let KeyCode::Char(c) = key.code {
            if !key.modifiers.contains(KeyModifiers::ALT) {
                match c {
                    'r' | 'R' => {
                        self.reload_top_pager();
                        return true;
                    }
                    '\t' => {
                        let is_diff =
                            matches!(self.current(), Screen::Pager(p) if p.is_diff_scope_pager());
                        if is_diff {
                            self.cycle_top_pager_scope();
                            return true;
                        }
                    }
                    'b' => {
                        let is_cmp =
                            matches!(self.current(), Screen::Pager(p) if p.is_compare_pager());
                        if is_cmp {
                            self.push(Screen::Prompt(PromptState {
                                kind: PromptKind::BaseRevision,
                                draft: String::new(),
                                placeholder: String::new(),
                                label: "base revision (e.g. origin/main, HEAD~5):".to_string(),
                            }));
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        let Screen::Pager(pager) = self.current_mut() else {
            return false;
        };
        let page = 10usize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                pager.scroll = pager.scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                pager.scroll += 1;
                true
            }
            KeyCode::PageUp => {
                pager.scroll = pager.scroll.saturating_sub(page);
                true
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                pager.scroll += page;
                true
            }
            KeyCode::Home => {
                pager.scroll = 0;
                true
            }
            KeyCode::End => {
                pager.scroll = usize::MAX;
                true
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pop();
                true
            }
            _ => false,
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> bool {
        // Filter draft mode swallows everything.
        let filtering = matches!(self.current(), Screen::List(l) if l.filter_active);
        if filtering {
            if let Screen::List(list) = self.current_mut() {
                match key.code {
                    KeyCode::Esc => {
                        list.filter_active = false;
                        list.filter_draft.clear();
                    }
                    KeyCode::Enter => {
                        list.committed_filter = list.filter_draft.clone();
                        list.filter_active = false;
                        list.clamp_cursor();
                    }
                    KeyCode::Backspace => {
                        list.filter_draft.pop();
                        list.clamp_cursor();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        list.filter_draft.push(c);
                        list.cursor = 0;
                        list.scroll_top = 0;
                        list.clamp_cursor();
                    }
                    _ => {}
                }
            }
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Screen::List(l) = self.current_mut() {
                    l.move_cursor(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Screen::List(l) = self.current_mut() {
                    l.move_cursor(1);
                }
            }
            KeyCode::PageUp => {
                if let Screen::List(l) = self.current_mut() {
                    for _ in 0..10 {
                        l.move_cursor(-1);
                    }
                }
            }
            KeyCode::PageDown => {
                if let Screen::List(l) = self.current_mut() {
                    for _ in 0..10 {
                        l.move_cursor(1);
                    }
                }
            }
            KeyCode::Home => {
                if let Screen::List(l) = self.current_mut() {
                    l.cursor = 0;
                }
            }
            KeyCode::End => {
                if let Screen::List(l) = self.current_mut() {
                    let n = l.filtered().len();
                    l.cursor = n.saturating_sub(1);
                }
            }
            KeyCode::Char('/') => {
                if let Screen::List(l) = self.current_mut() {
                    l.filter_active = true;
                    l.filter_draft = l.committed_filter.clone();
                }
            }
            KeyCode::Enter => {
                let item = if let Screen::List(l) = self.current() {
                    l.current().cloned()
                } else {
                    None
                };
                if let Some(item) = item {
                    if !item.disabled {
                        self.on_list_select(item);
                    }
                }
            }
            KeyCode::Esc => {
                if !self.at_root() {
                    self.pop();
                } else if !self.tasks.is_empty() {
                    self.cancel_top_task();
                }
            }
            KeyCode::Char('q') => {
                if !self.at_root() {
                    self.pop();
                } else if !self.tasks.is_empty() {
                    self.cancel_top_task();
                } else {
                    return false; // quit the app
                }
            }
            KeyCode::Char('?') => {
                self.push(Screen::Help);
            }
            KeyCode::Char('R') => {
                self.refresh();
            }
            KeyCode::Char(c) => {
                let kind = match self.current() {
                    Screen::List(l) => l.kind,
                    _ => ListKind::Home,
                };
                self.on_list_command(c, kind);
            }
            _ => {}
        }
        true
    }

    fn on_list_select(&mut self, item: ListItem) {
        let kind = match self.current() {
            Screen::List(l) => l.kind,
            _ => return,
        };
        match (kind, &item.value) {
            (ListKind::Home, ListValue::Home(action)) => {
                let action = *action;
                self.dispatch_home(action);
            }
            (ListKind::Files, ListValue::File(file)) => {
                self.open_file_diff(file.clone());
            }
            (ListKind::Log, ListValue::Commit(commit)) => {
                self.show_commit(commit.clone());
            }
            (ListKind::Branches, ListValue::Branch(entry)) if !entry.current => {
                self.switch_branch(entry.clone());
            }
            _ => {}
        }
    }

    fn current_list_item(&self) -> Option<ListItem> {
        if let Screen::List(list) = self.current() {
            list.current().cloned()
        } else {
            None
        }
    }

    fn on_list_command(&mut self, c: char, kind: ListKind) {
        match kind {
            ListKind::Home => {
                if c == 'r' {
                    self.refresh();
                }
            }
            ListKind::Files => match c {
                'd' => {
                    if let Some(item) = self.current_list_item() {
                        if !item.disabled {
                            if let ListValue::File(f) = &item.value {
                                self.open_file_diff(f.clone());
                            }
                        }
                    }
                }
                's' => {
                    if let Some(item) = self.current_list_item() {
                        if !item.disabled {
                            if let ListValue::File(f) = &item.value {
                                self.file_stage(f.clone(), true);
                            }
                        }
                    }
                }
                'u' => {
                    if let Some(item) = self.current_list_item() {
                        if !item.disabled {
                            if let ListValue::File(f) = &item.value {
                                self.file_stage(f.clone(), false);
                            }
                        }
                    }
                }
                'r' => self.reload_top_list(),
                _ => {}
            },
            ListKind::Log => {
                if c == 'r' {
                    self.reload_top_list();
                }
            }
            ListKind::Branches => match c {
                'n' => {
                    self.push(Screen::Prompt(PromptState {
                        kind: PromptKind::BranchName,
                        draft: String::new(),
                        placeholder: String::new(),
                        label: "new branch name:".to_string(),
                    }));
                }
                'u' => {
                    let branch = self.target.as_ref().and_then(|t| t.branch_name.clone());
                    self.set_upstream_for(branch);
                }
                'r' => self.reload_top_list(),
                _ => {}
            },
        }
    }

    fn reload_top_list(&mut self) {
        let kind = match self.current() {
            Screen::List(l) => l.kind,
            _ => return,
        };
        match kind {
            ListKind::Files => self.spawn_files_load(),
            ListKind::Log => self.spawn_log_load(),
            ListKind::Branches => self.spawn_branches_load(),
            ListKind::Home => self.refresh(),
        }
    }

    // --- file diff -----------------------------------------------------------------------

    fn open_file_diff(&mut self, file: FileChange) {
        let scope = diff_scope_for(&file);
        if scope == FileDiffScope::None {
            let lines = vec![
                file_label(&file),
                String::new(),
                "No diff exists for this entry:".to_string(),
                if file.kind == FileChangeKind::Untracked {
                    "untracked files are not in the index — stage it first (s) to diff.".to_string()
                } else {
                    "ignored files are excluded from diffs.".to_string()
                },
            ];
            self.push_static_pager(format!("diff — {}", file.path), lines);
            return;
        }
        let cycle = match scope {
            FileDiffScope::Staged => DiffCycle::Staged,
            FileDiffScope::Worktree => DiffCycle::Worktree,
            FileDiffScope::Head => DiffCycle::Head,
            FileDiffScope::None => unreachable!(),
        };
        self.push_pager(PagerLoad::FileDiff { file, scope: cycle });
    }

    fn show_commit(&mut self, commit: LogCommit) {
        if self.require_target().is_none() {
            return;
        }
        self.push_pager(PagerLoad::CommitShow {
            sha: commit.sha.clone(),
            subject: commit.subject.clone(),
        });
    }

    // --- branch flows ---------------------------------------------------------------------

    fn switch_branch(&mut self, entry: BranchEntry) {
        let Some(t) = self.require_target() else {
            return;
        };
        let repo = t.repo_root.clone();
        let from = t
            .branch_name
            .clone()
            .unwrap_or_else(|| "HEAD (detached)".to_string());
        let name = entry.name.clone();
        let sha = entry.sha.clone();
        self.confirm_mutation(PendingMutation {
            title: "Switch branch".to_string(),
            target_lines: vec![
                format!("repo: {repo}"),
                format!("from: {from}"),
                format!("to: {name} ({})", short_sha7(&sha)),
            ],
            consequence: "git switch changes HEAD; uncommitted changes may conflict.".to_string(),
            cli_label: format!("branch switch {name}"),
            action: MutationAction::BranchSwitch { name: name.clone() },
        });
    }

    // --- file stage -------------------------------------------------------------------------

    fn file_stage(&mut self, file: FileChange, staging: bool) {
        let Some(t) = self.require_target() else {
            return;
        };
        if !staging && file.kind == FileChangeKind::Untracked {
            self.notice = Some(format!(
                "{} is untracked — nothing staged to unstage",
                file.path
            ));
            self.sync_status_line();
            return;
        }
        if staging && file.kind == FileChangeKind::Ignored {
            self.notice = Some(format!(
                "{} is ignored — git add refuses it without -f",
                file.path
            ));
            self.sync_status_line();
            return;
        }
        let mut paths = vec![file.path.clone()];
        if let Some(old) = &file.old_path {
            paths.push(old.clone());
        }
        let verb = if staging { "stage" } else { "unstage" };
        self.confirm_mutation(PendingMutation {
            title: format!(
                "{} {}",
                if staging { "Stage" } else { "Unstage" },
                file.path
            ),
            target_lines: vec![
                format!("repo: {}", t.repo_root),
                format!(
                    "branch: {}",
                    t.branch_name.as_deref().unwrap_or("HEAD (detached)")
                ),
                format!("path: {}", paths.join(", ")),
            ],
            consequence: format!("git {} the listed path(s).", verb),
            cli_label: format!("{verb} {}", paths.join(" ")),
            action: if staging {
                MutationAction::Stage { all: false, paths }
            } else {
                MutationAction::Unstage { all: false, paths }
            },
        });
    }

    // --- remote selection ---------------------------------------------------------------------

    fn on_remote_chosen(
        &mut self,
        purpose: SelectPurpose,
        branch: Option<String>,
        item: SelectItem,
    ) {
        match purpose {
            SelectPurpose::FetchRemote => {
                let Some(t) = self.require_target() else {
                    return;
                };
                let remote = item.remote.name.clone();
                let url = item.remote.url.clone();
                self.confirm_mutation(PendingMutation {
                    title: "Fetch".to_string(),
                    target_lines: vec![
                        format!("repo: {}", t.repo_root),
                        format!("remote: {remote} ({url})"),
                    ],
                    consequence:
                        "git fetch downloads objects/refs; it does not touch your worktree."
                            .to_string(),
                    cli_label: format!("fetch {remote}"),
                    action: MutationAction::Fetch {
                        remote: remote.clone(),
                    },
                });
            }
            SelectPurpose::PushRemote => {
                let Some(t) = self.require_target() else {
                    return;
                };
                let Some(branch) = branch else { return };
                let remote = item.remote.name.clone();
                let url = item.remote.url.clone();
                let refspec = format!("HEAD:{branch}");
                self.confirm_mutation(PendingMutation {
                    title: "Push (set upstream)".to_string(),
                    target_lines: vec![
                        format!("repo: {}", t.repo_root),
                        format!("branch: {branch}"),
                        format!("remote: {remote} ({url})"),
                        format!("refspec: {refspec} (--set-upstream)"),
                    ],
                    consequence: "creates the remote branch and records upstream tracking."
                        .to_string(),
                    cli_label: format!("push -u {remote} {branch}"),
                    action: MutationAction::Push {
                        remote: Some(remote.clone()),
                        refspec: Some(refspec),
                        set_upstream: true,
                    },
                });
            }
        }
    }
}

// helpers -------------------------------------------------------------------------

fn repo_dir_name(repo_root: &str) -> String {
    Path::new(repo_root)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| repo_root.to_string())
}

fn file_list_item(f: &FileChange) -> ListItem {
    ListItem {
        label: file_label(f),
        detail: file_detail(f),
        disabled: false,
        disabled_reason: None,
        value: ListValue::File(f.clone()),
    }
}

fn commit_list_item(c: &LogCommit) -> ListItem {
    ListItem {
        label: commit_label(c),
        detail: c.sha.clone(),
        disabled: false,
        disabled_reason: None,
        value: ListValue::Commit(c.clone()),
    }
}

fn branch_list_item(b: &BranchEntry) -> ListItem {
    let detail = [
        b.upstream.as_ref().map(|u| format!("upstream {u}")),
        b.track.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    ListItem {
        label: branch_label(b),
        detail,
        disabled: b.current,
        disabled_reason: if b.current {
            Some("current".to_string())
        } else {
            None
        },
        value: ListValue::Branch(b.clone()),
    }
}

// ---------------------------------------------------------------------------
// Pager load execution (worker thread)
// ---------------------------------------------------------------------------

fn load_pager_lines(
    repo_root: &str,
    load: &PagerLoad,
    opts: &RunOptions,
) -> Result<Vec<String>, TuiError> {
    let repo = Path::new(repo_root);
    match load {
        PagerLoad::FileDiff { file, scope } => {
            let mut paths = vec![file.path.clone()];
            if let Some(old) = &file.old_path {
                paths.push(old.clone());
            }
            let mut request = DiffRequest {
                paths: paths.clone(),
                ..Default::default()
            };
            match scope {
                DiffCycle::Staged => request.staged = true,
                DiffCycle::Head => request.revisions = vec!["HEAD".to_string()],
                DiffCycle::Worktree => {}
            }
            let res = git::collect_diff(repo, &request, opts)?;
            let mut lines = res
                .output
                .split('\n')
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                lines.pop();
            }
            let mut out = vec![format!(
                "scope: {} · path: {}",
                scope.file_label(),
                paths.join(", ")
            )];
            if res.truncated {
                out.push("(truncated at byte cap — narrow the path)".to_string());
            }
            out.push(String::new());
            out.extend(lines);
            Ok(if res.output.is_empty() {
                vec!["(empty diff)".to_string()]
            } else {
                out
            })
        }
        PagerLoad::Diff { scope } => {
            let mut request = DiffRequest {
                max_bytes: Some(DIFF_MAX_BYTES),
                ..Default::default()
            };
            match scope {
                DiffCycle::Staged => request.staged = true,
                DiffCycle::Head => request.revisions = vec!["HEAD".to_string()],
                DiffCycle::Worktree => {}
            }
            let res = git::collect_diff(repo, &request, opts)?;
            let mut lines = res
                .output
                .split('\n')
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                lines.pop();
            }
            let mut out: Vec<String> = Vec::new();
            if res.truncated {
                out.push("(truncated at byte cap)".to_string());
                out.push(String::new());
            }
            out.extend(lines);
            Ok(if res.output.is_empty() {
                vec!["(empty diff)".to_string()]
            } else {
                out
            })
        }
        PagerLoad::CommitShow { sha, .. } => {
            let out = git::collect_commit_show(repo, sha, opts)?;
            let mut lines = out.split('\n').map(|s| s.to_string()).collect::<Vec<_>>();
            if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
                lines.pop();
            }
            Ok(lines)
        }
        PagerLoad::Compare { base } => {
            let res =
                git::collect_compare(repo, base.as_deref(), None, Some(DIFF_MAX_BYTES), opts)?;
            let mut lines = vec![
                format!(
                    "{}...{}  (merge-base {})",
                    res.base,
                    res.head,
                    short_sha7(&res.merge_base)
                ),
                format!("ahead {} · behind {}", res.ahead, res.behind),
                String::new(),
                res.stat.clone(),
                String::new(),
            ];
            let mut patch = res
                .patch
                .output
                .split('\n')
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            if patch.last().map(|l| l.is_empty()).unwrap_or(false) {
                patch.pop();
            }
            if res.patch.truncated {
                lines.insert(0, "(patch truncated at byte cap)".to_string());
                lines.insert(1, String::new());
            }
            lines.extend(patch);
            Ok(lines)
        }
        PagerLoad::Remotes => {
            let remotes = git::collect_remotes(repo, opts)?;
            if remotes.is_empty() {
                return Ok(vec!["(no remotes configured)".to_string()]);
            }
            let mut lines: Vec<String> = Vec::new();
            for r in &remotes {
                lines.push(r.name.clone());
                lines.push(format!("  fetch: {}", r.url));
                lines.push(format!("  push:  {}", r.push_url));
                lines.push(String::new());
            }
            lines.pop();
            Ok(lines)
        }
        PagerLoad::Static { lines } => Ok(lines.clone()),
    }
}

// ---------------------------------------------------------------------------
// Mutation execution (worker thread)
// ---------------------------------------------------------------------------

fn run_mutation_body(
    repo_root: &str,
    action: &MutationAction,
    opts: &mut RunOptions,
) -> Result<String, TuiError> {
    let repo = Path::new(repo_root);
    match action {
        MutationAction::Stage { all, paths } => {
            git::collect_stage(repo, *all, paths, opts)?;
            Ok(if *all {
                "staged all changes".to_string()
            } else {
                format!("staged {}", paths.join(", "))
            })
        }
        MutationAction::Unstage { all, paths } => {
            git::collect_unstage(repo, *all, paths, opts)?;
            Ok(if *all {
                "unstaged all changes".to_string()
            } else {
                format!("unstaged {}", paths.join(", "))
            })
        }
        MutationAction::Commit { messages } => {
            let (sha, _summary) = git::collect_commit(repo, messages, false, opts)?;
            Ok(format!("committed {}", short_sha7(&sha)))
        }
        MutationAction::BranchSwitch { name } => {
            git::collect_branch_switch(repo, name, opts)?;
            Ok(format!("switched to {name}"))
        }
        MutationAction::BranchCreate { name } => {
            git::collect_branch_create(repo, name, true, opts)?;
            Ok(format!("created and switched to {name}"))
        }
        MutationAction::SetUpstream { upstream } => {
            git::collect_upstream(repo, upstream, opts)?;
            Ok(format!("upstream set: {}", upstream))
        }
        MutationAction::Fetch { remote } => {
            let out = git::collect_fetch(repo, Some(remote), false, opts)?;
            Ok(out
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("fetched {remote}")))
        }
        MutationAction::Push {
            remote,
            refspec,
            set_upstream,
        } => {
            let out = git::collect_push(
                repo,
                remote.as_deref(),
                refspec.as_deref(),
                *set_upstream,
                false,
                opts,
            )?;
            Ok(out
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "pushed".to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn paint_diff_line(line: &str) -> Line<'static> {
    let safe = || bound_text(&sanitize_display(line), 500);
    if line.starts_with("@@") {
        return Line::from(Span::styled(
            safe(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if line.starts_with("+++") || line.starts_with("---") {
        return Line::from(Span::styled(
            safe(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    if line.starts_with('+') {
        return Line::from(Span::styled(safe(), Style::default().fg(Color::Green)));
    }
    if line.starts_with('-') {
        return Line::from(Span::styled(safe(), Style::default().fg(Color::Red)));
    }
    if line.starts_with("diff ") || line.starts_with("commit ") {
        return Line::from(Span::styled(
            safe(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(safe())
}

fn centered(area: Rect) -> Rect {
    Rect {
        x: area.x + area.width / 4,
        y: area.y + area.height / 4,
        width: area.width / 2,
        height: area.height / 2,
    }
}

fn render(app: &mut GitTui, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // repo/branch header
            Constraint::Min(1),
            Constraint::Length(1), // status line
        ])
        .split(area);

    // Header: branch + change counts on home, repo root otherwise.
    let is_home = matches!(app.current(), Screen::List(l) if l.kind == ListKind::Home);
    let header_text = if is_home {
        let mut parts = Vec::new();
        if let Some(s) = &app.status {
            parts.push(format!("⎇ {}", format_branch_line(&s.branch)));
            parts.push(format_change_counts(&s.changes));
        }
        if let Some(t) = &app.target {
            parts.push(format!("repo: {}", t.repo_root));
            if let Some(sha) = &t.head_sha {
                parts.push(format!("HEAD: {}", short_sha7(sha)));
            }
        }
        parts.join("   ")
    } else {
        app.target
            .as_ref()
            .map(|t| format!("repo: {}", t.repo_root))
            .unwrap_or_else(|| "repo: (unresolved)".to_string())
    };
    frame.render_widget(
        Paragraph::new(sanitize_display(&header_text))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    // Body
    let has_head = app
        .target
        .as_ref()
        .and_then(|t| t.head_sha.as_ref())
        .is_some();
    match app.current_mut() {
        Screen::List(list) => render_list(list, frame, chunks[1], has_head),
        Screen::Pager(pager) => render_pager(pager, frame, chunks[1]),
        Screen::Prompt(prompt) => {
            let lines = vec![
                Line::from(sanitize_display(&prompt.label))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from(format!("> {}█", sanitize_display(&prompt.draft))),
                Line::from(Span::styled(
                    format!(
                        "(enter submit · esc cancel · placeholder: {})",
                        sanitize_display(&prompt.placeholder)
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ];
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("git")),
                chunks[1],
            );
        }
        Screen::Confirm(mutation) => {
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
            let area = centered(chunks[1]);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("confirm")),
                area,
            );
        }
        Screen::Select(sel) => {
            let mut lines: Vec<Line> = Vec::new();
            lines.push(
                Line::from(sanitize_display(&sel.title))
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            );
            lines.push(Line::from(""));
            let visible = (chunks[1].height as usize).saturating_sub(3).max(1);
            let n = sel.items.len();
            if sel.cursor < sel.scroll_top {
                sel.scroll_top = sel.cursor;
            }
            if sel.cursor >= sel.scroll_top + visible {
                sel.scroll_top = sel.cursor + 1 - visible;
            }
            sel.scroll_top = sel.scroll_top.min(n.saturating_sub(visible));
            for i in 0..visible.min(n.saturating_sub(sel.scroll_top)) {
                let item = &sel.items[sel.scroll_top + i];
                let is_sel = sel.scroll_top + i == sel.cursor;
                let prefix = if is_sel { "❯ " } else { "  " };
                let style = if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(sanitize_display(&item.label), style),
                    Span::styled(
                        format!("  {}", sanitize_display(&item.detail)),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
            if n > visible {
                lines.push(Line::from(Span::styled(
                    format!("  {}/{}", sel.cursor + 1, n),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            lines.push(Line::from(Span::styled(
                "j/k move · enter choose · esc cancel",
                Style::default().add_modifier(Modifier::DIM),
            )));
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("select")),
                chunks[1],
            );
        }
        Screen::Help => {
            let mut text: Vec<Line> = Vec::new();
            text.push(
                Line::from("Git TUI keymap").style(Style::default().add_modifier(Modifier::BOLD)),
            );
            text.push(Line::from(""));
            for (keys, desc) in [
                ("j/k · ↑/↓", "move"),
                ("pgup/pgdn · space", "page"),
                ("home/end", "top/bottom"),
                ("/", "filter list"),
                ("enter", "open / select"),
                ("d", "file diff (files)"),
                ("s", "stage file (files)"),
                ("u", "unstage file / set upstream (files/branches)"),
                ("n", "new branch (branches)"),
                ("r/R", "reload / refresh"),
                ("tab", "cycle diff scope (diff)"),
                ("b", "set base revision (compare)"),
                ("?", "help"),
                ("esc/q", "back / quit / cancel task"),
            ] {
                text.push(Line::from(format!("  {:<24} {}", keys, desc)));
            }
            text.push(Line::from(""));
            text.push(Line::from(
                "Mutations are confirm-gated and revalidate the pinned target before running.",
            ));
            frame.render_widget(
                Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("help")),
                chunks[1],
            );
        }
    }

    // Footer status line.
    let busy = app
        .busy_label()
        .map(|l| format!(" … {l}"))
        .unwrap_or_default();
    let footer = if let Screen::List(list) = app.current() {
        if list.filter_active {
            format!(
                " filter: /{}█ — enter apply · esc cancel",
                sanitize_display(&list.filter_draft)
            )
        } else {
            format!(
                " {}{} · ? help · q quit",
                sanitize_display(&app.status_left),
                busy
            )
        }
    } else {
        format!(
            " {}{} · esc back · q quit",
            sanitize_display(&app.status_left),
            busy
        )
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().add_modifier(Modifier::DIM)),
        chunks[2],
    );
}

fn render_list(list: &mut ListScreen, frame: &mut ratatui::Frame, area: Rect, has_head: bool) {
    let filtered = list.filtered();
    if filtered.is_empty() {
        let msg = if list.items.is_empty() {
            match list.kind {
                ListKind::Files => "  (no changes)",
                ListKind::Log => {
                    if has_head {
                        "  (no commits)"
                    } else {
                        "  (unborn branch — no commits yet)"
                    }
                }
                ListKind::Branches => "  (no local branches)",
                ListKind::Home => "  (no actions)",
            }
        } else {
            "  no items match the current filter"
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
    let use_detail = area.width >= 60;
    let list_w = if use_detail {
        (area.width as f64 * 0.55) as u16
    } else {
        area.width
    };
    let detail_w = area.width.saturating_sub(list_w + 1);
    let chrome = if list.filter_active || !list.committed_filter.is_empty() {
        2
    } else {
        1
    };
    let visible = (area.height as usize).saturating_sub(chrome).max(1);
    let n = filtered.len();
    if list.cursor < list.scroll_top {
        list.scroll_top = list.cursor;
    }
    if list.cursor >= list.scroll_top + visible {
        list.scroll_top = list.cursor + 1 - visible;
    }
    list.scroll_top = list.scroll_top.min(n.saturating_sub(visible));

    let mut list_lines: Vec<Line> = Vec::new();
    if list.filter_active || !list.committed_filter.is_empty() {
        let q = if list.filter_active {
            list.filter_draft.clone()
        } else {
            list.committed_filter.clone()
        };
        list_lines.push(Line::from(Span::styled(
            format!(" /{}", sanitize_display(&q)),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    for i in 0..visible.min(n.saturating_sub(list.scroll_top)) {
        let idx = filtered[list.scroll_top + i];
        let item = &list.items[idx];
        let is_sel = list.scroll_top + i == list.cursor;
        let mut label = item.label.clone();
        if item.disabled {
            if let Some(reason) = &item.disabled_reason {
                label = format!("{} ({})", label, reason);
            }
        }
        let prefix = if is_sel { "❯ " } else { "  " };
        let style = if is_sel {
            Style::default().add_modifier(Modifier::BOLD)
        } else if item.disabled {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(prefix, style),
            Span::styled(sanitize_display(&label), style),
        ];
        let w = list_w as usize;
        let used = prefix.chars().count() + label.chars().count();
        if used < w {
            spans.push(Span::raw(" ".repeat(w.saturating_sub(used))));
        }
        list_lines.push(Line::from(spans));
    }
    if n > visible {
        list_lines.push(Line::from(Span::styled(
            format!("  {}/{}", list.cursor + 1, n),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: list_w,
        height: area.height,
    };
    frame.render_widget(Paragraph::new(list_lines), list_area);

    if use_detail && detail_w > 0 {
        let detail_text = filtered
            .get(list.cursor)
            .and_then(|&i| list.items.get(i))
            .map(|item| sanitize_display(&item.detail))
            .unwrap_or_default();
        let detail_area = Rect {
            x: area.x + list_w + 1,
            y: area.y,
            width: detail_w,
            height: area.height,
        };
        frame.render_widget(
            Paragraph::new(detail_text).style(Style::default().add_modifier(Modifier::DIM)),
            detail_area,
        );
    }
}

fn render_pager(pager: &mut PagerScreen, frame: &mut ratatui::Frame, area: Rect) {
    let title = sanitize_display(&pager.title());
    let body_h = area.height.saturating_sub(3).max(1) as usize;
    let source: Vec<String> = if pager.loading && pager.lines.is_empty() && pager.error.is_none() {
        vec!["(loading…)".to_string()]
    } else if let Some(err) = &pager.error {
        let mut v = vec![String::new(), err.title.clone(), String::new()];
        v.extend(err.lines.clone());
        v
    } else if pager.lines.is_empty() {
        vec!["(empty)".to_string()]
    } else {
        pager.lines.clone()
    };
    let max_scroll = source.len().saturating_sub(body_h);
    pager.scroll = pager.scroll.min(max_scroll);
    let mut lines: Vec<Line> = Vec::new();
    for r in 0..body_h {
        match source.get(pager.scroll + r) {
            Some(l) => lines.push(paint_diff_line(l)),
            None => lines.push(Line::from("")),
        }
    }
    let pos = format!("{}/{}", (pager.scroll + 1).min(source.len()), source.len());
    let state = if pager.loading {
        " …"
    } else if pager.error.is_some() {
        " !"
    } else {
        ""
    };
    let w = area.width as usize;
    let pos_w = pos.chars().count() + state.len() + 1;
    let pad = w.saturating_sub(pos_w);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            format!("{pos}{state}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));

    let mut out: Vec<Line> = Vec::new();
    out.push(Line::from(Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    )));
    out.push(Line::from("─".repeat(area.width as usize)));
    out.extend(lines);
    frame.render_widget(Paragraph::new(out).wrap(Wrap { trim: false }), area);
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

const USAGE: &str = r#"rebuildup.git TUI — interactive git surface (contract herdr-plugins.tui/v1)

usage:
  git tui [main|forge] [--cwd <path>]   open the UI (manifest pane entrypoint)
  git tui --open                        open a UI pane for the origin target
  git tui --help                        this text

environment:
  HERDR_PLUGIN_CONTEXT_JSON   origin workspace/tab/focused pane (pinned target)
  HERDR_TUI_TARGET_PANE_ID    explicit origin pane override
  HERDR_TUI_TARGET_CWD        explicit repo cwd override
  HERDR_BIN_PATH              herdr binary for --open (default: herdr on PATH)
"#;

struct CliArgs {
    open: bool,
    cwd: Option<String>,
    help: bool,
    surface: String,
    /// Extra positionals forwarded to the forge entrypoint (e.g. `open`).
    rest: Vec<String>,
}

fn parse_tui_args(argv: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs {
        open: false,
        cwd: None,
        help: false,
        surface: "main".to_string(),
        rest: Vec::new(),
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--open" {
            out.open = true;
        } else if arg == "--help" || arg == "-h" {
            out.help = true;
        } else if arg == "--cwd" {
            match argv.get(i + 1) {
                Some(v) if !v.starts_with('-') => {
                    out.cwd = Some(v.clone());
                    i += 1;
                }
                _ => return Err("--cwd requires a path".to_string()),
            }
        } else if let Some(v) = arg.strip_prefix("--cwd=") {
            if v.is_empty() {
                return Err("--cwd requires a path".to_string());
            }
            out.cwd = Some(v.to_string());
        } else if arg == "main" || arg == "forge" {
            out.surface = arg.clone();
        } else if arg == "open" {
            out.rest.push(arg.clone());
        } else if arg == "--" {
            // separator — ignored
        } else {
            return Err(format!("unknown argument: {arg}"));
        }
        i += 1;
    }
    Ok(out)
}

/// `--open`: spawn `herdr plugin pane open` and return its exit status.
fn run_open(surface: &str) -> i32 {
    let context =
        tui_model::parse_plugin_context(std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok().as_deref());
    let spec = tui_model::resolve_target_spec(None);
    let entrypoint = if surface == "forge" { "forge" } else { "main" };
    let args = tui_model::build_pane_open_args(
        "rebuildup.git",
        entrypoint,
        "tab",
        context
            .as_ref()
            .and_then(|c| c.workspace_id.as_deref())
            .or(spec.workspace_id.as_deref()),
        spec.pane_id.as_deref(),
    );
    let herdr_bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let result = Command::new(&herdr_bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    match result {
        Ok(mut child) => {
            // 30s timeout like the TS spawn timeout.
            let deadline = std::time::Instant::now() + Duration::from_millis(30_000);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.code().unwrap_or(1),
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            eprintln!("git-tui: herdr plugin pane open timed out");
                            return 1;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return 1,
                }
            }
        }
        Err(err) => {
            eprintln!(
                "git-tui: failed to run {herdr_bin}: {err}\n\
                 fall back to CLI: bin/git status <pane_id>"
            );
            1
        }
    }
}

/// `git tui [main|forge] [--open|--cwd DIR]` entrypoint.
pub fn run(args: &[String]) -> ! {
    let code = tui_main(args);
    std::process::exit(code);
}

fn tui_main(args: &[String]) -> i32 {
    let parsed = match parse_tui_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("git-tui: {e}");
            eprint!("{USAGE}");
            return 2;
        }
    };
    if parsed.surface == "forge" {
        if parsed.help {
            return crate::tui_forge::run_forge_tui(&["--help".to_string()]);
        }
        if parsed.open {
            return run_open(&parsed.surface);
        }
        return crate::tui_forge::run_forge_tui(&parsed.rest);
    }
    if parsed.help {
        print!("{USAGE}");
        return 0;
    }
    if parsed.open {
        return run_open(&parsed.surface);
    }

    let spec = tui_model::resolve_target_spec(parsed.cwd.as_deref());

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        // Honest unsupported state (contract item 9): never fake a TUI
        // on a pipe. The CLI is the non-interactive path.
        eprintln!(
            "git-tui: interactive UI requires a TTY.\n\
             Use the CLI instead: git status|files|diff|log|compare|stage|commit|branch|push <pane_id>"
        );
        return 1;
    }

    let call = CallOptions::default();
    let mut tui = GitTui::new(call, spec.clone());
    tui.attach(spec);

    if crossterm::terminal::enable_raw_mode().is_err() {
        eprintln!("git-tui: could not enable raw mode");
        return 1;
    }
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(err) => {
            eprintln!("git-tui: {err}");
            return 1;
        }
    };
    // NOTE: no terminal.clear() — ratatui-core's clear() queries cursor
    // position (ESC[6n) and blocks ~2s on terminals that never answer
    // DSR. EnterAlternateScreen already gives a blank screen.

    let result = (|| -> io::Result<()> {
        loop {
            // Apply finished background tasks before drawing.
            tui.poll_tasks();
            terminal.draw(|frame| render(&mut tui, frame))?;
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == event::KeyEventKind::Press && !tui.handle_key(key) {
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
            let _ = writeln!(io::stderr(), "git TUI closed");
            0
        }
        Err(err) => {
            eprintln!("git-tui: {err}");
            1
        }
    }
}
