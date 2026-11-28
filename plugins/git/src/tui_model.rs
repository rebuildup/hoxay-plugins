//! Pure view-model helpers for the rebuildup.git TUI. Rust port of
//! `plugins/git/src/tui/model.ts` — synchronous and side-effect free so
//! the formatting/parsing contract is unit-testable without a TTY or a
//! herdr session (mirrors `tests/git-tui.test.ts`).

use serde_json::Value;

use crate::git::{self, BranchInfo, ChangeCounts, FileChange, FileChangeKind, LogCommit};

// --- Launch context / target identity -----------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PluginContextIdentity {
    /// Origin pane the surface was launched from (the git target).
    pub focused_pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    /// Raw parsed context for callers that need more fields.
    pub raw: Value,
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Parse `HERDR_PLUGIN_CONTEXT_JSON`. Returns None when the variable is
/// absent or unparseable — callers must treat that as "no pinned
/// target", never guess from ambient state.
pub fn parse_plugin_context(raw: Option<&str>) -> Option<PluginContextIdentity> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(raw).ok()?;
    if !parsed.is_object() {
        return None;
    }
    Some(PluginContextIdentity {
        focused_pane_id: non_empty(parsed.get("focused_pane_id")),
        workspace_id: non_empty(parsed.get("workspace_id")),
        tab_id: non_empty(parsed.get("tab_id")),
        raw: parsed,
    })
}

#[derive(Debug, Clone, Default)]
pub struct TuiTargetSpec {
    /// Origin pane id pinned at launch; null when launched with --cwd only.
    pub pane_id: Option<String>,
    /// Optional explicit repo cwd (--cwd flag / HERDR_TUI_TARGET_CWD).
    pub cwd: Option<String>,
    pub workspace_id: Option<String>,
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Resolve the pinned target spec from the process environment.
/// Precedence: explicit `HERDR_TUI_TARGET_PANE_ID`/`HERDR_TUI_TARGET_CWD`
/// (set by the `open` action helper) > context `focused_pane_id` >
/// context workspace (pane_id stays null; cwd may come from --cwd).
/// `HERDR_PANE_ID` is deliberately NOT consulted: inside a pane-backed
/// plugin pane it identifies the UI pane itself (tui-v1 §3.2).
pub fn resolve_target_spec(cli_cwd: Option<&str>) -> TuiTargetSpec {
    let context = parse_plugin_context(env_nonempty("HERDR_PLUGIN_CONTEXT_JSON").as_deref());
    TuiTargetSpec {
        pane_id: env_nonempty("HERDR_TUI_TARGET_PANE_ID")
            .or_else(|| context.as_ref().and_then(|c| c.focused_pane_id.clone())),
        cwd: cli_cwd
            .map(|s| s.to_string())
            .or_else(|| env_nonempty("HERDR_TUI_TARGET_CWD"))
            .or_else(|| env_nonempty("HERDR_TUI_CWD")),
        workspace_id: context.and_then(|c| c.workspace_id),
    }
}

/// argv for `herdr plugin pane open` used by the `open` action. Pure so
/// the exact invocation is unit-testable.
pub fn build_pane_open_args(
    plugin_id: &str,
    entrypoint: &str,
    placement: &str,
    workspace_id: Option<&str>,
    target_pane_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "plugin".to_string(),
        "pane".to_string(),
        "open".to_string(),
        "--plugin".to_string(),
        plugin_id.to_string(),
        "--entrypoint".to_string(),
        entrypoint.to_string(),
        "--placement".to_string(),
        placement.to_string(),
    ];
    if let Some(ws) = workspace_id {
        args.push("--workspace".to_string());
        args.push(ws.to_string());
    }
    if let Some(tp) = target_pane_id {
        args.push("--env".to_string());
        args.push(format!("HERDR_TUI_TARGET_PANE_ID={tp}"));
    }
    args
}

// --- Display formatting ---------------------------------------------------------

pub fn short_sha(sha: &str, len: usize) -> String {
    if sha.chars().count() <= len {
        sha.to_string()
    } else {
        sha.chars().take(len).collect()
    }
}

pub fn short_sha7(sha: &str) -> String {
    short_sha(sha, 7)
}

/// One-line branch badge for the status line / headers.
pub fn format_branch_line(branch: &BranchInfo) -> String {
    if branch.detached {
        return "HEAD (detached)".to_string();
    }
    let mut parts = vec![branch.name.clone()];
    if let Some(upstream) = &branch.upstream {
        parts.push(format!("-> {upstream}"));
        if let Some(ahead) = branch.ahead {
            if ahead > 0 {
                parts.push(format!("ahead {ahead}"));
            }
        }
        if let Some(behind) = branch.behind {
            if behind > 0 {
                parts.push(format!("behind {behind}"));
            }
        }
        if branch.ahead.unwrap_or(0) == 0 && branch.behind.unwrap_or(0) == 0 {
            parts.push("up to date".to_string());
        }
    } else {
        parts.push("(no upstream)".to_string());
    }
    parts.join(" ")
}

/// Compact change counts for the status line.
pub fn format_change_counts(counts: &ChangeCounts) -> String {
    let total = counts.staged + counts.unstaged + counts.untracked + counts.conflicted;
    if total == 0 {
        return "clean".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if counts.staged > 0 {
        parts.push(format!("staged {}", counts.staged));
    }
    if counts.unstaged > 0 {
        parts.push(format!("unstaged {}", counts.unstaged));
    }
    if counts.untracked > 0 {
        parts.push(format!("untracked {}", counts.untracked));
    }
    if counts.conflicted > 0 {
        parts.push(format!("CONFLICTS {}", counts.conflicted));
    }
    parts.join(" · ")
}

const KIND_LABEL: fn(FileChangeKind) -> &'static str = |kind| match kind {
    FileChangeKind::Added => "added",
    FileChangeKind::Modified => "modified",
    FileChangeKind::Deleted => "deleted",
    FileChangeKind::Renamed => "renamed",
    FileChangeKind::Copied => "copied",
    FileChangeKind::Typechange => "type",
    FileChangeKind::Untracked => "untracked",
    FileChangeKind::Conflicted => "conflict",
    FileChangeKind::Ignored => "ignored",
};

pub fn kind_label(kind: FileChangeKind) -> &'static str {
    KIND_LABEL(kind)
}

/// XY status columns + kind marker for a changed file row.
pub fn file_status_tag(file: &FileChange) -> String {
    match file.kind {
        FileChangeKind::Untracked => "??".to_string(),
        FileChangeKind::Conflicted => "UU".to_string(),
        FileChangeKind::Ignored => "!!".to_string(),
        _ => {
            let x = if file.index == " " {
                " "
            } else {
                file.index.as_str()
            };
            let y = if file.worktree == " " {
                " "
            } else {
                file.worktree.as_str()
            };
            format!("{x}{y}")
        }
    }
}

/// SelectList label for a changed file.
pub fn file_label(file: &FileChange) -> String {
    let path = match &file.old_path {
        Some(old) => format!("{old} -> {}", file.path),
        None => file.path.clone(),
    };
    format!(
        "{} {}  {}",
        file_status_tag(file),
        kind_label(file.kind),
        path
    )
}

/// SelectList detail (right pane / wide terminals) for a changed file.
pub fn file_detail(file: &FileChange) -> String {
    let mut parts = vec![kind_label(file.kind).to_string()];
    let mut flags: Vec<&str> = Vec::new();
    if file.staged {
        flags.push("staged");
    }
    if file.unstaged {
        flags.push("unstaged");
    }
    if file.kind == FileChangeKind::Untracked {
        flags.push("not in index");
    }
    if file.binary == Some(true) {
        flags.push("binary");
    }
    if !flags.is_empty() {
        parts.push(flags.join("+"));
    }
    if let Some(old) = &file.old_path {
        parts.push(format!("from {old}"));
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDiffScope {
    Staged,
    Worktree,
    Head,
    None,
}

/// Which `git diff` scope shows this file's change best:
/// - untracked/ignored: no diff exists ("none" — the view says so honestly)
/// - staged-only: `--staged`
/// - unstaged-only: worktree diff
/// - both staged and unstaged: `HEAD` revision diff (covers both sides)
/// - conflicted: worktree `git diff` shows the combined conflict markers
pub fn diff_scope_for(file: &FileChange) -> FileDiffScope {
    match file.kind {
        FileChangeKind::Untracked | FileChangeKind::Ignored => return FileDiffScope::None,
        FileChangeKind::Conflicted => return FileDiffScope::Worktree,
        _ => {}
    }
    if file.staged && file.unstaged {
        FileDiffScope::Head
    } else if file.staged && !file.unstaged {
        FileDiffScope::Staged
    } else {
        FileDiffScope::Worktree
    }
}

/// SelectList label for a log entry.
pub fn commit_label(commit: &LogCommit) -> String {
    format!("{} {}", short_sha7(&commit.sha), commit.subject)
}

/// Detail text for a log entry (wide-terminal detail pane).
pub fn commit_detail(commit: &LogCommit) -> String {
    let mut lines = vec![
        format!("sha: {}", commit.sha),
        format!(
            "author: {} <{}> {}",
            commit.author_name, commit.author_email, commit.author_date
        ),
        format!(
            "committer: {} <{}> {}",
            commit.committer_name, commit.committer_email, commit.committer_date
        ),
    ];
    if !commit.parents.is_empty() {
        lines.push(format!("parents: {}", commit.parents.join(" ")));
    }
    lines.join("\n")
}

// --- Branch list -----------------------------------------------------------------

pub fn branch_label(entry: &git::BranchEntry) -> String {
    let marker = if entry.current { "* " } else { "  " };
    let up = entry
        .upstream
        .as_ref()
        .map(|u| format!(" -> {u}"))
        .unwrap_or_default();
    let track = entry
        .track
        .as_ref()
        .map(|t| format!(" {t}"))
        .unwrap_or_default();
    format!(
        "{}{}{}{}  {}",
        marker,
        entry.name,
        up,
        track,
        short_sha7(&entry.sha)
    )
}

// --- Error classification ---------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HonestError {
    /// Short title for the status/detail header.
    pub title: String,
    /// Explanation lines (already plain text).
    pub lines: Vec<String>,
    /// Suggested CLI fallback command, when one exists.
    pub cli_fallback: Option<String>,
}

const CLI_FALLBACK: &str = "bin/git status <pane_id>";

/// Error surface the TUI classifies: GitError kinds, TargetResolutionError
/// codes, herdr socket errors. Mirrors `describeError`.
pub fn describe_error_kind(kind: &str, message: &str, cwd: Option<&str>) -> HonestError {
    match kind {
        "not_a_repo" => HonestError {
            title: "Not a git repository".to_string(),
            lines: vec![
                "No git work tree contains the target directory.".to_string(),
                format!("target cwd: {}", cwd.unwrap_or("(unknown)")),
                String::new(),
                "This surface only works inside a git work tree.".to_string(),
            ],
            cli_fallback: Some(CLI_FALLBACK.to_string()),
        },
        "bare_repo" => HonestError {
            title: "Bare repository".to_string(),
            lines: vec![
                "The target is a bare repository; there is no work tree to show.".to_string(),
            ],
            cli_fallback: Some(CLI_FALLBACK.to_string()),
        },
        "git_missing" => HonestError {
            title: "git not found".to_string(),
            lines: vec![
                "The `git` executable is not on PATH for this plugin process.".to_string(),
                "Install git or fix PATH, then reopen this surface.".to_string(),
            ],
            cli_fallback: Some(CLI_FALLBACK.to_string()),
        },
        "timeout" => HonestError {
            title: "Timed out".to_string(),
            lines: vec![
                "A git operation timed out.".to_string(),
                message.to_string(),
            ],
            cli_fallback: None,
        },
        "ambient_identity_mismatch" | "target_required" | "pane_not_found" | "no_pane_cwd" => {
            HonestError {
                title: "Target unavailable".to_string(),
                lines: vec![
                    message.to_string(),
                    String::new(),
                    "The pane this surface was opened for is gone or moved.".to_string(),
                ],
                cli_fallback: None,
            }
        }
        _ => HonestError {
            title: "Error".to_string(),
            lines: vec![message.to_string()],
            cli_fallback: Some(CLI_FALLBACK.to_string()),
        },
    }
}

/// Classify any error the TUI can meet: GitError (`kind`),
/// TargetResolutionError (`code`), or a plain message.
pub fn describe_error(err: &TuiError, cwd: Option<&str>) -> HonestError {
    match err {
        TuiError::Git(g) => describe_error_kind(g.kind.as_str(), &g.message, cwd),
        TuiError::Target(t) => describe_error_kind(t.code.as_str(), &t.message, cwd),
        TuiError::Kind { kind, message } => describe_error_kind(kind, message, cwd),
        TuiError::Stale(m) => HonestError {
            title: "Stale target".to_string(),
            lines: vec![m.clone()],
            cli_fallback: None,
        },
        TuiError::Other(m) => describe_error_kind("", m, cwd),
    }
}

/// Unified error type for TUI domain calls.
#[derive(Debug)]
pub enum TuiError {
    Git(git::GitError),
    Target(hoxay_herdr_client::target::TargetResolutionError),
    /// Kind-tagged error (pane_not_found, no_pane_cwd, target_required,
    /// stale_target) classified by `describe_error`.
    Kind {
        kind: &'static str,
        message: String,
    },
    /// Pre-mutation revalidation failure (tui-v1 §3.3).
    Stale(String),
    Other(String),
}

impl From<git::GitError> for TuiError {
    fn from(e: git::GitError) -> Self {
        TuiError::Git(e)
    }
}

impl From<hoxay_herdr_client::target::TargetResolutionError> for TuiError {
    fn from(e: hoxay_herdr_client::target::TargetResolutionError) -> Self {
        TuiError::Target(e)
    }
}

impl From<hoxay_herdr_client::commands::CommandError> for TuiError {
    fn from(e: hoxay_herdr_client::commands::CommandError) -> Self {
        match e {
            hoxay_herdr_client::commands::CommandError::Target(t) => TuiError::Target(t),
            hoxay_herdr_client::commands::CommandError::Client(c) => TuiError::Other(c.to_string()),
        }
    }
}

/// Error carrying a `kind` tag consumed by `describeError`.
pub struct TuiTargetError {
    pub kind: &'static str,
    pub message: String,
}

impl TuiTargetError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        TuiTargetError {
            kind,
            message: message.into(),
        }
    }
}

/// Resolve the pinned git target result (see tui/app.ts PinnedTarget).
#[derive(Debug, Clone)]
pub struct PinnedTarget {
    /// Origin pane id pinned at launch; null for --cwd / context-less runs.
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    /// Origin cwd the repo was resolved from (pane cwd or --cwd).
    pub cwd: String,
    /// Repo root all git commands run against.
    pub repo_root: String,
    /// HEAD sha at launch; null on unborn branches.
    pub head_sha: Option<String>,
    /// Branch name at launch; null when detached or unknown.
    pub branch_name: Option<String>,
    /// True when HEAD is detached.
    pub detached: bool,
}

/// Stale-generation marker for the pre-mutation revalidation (tui-v1 §3.3).
pub fn stale_target(message: impl Into<String>) -> TuiError {
    TuiError::Stale(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{BranchInfo, ChangeCounts, FileChange, FileChangeKind, LogCommit};

    fn branch(
        name: &str,
        detached: bool,
        upstream: Option<&str>,
        ahead: Option<i64>,
        behind: Option<i64>,
    ) -> BranchInfo {
        BranchInfo {
            name: name.to_string(),
            detached,
            upstream: upstream.map(|s| s.to_string()),
            ahead,
            behind,
        }
    }

    #[test]
    fn parse_plugin_context_works() {
        let ctx = parse_plugin_context(Some(
            r#"{"focused_pane_id":"p1","workspace_id":"w1","tab_id":"t1"}"#,
        ))
        .unwrap();
        assert_eq!(ctx.focused_pane_id.as_deref(), Some("p1"));
        assert_eq!(ctx.workspace_id.as_deref(), Some("w1"));
        assert!(parse_plugin_context(None).is_none());
        assert!(parse_plugin_context(Some("not json")).is_none());
        assert!(parse_plugin_context(Some("[1,2]")).is_none());
    }

    #[test]
    fn short_sha_truncates() {
        assert_eq!(short_sha("abcdef123456", 7), "abcdef1");
        assert_eq!(short_sha("abc", 7), "abc");
    }

    #[test]
    fn format_branch_line_states() {
        assert_eq!(
            format_branch_line(&branch("main", true, None, None, None)),
            "HEAD (detached)"
        );
        assert_eq!(
            format_branch_line(&branch(
                "main",
                false,
                Some("origin/main"),
                Some(2),
                Some(0)
            )),
            "main -> origin/main ahead 2"
        );
        assert_eq!(
            format_branch_line(&branch(
                "main",
                false,
                Some("origin/main"),
                Some(0),
                Some(0)
            )),
            "main -> origin/main up to date"
        );
        assert_eq!(
            format_branch_line(&branch("main", false, None, None, None)),
            "main (no upstream)"
        );
    }

    #[test]
    fn format_change_counts_works() {
        let c = ChangeCounts {
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
        };
        assert_eq!(format_change_counts(&c), "clean");
        let c = ChangeCounts {
            staged: 1,
            unstaged: 2,
            untracked: 3,
            conflicted: 4,
        };
        assert_eq!(
            format_change_counts(&c),
            "staged 1 · unstaged 2 · untracked 3 · CONFLICTS 4"
        );
    }

    fn file(
        kind: FileChangeKind,
        index: &str,
        worktree: &str,
        staged: bool,
        unstaged: bool,
        old: Option<&str>,
    ) -> FileChange {
        FileChange {
            path: "b.txt".to_string(),
            old_path: old.map(|s| s.to_string()),
            index: index.to_string(),
            worktree: worktree.to_string(),
            kind,
            staged,
            unstaged,
            binary: None,
        }
    }

    #[test]
    fn file_label_and_detail() {
        let f = file(FileChangeKind::Modified, "M", " ", true, false, None);
        assert_eq!(file_status_tag(&f), "M ");
        assert!(file_label(&f).contains("modified"));
        let u = file(FileChangeKind::Untracked, "?", "?", false, false, None);
        assert_eq!(file_status_tag(&u), "??");
        let r = file(
            FileChangeKind::Renamed,
            "R",
            " ",
            true,
            false,
            Some("a.txt"),
        );
        assert!(file_label(&r).contains("a.txt -> b.txt"));
    }

    #[test]
    fn diff_scope_for_cases() {
        assert_eq!(
            diff_scope_for(&file(
                FileChangeKind::Untracked,
                "?",
                "?",
                false,
                false,
                None
            )),
            FileDiffScope::None
        );
        assert_eq!(
            diff_scope_for(&file(FileChangeKind::Ignored, "!", "!", false, false, None)),
            FileDiffScope::None
        );
        assert_eq!(
            diff_scope_for(&file(
                FileChangeKind::Conflicted,
                "U",
                "U",
                false,
                false,
                None
            )),
            FileDiffScope::Worktree
        );
        assert_eq!(
            diff_scope_for(&file(FileChangeKind::Modified, "M", "M", true, true, None)),
            FileDiffScope::Head
        );
        assert_eq!(
            diff_scope_for(&file(FileChangeKind::Modified, "M", " ", true, false, None)),
            FileDiffScope::Staged
        );
        assert_eq!(
            diff_scope_for(&file(FileChangeKind::Modified, " ", "M", false, true, None)),
            FileDiffScope::Worktree
        );
    }

    #[test]
    fn commit_label_and_detail() {
        let c = LogCommit {
            sha: "abcdef1234567890".to_string(),
            parents: vec!["111".to_string()],
            author_name: "A".to_string(),
            author_email: "a@x".to_string(),
            author_date: "2024-01-01".to_string(),
            committer_name: "C".to_string(),
            committer_email: "c@x".to_string(),
            committer_date: "2024-01-02".to_string(),
            subject: "subject".to_string(),
        };
        assert_eq!(commit_label(&c), "abcdef1 subject");
        assert!(commit_detail(&c).contains("parents: 111"));
    }

    #[test]
    fn parse_branches_rows() {
        let out = "main\t*\torigin/main\t[ahead 1]\tdeadbeef\nfeature\t \t\t\t12345678\n";
        let entries = git::parse_branches(out);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].current);
        assert_eq!(entries[0].upstream.as_deref(), Some("origin/main"));
        assert_eq!(entries[0].track.as_deref(), Some("[ahead 1]"));
        assert!(!entries[1].current);
        assert_eq!(entries[1].upstream, None);
    }

    #[test]
    fn describe_error_kinds() {
        let e = describe_error_kind("not_a_repo", "no repo", Some("/x"));
        assert_eq!(e.title, "Not a git repository");
        assert!(e.lines.iter().any(|l| l.contains("/x")));
        let e = describe_error_kind("target_required", "boom", None);
        assert_eq!(e.title, "Target unavailable");
        assert!(e.cli_fallback.is_none());
        let e = describe_error_kind("weird", "msg", None);
        assert_eq!(e.title, "Error");
        assert!(e.cli_fallback.is_some());
    }

    #[test]
    fn build_pane_open_args_shape() {
        let args = build_pane_open_args("rebuildup.git", "main", "tab", Some("w1"), Some("p1"));
        assert_eq!(
            args,
            vec![
                "plugin",
                "pane",
                "open",
                "--plugin",
                "rebuildup.git",
                "--entrypoint",
                "main",
                "--placement",
                "tab",
                "--workspace",
                "w1",
                "--env",
                "HERDR_TUI_TARGET_PANE_ID=p1",
            ]
        );
        let args = build_pane_open_args("rebuildup.git", "forge", "tab", None, None);
        assert!(!args.iter().any(|a| a == "--workspace" || a == "--env"));
    }
}
