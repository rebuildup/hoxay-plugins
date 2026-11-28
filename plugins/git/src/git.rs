//! Read-only git access for rebuildup.git. Rust port of
//! `plugins/git/src/git.ts` + the spawn/resolve half of
//! `packages/git-core/src/index.ts`.
//!
//! Every repository query is a `git` binary invocation via argv spawn —
//! no shell, no string interpolation of paths or revisions into command
//! text (ADR-0006). The child env is sanitized by `gitenv` (issue #96).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::gitenv;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorKind {
    GitMissing,
    Timeout,
    GitError,
    NotARepo,
    BareRepo,
    /// TUI task cancellation: the child was killed on user request.
    /// Never produced by the CLI surface (no cancel flag is set there).
    Cancelled,
}

impl GitErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GitErrorKind::GitMissing => "git_missing",
            GitErrorKind::Timeout => "timeout",
            GitErrorKind::GitError => "git_error",
            GitErrorKind::NotARepo => "not_a_repo",
            GitErrorKind::BareRepo => "bare_repo",
            GitErrorKind::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug)]
pub struct GitError {
    pub kind: GitErrorKind,
    pub message: String,
}

impl GitError {
    pub fn new(kind: GitErrorKind, message: impl Into<String>) -> Self {
        GitError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GitError {}

pub type GitResult = Result<SpawnOutput, GitError>;

#[derive(Debug, Clone)]
pub struct SpawnOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub timeout_ms: Option<u64>,
    pub max_buffer: Option<usize>,
    /// Caller-supplied env vars (trusted internal overrides, e.g.
    /// GIT_INDEX_FILE for a scratch index). Applied before the safety pins.
    pub env: Vec<(String, String)>,
    /// Bytes piped to the child's stdin (`gh api --input -`, `--body-file -`).
    pub input: Option<String>,
    /// Cooperative cancellation (TUI task runner): when set, the wait
    /// loop kills the child and returns GitErrorKind::Cancelled.
    pub cancel: Option<Arc<AtomicBool>>,
}

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_BUFFER: usize = 16 * 1024 * 1024;

fn git_env(extra: &[(String, String)]) -> HashMap<String, String> {
    let mut overrides: Vec<(String, String)> = extra.to_vec();
    overrides.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    overrides.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
    overrides.push(("LC_ALL".into(), "C".into()));
    gitenv::sanitized_child_env(&overrides, &[])
}

/// Spawn a child with a fully replaced (sanitized) environment, piped
/// stdio, optional stdin payload, timeout, and an output byte cap.
/// Returns the collected output; spawn/timeout/overflow surface as errors.
fn spawn_bounded(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: HashMap<String, String>,
    opts: &RunOptions,
    max_buffer: usize,
    timeout_kind: fn(&str, u64) -> GitError,
    overflow_msg: &str,
    missing_msg: &str,
) -> GitResult {
    let timeout_ms = opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(if opts.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::new(GitErrorKind::GitMissing, missing_msg)
        } else {
            GitError::new(GitErrorKind::GitError, e.to_string())
        }
    })?;

    if let Some(input) = &opts.input {
        if let Some(mut stdin) = child.stdin.take() {
            // Write on a thread: a large payload plus early child output
            // could otherwise deadlock against the not-yet-spawned pipe
            // readers below.
            let data = input.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            });
        }
    }

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let (tx, rx) = std::sync::mpsc::channel::<(Vec<u8>, Vec<u8>)>();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut out);
        tx.send((out, Vec::new())).ok();
    });
    let (tx_err, rx_err) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut err = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut err);
        tx_err.send(err).ok();
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if opts
                    .cancel
                    .as_ref()
                    .map(|c| c.load(Ordering::SeqCst))
                    .unwrap_or(false)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::new(
                        GitErrorKind::Cancelled,
                        format!(
                            "git {} cancelled",
                            args.first().map(|s| s.as_str()).unwrap_or("")
                        ),
                    ));
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(timeout_kind(
                        args.first().map(|s| s.as_str()).unwrap_or(""),
                        timeout_ms,
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                return Err(GitError::new(GitErrorKind::GitError, e.to_string()));
            }
        }
    };
    let stdout_bytes = rx.recv().unwrap_or_default().0;
    let stderr_bytes = rx_err.recv().unwrap_or_default();
    let code = status.code().unwrap_or(-1);

    let mut stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    if stdout.len() > max_buffer {
        stdout.truncate(max_buffer);
        return Err(GitError::new(GitErrorKind::GitError, overflow_msg));
    }
    Ok(SpawnOutput {
        stdout,
        stderr,
        code,
    })
}

/// Run `git <args>` with `cwd` as the working directory. Resolves on any
/// exit code; callers inspect `code`. Errors on spawn failure, timeout,
/// or output exceeding the buffer cap. Mirrors `runGit`.
pub fn run_git(cwd: &Path, args: &[String], opts: &RunOptions) -> GitResult {
    spawn_bounded(
        "git",
        args,
        cwd,
        git_env(&opts.env),
        opts,
        opts.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER),
        |sub, ms| {
            GitError::new(
                GitErrorKind::Timeout,
                format!("git {sub} timed out after {ms}ms"),
            )
        },
        "git output exceeded buffer limit",
        "git executable not found on PATH",
    )
}

/// Run git and require exit code 0; map failures to GitError. Mirrors
/// `gitOk`. Error messages join argv with spaces like the TS version.
pub fn git_ok(cwd: &Path, args: &[String], opts: &RunOptions) -> Result<String, GitError> {
    let result = run_git(cwd, args, opts)?;
    if result.code != 0 {
        let detail = {
            let stderr = result.stderr.trim();
            let stdout = result.stdout.trim();
            if !stderr.is_empty() {
                stderr.to_string()
            } else if !stdout.is_empty() {
                stdout.to_string()
            } else {
                format!("exit {}", result.code)
            }
        };
        return Err(GitError::new(
            GitErrorKind::GitError,
            format!("git {}: {}", args.join(" "), detail),
        ));
    }
    Ok(result.stdout)
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub root: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub bare: bool,
}

/// Resolve the repository containing `cwd`. Errors `not_a_repo` outside
/// work trees and `bare_repo` for bare repositories. Mirrors `resolveRepo`.
pub fn resolve_repo(cwd: &Path, opts: &RunOptions) -> Result<RepoInfo, GitError> {
    let top = match git_ok(cwd, &["rev-parse".into(), "--show-toplevel".into()], opts) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            if e.kind == GitErrorKind::GitError
                && e.message.to_lowercase().contains("not a git repository")
            {
                return Err(GitError::new(
                    GitErrorKind::NotARepo,
                    format!("no git work tree contains: {}", cwd.display()),
                ));
            }
            return Err(e);
        }
    };
    let top_path = Path::new(&top);
    // Independent rev-parse probes run concurrently (the TS implementation
    // used Promise.all); on large/dirty repos each spawn costs real time.
    let git_dir_args = vec!["rev-parse".to_string(), "--absolute-git-dir".to_string()];
    let common_args = vec![
        "rev-parse".to_string(),
        "--path-format=absolute".to_string(),
        "--git-common-dir".to_string(),
    ];
    let bare_args = vec!["rev-parse".to_string(), "--is-bare-repository".to_string()];
    let (git_dir_r, common_r, bare_r) = std::thread::scope(|scope| {
        let git_dir = scope.spawn(|| git_ok(top_path, &git_dir_args, opts));
        let common = scope.spawn(|| git_ok(top_path, &common_args, opts));
        let bare = scope.spawn(|| git_ok(top_path, &bare_args, opts));
        (
            git_dir.join().expect("git_dir probe panicked"),
            common.join().expect("common-dir probe panicked"),
            bare.join().expect("bare probe panicked"),
        )
    });
    let git_dir = git_dir_r?.trim().to_string();
    let common_dir = common_r?.trim().to_string();
    let bare = bare_r?.trim() == "true";
    if bare {
        return Err(GitError::new(
            GitErrorKind::BareRepo,
            format!("bare repository has no work tree: {top}"),
        ));
    }
    Ok(RepoInfo {
        root: top,
        git_dir,
        git_common_dir: common_dir,
        bare,
    })
}

/// Repository identity for cross-checkout grouping: the normalized
/// `origin` remote URL when present, else the repo root path. Mirrors
/// `normalizeRemoteUrl`.
pub fn normalize_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    // Scheme URL parse: host (lowercased, keeps non-default port) + path
    // minus trailing .git and slashes.
    if let Some(colon) = trimmed.find("://") {
        let rest = &trimmed[colon + 3..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let path_part = &rest[authority_end..];
        // Strip userinfo.
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        let host = hostport.to_lowercase();
        let path_clean = strip_git_suffix(path_part);
        return format!("{host}{path_clean}");
    }
    // scp-like `user@host:path`
    if let Some(at) = trimmed.find('@') {
        if let Some(colon) = trimmed[at + 1..].find(':') {
            let host = trimmed[at + 1..at + 1 + colon].to_lowercase();
            let path_clean = strip_git_suffix(&trimmed[at + 1 + colon + 1..]);
            return format!("{host}/{path_clean}");
        }
    }
    strip_git_suffix(trimmed)
}

fn strip_git_suffix(s: &str) -> String {
    let mut out = s.trim_end_matches('/').to_string();
    let lower = out.to_lowercase();
    if lower.ends_with(".git") {
        out.truncate(out.len() - 4);
    }
    out.trim_end_matches('/').to_string()
}

/// Fetch URL of `origin` (falling back to the first remote), normalized.
pub fn repo_remote_identity(
    repo_root: &Path,
    opts: &RunOptions,
) -> Result<Option<String>, GitError> {
    let names_out = git_ok(repo_root, &["remote".into()], opts)?;
    let names: Vec<&str> = names_out
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty())
        .collect();
    let preferred = if names.contains(&"origin") {
        "origin"
    } else {
        match names.first() {
            Some(n) => n,
            None => return Ok(None),
        }
    };
    let url = git_ok(
        repo_root,
        &["remote".into(), "get-url".into(), preferred.to_string()],
        opts,
    )?
    .trim()
    .to_string();
    Ok(Some(normalize_remote_url(&url)))
}

#[derive(Debug, Clone)]
pub struct BoundedResult {
    pub stdout: String,
    /// True when output hit the byte cap and the child was killed early.
    pub truncated: bool,
    /// UTF-8 byte length of the (possibly truncated) returned stdout.
    pub bytes: usize,
}

/// Shared bounded-stream runner used by `collectDiff` and
/// `run_git_bounded`. Streams stdout in chunks, kills the child once
/// `max_bytes` is exceeded, and reports truncation honestly. When the
/// child exits non-zero without truncation this returns a GitError
/// carrying stderr — mirroring `runGitBounded`/`collectDiff`.
fn run_stream_bounded(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: HashMap<String, String>,
    stdin_data: Option<&str>,
    timeout_ms: u64,
    max_bytes: usize,
    timeout_prefix: &str,
    argv_label: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BoundedResult, GitError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::new(GitErrorKind::GitMissing, "git executable not found on PATH")
        } else {
            GitError::new(GitErrorKind::GitError, e.to_string())
        }
    })?;
    if let Some(input) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            // Write on a thread so a large payload cannot deadlock
            // against the stdout/stderr readers spawned below.
            let data = input.to_string();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(data.as_bytes());
            });
        }
    }
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let child_id = child.id();

    // Chunked stdout reader: sends chunks until EOF.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stdout_pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // stderr reader into a shared buffer.
    let stderr_shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let stderr_shared2 = stderr_shared.clone();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        if let Ok(mut guard) = stderr_shared2.lock() {
            *guard = buf;
        }
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut chunks: Vec<u8> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut timed_out = false;
    let mut exited = false;

    loop {
        if truncated || timed_out || exited {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => {
                bytes += chunk.len();
                if bytes > max_bytes {
                    let keep = max_bytes.saturating_sub(bytes - chunk.len());
                    chunks.extend_from_slice(&chunk[..keep.min(chunk.len())]);
                    truncated = true;
                    unsafe {
                        libc::kill(child_id as i32, libc::SIGKILL);
                    }
                    break;
                }
                chunks.extend_from_slice(&chunk);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                match child.try_wait() {
                    Ok(Some(_)) => {
                        exited = true;
                        // Drain whatever remains buffered.
                        while let Ok(chunk) = rx.try_recv() {
                            bytes += chunk.len();
                            if bytes > max_bytes {
                                let keep = max_bytes.saturating_sub(bytes - chunk.len());
                                chunks.extend_from_slice(&chunk[..keep.min(chunk.len())]);
                                truncated = true;
                                break;
                            }
                            chunks.extend_from_slice(&chunk);
                        }
                    }
                    Ok(None) => {
                        if cancel
                            .as_ref()
                            .map(|c| c.load(Ordering::SeqCst))
                            .unwrap_or(false)
                        {
                            unsafe {
                                libc::kill(child_id as i32, libc::SIGKILL);
                            }
                            let _ = child.wait();
                            return Err(GitError::new(
                                GitErrorKind::Cancelled,
                                format!("{argv_label} cancelled"),
                            ));
                        }
                        if Instant::now() >= deadline {
                            timed_out = true;
                            unsafe {
                                libc::kill(child_id as i32, libc::SIGKILL);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().ok();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    let stderr = String::from_utf8_lossy(
        stderr_shared
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
            .as_slice(),
    )
    .to_string();

    if timed_out {
        return Err(GitError::new(
            GitErrorKind::Timeout,
            format!("{timeout_prefix} timed out after {timeout_ms}ms"),
        ));
    }
    let stdout = String::from_utf8_lossy(&chunks).to_string();
    if !truncated && code != 0 {
        let detail = {
            let s = stderr.trim();
            if !s.is_empty() {
                s.to_string()
            } else {
                format!("exit {code}")
            }
        };
        return Err(GitError::new(
            GitErrorKind::GitError,
            format!("{argv_label}: {detail}"),
        ));
    }
    Ok(BoundedResult {
        bytes: stdout.len(),
        stdout,
        truncated,
    })
}

/// Run `git <args>` streaming stdout with a hard byte cap. Mirrors
/// `runGitBounded`.
pub fn run_git_bounded(
    cwd: &Path,
    args: &[String],
    opts: &RunOptions,
    max_bytes: usize,
) -> Result<BoundedResult, GitError> {
    let timeout_ms = opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    run_stream_bounded(
        "git",
        args,
        cwd,
        git_env(&opts.env),
        opts.input.as_deref(),
        timeout_ms,
        max_bytes,
        &format!("git {}", args.first().map(|s| s.as_str()).unwrap_or("")),
        &format!("git {}", args.join(" ")),
        opts.cancel.clone(),
    )
}

// ---------------------------------------------------------------------------
// Collectors (port of git.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
}

/// Parse `git status --porcelain=v1` output into file-level counts.
pub fn parse_porcelain(output: &str) -> ChangeCounts {
    let mut counts = ChangeCounts::default();
    for line in output.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() < 3 {
            continue;
        }
        let x = chars[0];
        let y = chars[1];
        if x == '?' && y == '?' {
            counts.untracked += 1;
            continue;
        }
        if x == '!' && y == '!' {
            continue;
        }
        let unmerged = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
        if unmerged {
            counts.conflicted += 1;
            continue;
        }
        if x != ' ' && x != '?' {
            counts.staged += 1;
        }
        if y != ' ' && y != '?' {
            counts.unstaged += 1;
        }
    }
    counts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Typechange,
    Untracked,
    Conflicted,
    Ignored,
}

impl FileChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileChangeKind::Added => "added",
            FileChangeKind::Modified => "modified",
            FileChangeKind::Deleted => "deleted",
            FileChangeKind::Renamed => "renamed",
            FileChangeKind::Copied => "copied",
            FileChangeKind::Typechange => "typechange",
            FileChangeKind::Untracked => "untracked",
            FileChangeKind::Conflicted => "conflicted",
            FileChangeKind::Ignored => "ignored",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path relative to the repo root (the "to" side for renames/copies).
    pub path: String,
    /// Source path for renames/copies; None otherwise.
    pub old_path: Option<String>,
    /// Raw porcelain XY columns for clients that need the detail.
    pub index: String,
    pub worktree: String,
    pub kind: FileChangeKind,
    pub staged: bool,
    pub unstaged: bool,
    /// Binary detection from `git diff --numstat`: true/false for tracked
    /// changes, None when not determinable (untracked, conflicted).
    pub binary: Option<bool>,
}

impl FileChange {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "old_path": self.old_path,
            "oldPath": self.old_path,
            "index": self.index,
            "worktree": self.worktree,
            "kind": self.kind.as_str(),
            "staged": self.staged,
            "unstaged": self.unstaged,
            "binary": self.binary,
        })
    }
}

fn file_kind(x: char, y: char) -> FileChangeKind {
    if x == '?' && y == '?' {
        return FileChangeKind::Untracked;
    }
    if x == '!' && y == '!' {
        return FileChangeKind::Ignored;
    }
    if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
        return FileChangeKind::Conflicted;
    }
    if x == 'R' || y == 'R' {
        return FileChangeKind::Renamed;
    }
    if x == 'C' || y == 'C' {
        return FileChangeKind::Copied;
    }
    if x == 'A' || y == 'A' {
        return FileChangeKind::Added;
    }
    if x == 'D' || y == 'D' {
        return FileChangeKind::Deleted;
    }
    if x == 'T' || y == 'T' {
        return FileChangeKind::Typechange;
    }
    FileChangeKind::Modified
}

/// Parse `git status --porcelain=v1 -z` output. Mirrors `parsePorcelainZ`.
pub fn parse_porcelain_z(output: &str) -> Vec<FileChange> {
    let tokens: Vec<&str> = output.split('\0').filter(|t| !t.is_empty()).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        i += 1;
        let chars: Vec<char> = token.chars().collect();
        if chars.len() < 4 {
            continue;
        }
        let x = chars[0];
        let y = chars[1];
        let path: String = chars[3..].iter().collect();
        let mut old_path: Option<String> = None;
        if (x == 'R' || y == 'R' || x == 'C' || y == 'C') && i < tokens.len() {
            old_path = Some(tokens[i].to_string());
            i += 1;
        }
        let kind = file_kind(x, y);
        let tracked = !matches!(
            kind,
            FileChangeKind::Untracked | FileChangeKind::Conflicted | FileChangeKind::Ignored
        );
        entries.push(FileChange {
            path,
            old_path,
            index: x.to_string(),
            worktree: y.to_string(),
            kind,
            staged: tracked && x != ' ' && x != '?' && x != '!',
            unstaged: tracked && y != ' ' && y != '?' && y != '!',
            binary: None,
        });
    }
    entries
}

/// Parse `git diff --numstat -z` output. Mirrors `parseNumstatZ`.
pub fn parse_numstat_z(output: &str) -> (HashSet<String>, HashMap<String, (i64, i64)>) {
    let tokens: Vec<&str> = output.split('\0').filter(|t| !t.is_empty()).collect();
    let mut binary = HashSet::new();
    let mut stats = HashMap::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        i += 1;
        let mut parts = token.splitn(3, '\t');
        let added = parts.next().unwrap_or("");
        let deleted = parts.next().unwrap_or("");
        let inline = parts.next().unwrap_or("");
        // Validate numeric or '-'
        if !(added == "-" || added.parse::<i64>().is_ok())
            || !(deleted == "-" || deleted.parse::<i64>().is_ok())
        {
            continue;
        }
        let mut path = inline.to_string();
        if inline.is_empty() {
            // Rename/copy: `i` already points at the old path; the new
            // path is the next token. Consume both so the loop lands on
            // the following record's header.
            i += 1;
            path = tokens.get(i).copied().unwrap_or("").to_string();
            i += 1;
        }
        if path.is_empty() {
            continue;
        }
        if added == "-" {
            binary.insert(path);
        } else {
            stats.insert(
                path,
                (
                    added.parse::<i64>().unwrap_or(0),
                    deleted.parse::<i64>().unwrap_or(0),
                ),
            );
        }
    }
    (binary, stats)
}

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: Option<i64>,
    pub behind: Option<i64>,
}

fn upstream_ref(repo_root: &Path, opts: &RunOptions) -> Option<String> {
    let result = run_git(
        repo_root,
        &[
            "rev-parse".into(),
            "--abbrev-ref".into(),
            "--symbolic-full-name".into(),
            "@{upstream}".into(),
        ],
        opts,
    )
    .ok()?;
    if result.code != 0 {
        return None;
    }
    let reference = result.stdout.trim();
    if reference.is_empty() {
        None
    } else {
        Some(reference.to_string())
    }
}

fn ahead_behind(repo_root: &Path, opts: &RunOptions) -> Option<(i64, i64)> {
    let result = run_git(
        repo_root,
        &[
            "rev-list".into(),
            "--left-right".into(),
            "--count".into(),
            "HEAD...@{upstream}".into(),
        ],
        opts,
    )
    .ok()?;
    if result.code != 0 {
        return None;
    }
    let trimmed = result.stdout.trim();
    let mut parts = trimmed.split_whitespace();
    let ahead = parts.next()?.parse::<i64>().ok()?;
    let behind = parts.next()?.parse::<i64>().ok()?;
    Some((ahead, behind))
}

/// Collect branch name, detached state, upstream and ahead/behind counts.
pub fn collect_branch(repo_root: &Path, opts: &RunOptions) -> Result<BranchInfo, GitError> {
    let head = git_ok(
        repo_root,
        &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        opts,
    )?
    .trim()
    .to_string();
    let detached = head == "HEAD";
    let upstream = upstream_ref(repo_root, opts);
    let counts = if upstream.is_some() {
        ahead_behind(repo_root, opts)
    } else {
        None
    };
    Ok(BranchInfo {
        name: head,
        detached,
        upstream,
        ahead: counts.map(|c| c.0),
        behind: counts.map(|c| c.1),
    })
}

#[derive(Debug, Clone)]
pub struct StatusReport {
    pub repo: RepoInfo,
    pub branch: BranchInfo,
    pub changes: ChangeCounts,
}

pub fn collect_status(cwd: &Path, opts: &RunOptions) -> Result<StatusReport, GitError> {
    let repo = resolve_repo(cwd, opts)?;
    let root = Path::new(&repo.root);
    let porcelain_args = vec![
        "status".to_string(),
        "--porcelain=v1".to_string(),
        "--untracked-files=normal".to_string(),
    ];
    // Branch probes and the porcelain scan are independent — run them
    // concurrently like the TS Promise.all did.
    let (branch_r, porcelain_r) = std::thread::scope(|scope| {
        let branch = scope.spawn(|| collect_branch(root, opts));
        let porcelain = scope.spawn(|| git_ok(root, &porcelain_args, opts));
        (
            branch.join().expect("branch probe panicked"),
            porcelain.join().expect("porcelain probe panicked"),
        )
    });
    let branch = branch_r?;
    let porcelain = porcelain_r?;
    Ok(StatusReport {
        repo,
        branch,
        changes: parse_porcelain(&porcelain),
    })
}

#[derive(Debug, Clone, Default)]
pub struct FilesResult {
    pub files: Vec<FileChange>,
}

/// Structured per-file change list for the worktree + index.
pub fn collect_files(
    repo_root: &Path,
    pathspec: &[String],
    opts: &RunOptions,
) -> Result<FilesResult, GitError> {
    let mut args = vec![
        "status".into(),
        "--porcelain=v1".into(),
        "-z".into(),
        "--untracked-files=normal".into(),
    ];
    if !pathspec.is_empty() {
        args.push("--".into());
        args.extend(pathspec.iter().cloned());
    }
    let unstaged_args = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "-z".to_string(),
    ];
    let staged_args = vec![
        "diff".to_string(),
        "--numstat".to_string(),
        "-z".to_string(),
        "--cached".to_string(),
    ];
    // Porcelain scan and both numstat diffs are independent — the numstat
    // passes dominate on large diffs, so overlap them (TS used Promise.all).
    let (porcelain_r, unstaged_r, staged_r) = std::thread::scope(|scope| {
        let porcelain = scope.spawn(|| git_ok(repo_root, &args, opts));
        let unstaged = scope.spawn(|| git_ok(repo_root, &unstaged_args, opts));
        let staged = scope.spawn(|| git_ok(repo_root, &staged_args, opts));
        (
            porcelain.join().expect("porcelain probe panicked"),
            unstaged.join().expect("unstaged numstat panicked"),
            staged.join().expect("staged numstat panicked"),
        )
    });
    let porcelain = porcelain_r?;
    let unstaged = unstaged_r?;
    let staged = staged_r?;
    let mut files = parse_porcelain_z(&porcelain);
    let (bin_u, _) = parse_numstat_z(&unstaged);
    let (bin_s, _) = parse_numstat_z(&staged);
    let binary_paths: HashSet<String> = bin_u.union(&bin_s).cloned().collect();
    for file in files.iter_mut() {
        if matches!(
            file.kind,
            FileChangeKind::Untracked | FileChangeKind::Conflicted | FileChangeKind::Ignored
        ) {
            continue;
        }
        file.binary = Some(
            binary_paths.contains(&file.path)
                || file
                    .old_path
                    .as_ref()
                    .map(|o| binary_paths.contains(o))
                    .unwrap_or(false),
        );
    }
    Ok(FilesResult { files })
}

#[derive(Debug, Clone)]
pub struct LogCommit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_date: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: String,
    pub subject: String,
}

impl LogCommit {
    pub fn to_json(&self) -> Value {
        json!({
            "sha": self.sha,
            "parents": self.parents,
            "author": {
                "name": self.author_name,
                "email": self.author_email,
                "date": self.author_date,
            },
            "committer": {
                "name": self.committer_name,
                "email": self.committer_email,
                "date": self.committer_date,
            },
            "subject": self.subject,
        })
    }
}

const LOG_FORMAT: &str = "%x01%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%s";
const LOG_DEFAULT_LIMIT: i64 = 50;
const LOG_MAX_LIMIT: i64 = 1000;

/// Parse `git log --format` output (\x01 record sep, \x1f field sep).
pub fn parse_log_output(output: &str) -> Vec<LogCommit> {
    let mut commits = Vec::new();
    for record in output.split('\x01') {
        if record.is_empty() {
            continue;
        }
        let f: Vec<&str> = record.split('\x1f').collect();
        if f.len() < 9 {
            continue;
        }
        commits.push(LogCommit {
            sha: f[0].to_string(),
            parents: f[1]
                .split(' ')
                .filter(|p| !p.is_empty())
                .map(|s| s.to_string())
                .collect(),
            author_name: f[2].to_string(),
            author_email: f[3].to_string(),
            author_date: f[4].to_string(),
            committer_name: f[5].to_string(),
            committer_email: f[6].to_string(),
            committer_date: f[7].to_string(),
            subject: f[8].trim_end_matches('\n').to_string(),
        });
    }
    commits
}

#[derive(Debug, Clone, Default)]
pub struct LogRequest {
    pub limit: Option<i64>,
    pub skip: Option<i64>,
    pub all: bool,
    pub first_parent: bool,
    pub author: Option<String>,
    pub grep: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub pathspec: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LogResult {
    pub commits: Vec<LogCommit>,
    pub has_more: bool,
}

pub fn collect_log(
    repo_root: &Path,
    request: &LogRequest,
    opts: &RunOptions,
) -> Result<LogResult, GitError> {
    let limit = request
        .limit
        .unwrap_or(LOG_DEFAULT_LIMIT)
        .clamp(1, LOG_MAX_LIMIT);
    let mut args = vec![
        "log".to_string(),
        format!("--format={LOG_FORMAT}"),
        format!("--max-count={}", limit + 1),
    ];
    if let Some(skip) = request.skip {
        if skip > 0 {
            args.push(format!("--skip={skip}"));
        }
    }
    if request.all {
        args.push("--all".into());
    }
    if request.first_parent {
        args.push("--first-parent".into());
    }
    if let Some(author) = &request.author {
        args.push(format!("--author={author}"));
    }
    if let Some(grep) = &request.grep {
        args.push(format!("--grep={grep}"));
    }
    if let Some(since) = &request.since {
        args.push(format!("--since={since}"));
    }
    if let Some(until) = &request.until {
        args.push(format!("--until={until}"));
    }
    if !request.pathspec.is_empty() {
        args.push("--".into());
        args.extend(request.pathspec.iter().cloned());
    }
    let output = git_ok(repo_root, &args, opts)?;
    let commits = parse_log_output(&output);
    let has_more = commits.len() as i64 > limit;
    Ok(LogResult {
        commits: commits.into_iter().take(limit as usize).collect(),
        has_more,
    })
}

#[derive(Debug, Clone)]
pub struct NameStatusEntry {
    /// Raw status letters, e.g. "M", "A", "D", "R100".
    pub status: String,
    pub path: String,
    pub old_path: Option<String>,
}

/// Parse `git diff --name-status -z` output.
pub fn parse_name_status_z(output: &str) -> Vec<NameStatusEntry> {
    let tokens: Vec<&str> = output.split('\0').filter(|t| !t.is_empty()).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = tokens[i];
        i += 1;
        if status.is_empty() {
            continue;
        }
        if status.starts_with('R') || status.starts_with('C') {
            let old_path = tokens.get(i).copied().unwrap_or("");
            let path = tokens.get(i + 1).copied().unwrap_or("");
            i += 2;
            if !path.is_empty() {
                entries.push(NameStatusEntry {
                    status: status.to_string(),
                    path: path.to_string(),
                    old_path: if old_path.is_empty() {
                        None
                    } else {
                        Some(old_path.to_string())
                    },
                });
            }
        } else {
            let path = tokens.get(i).copied().unwrap_or("");
            i += 1;
            if !path.is_empty() {
                entries.push(NameStatusEntry {
                    status: status.to_string(),
                    path: path.to_string(),
                    old_path: None,
                });
            }
        }
    }
    entries
}

#[derive(Debug, Clone)]
pub struct CompareResult {
    /// Revision the comparison is anchored on (as requested).
    pub base: String,
    pub head: String,
    pub merge_base: String,
    pub ahead: i64,
    pub behind: i64,
    pub files: Vec<NameStatusEntry>,
    pub stat: String,
    pub patch: DiffResult,
}

/// Branch/revision comparison: `git diff <merge-base> <head>` plus
/// ahead/behind counts. `base` defaults to the upstream of `head`'s branch.
pub fn collect_compare(
    repo_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
    max_bytes: Option<usize>,
    opts: &RunOptions,
) -> Result<CompareResult, GitError> {
    let head = head.unwrap_or("HEAD").to_string();
    let base = match base {
        Some(b) => b.to_string(),
        None => upstream_ref(repo_root, opts).ok_or_else(|| {
            GitError::new(
                GitErrorKind::GitError,
                "no upstream configured; pass an explicit base revision",
            )
        })?,
    };
    let merge_base = git_ok(
        repo_root,
        &["merge-base".into(), base.clone(), head.clone()],
        opts,
    )?
    .trim()
    .split('\n')
    .next()
    .unwrap_or("")
    .to_string();
    let counts = run_git(
        repo_root,
        &[
            "rev-list".into(),
            "--left-right".into(),
            "--count".into(),
            format!("{base}...{head}"),
        ],
        opts,
    )?;
    let mut behind = 0i64;
    let mut ahead = 0i64;
    if counts.code == 0 {
        let mut parts = counts.stdout.split_whitespace();
        if let (Some(b), Some(a)) = (parts.next(), parts.next()) {
            behind = b.parse().unwrap_or(0);
            ahead = a.parse().unwrap_or(0);
        }
    }
    let name_status = git_ok(
        repo_root,
        &[
            "diff".into(),
            "--name-status".into(),
            "-z".into(),
            merge_base.clone(),
            head.clone(),
        ],
        opts,
    )?;
    let stat = git_ok(
        repo_root,
        &[
            "diff".into(),
            "--stat".into(),
            merge_base.clone(),
            head.clone(),
        ],
        opts,
    )?;
    let patch = collect_diff(
        repo_root,
        &DiffRequest {
            revisions: vec![merge_base.clone(), head.clone()],
            max_bytes,
            ..Default::default()
        },
        opts,
    )?;
    Ok(CompareResult {
        base,
        head,
        merge_base,
        ahead,
        behind,
        files: parse_name_status_z(&name_status),
        stat: stat.trim_end().to_string(),
        patch,
    })
}

/// Reject empty or option-like values that could inject git flags.
pub fn assert_safe_git_arg(value: &str, what: &str) -> Result<(), GitError> {
    if value.is_empty() || value.starts_with('-') {
        return Err(GitError::new(
            GitErrorKind::GitError,
            format!(
                "invalid {what}: {}",
                serde_json::to_string(value).unwrap_or_default()
            ),
        ));
    }
    Ok(())
}

fn safe_paths(paths: &[String]) -> Result<(), GitError> {
    for p in paths {
        assert_safe_git_arg(p, "path")?;
    }
    Ok(())
}

pub fn collect_stage(
    repo_root: &Path,
    all: bool,
    paths: &[String],
    opts: &RunOptions,
) -> Result<String, GitError> {
    safe_paths(paths)?;
    if !all && paths.is_empty() {
        return Err(GitError::new(
            GitErrorKind::GitError,
            "stage requires --all or at least one --path",
        ));
    }
    let args: Vec<String> = if all {
        vec!["add".into(), "-A".into()]
    } else {
        let mut a = vec!["add".into(), "--".into()];
        a.extend(paths.iter().cloned());
        a
    };
    Ok(git_ok(repo_root, &args, opts)?.trim().to_string())
}

pub fn collect_unstage(
    repo_root: &Path,
    all: bool,
    paths: &[String],
    opts: &RunOptions,
) -> Result<String, GitError> {
    safe_paths(paths)?;
    if !all && paths.is_empty() {
        return Err(GitError::new(
            GitErrorKind::GitError,
            "unstage requires --all or at least one --path",
        ));
    }
    let args: Vec<String> = if all {
        vec!["reset".into(), "-q".into()]
    } else {
        let mut a = vec!["restore".into(), "--staged".into(), "--".into()];
        a.extend(paths.iter().cloned());
        a
    };
    Ok(git_ok(repo_root, &args, opts)?.trim().to_string())
}

pub fn collect_commit(
    repo_root: &Path,
    messages: &[String],
    allow_empty: bool,
    opts: &RunOptions,
) -> Result<(String, String), GitError> {
    if messages.is_empty() {
        return Err(GitError::new(
            GitErrorKind::GitError,
            "commit requires at least one -m message",
        ));
    }
    let mut args = vec!["commit".to_string()];
    for message in messages {
        args.push("-m".into());
        args.push(message.clone());
    }
    if allow_empty {
        args.push("--allow-empty".into());
    }
    let summary = git_ok(repo_root, &args, opts)?.trim().to_string();
    let sha = git_ok(repo_root, &["rev-parse".into(), "HEAD".into()], opts)?
        .trim()
        .to_string();
    Ok((sha, summary))
}

pub fn collect_branch_create(
    repo_root: &Path,
    name: &str,
    switch_to: bool,
    opts: &RunOptions,
) -> Result<String, GitError> {
    assert_safe_git_arg(name, "branch name")?;
    let args: Vec<String> = if switch_to {
        vec!["switch".into(), "-c".into(), name.to_string()]
    } else {
        vec!["branch".into(), name.to_string()]
    };
    Ok(git_ok(repo_root, &args, opts)?.trim().to_string())
}

pub fn collect_branch_switch(
    repo_root: &Path,
    name: &str,
    opts: &RunOptions,
) -> Result<String, GitError> {
    assert_safe_git_arg(name, "branch name")?;
    Ok(
        git_ok(repo_root, &["switch".into(), name.to_string()], opts)?
            .trim()
            .to_string(),
    )
}

pub fn collect_upstream(
    repo_root: &Path,
    upstream: &str,
    opts: &RunOptions,
) -> Result<String, GitError> {
    assert_safe_git_arg(upstream, "upstream ref")?;
    Ok(git_ok(
        repo_root,
        &[
            "branch".into(),
            "--set-upstream-to".into(),
            upstream.to_string(),
        ],
        opts,
    )?
    .trim()
    .to_string())
}

pub fn collect_fetch(
    repo_root: &Path,
    remote: Option<&str>,
    prune: bool,
    opts: &RunOptions,
) -> Result<String, GitError> {
    if let Some(r) = remote {
        assert_safe_git_arg(r, "remote")?;
    }
    let mut args = vec!["fetch".to_string()];
    if prune {
        args.push("--prune".into());
    }
    if let Some(r) = remote {
        args.push(r.to_string());
    }
    Ok(git_ok(repo_root, &args, opts)?.trim().to_string())
}

pub fn collect_push(
    repo_root: &Path,
    remote: Option<&str>,
    refspec: Option<&str>,
    set_upstream: bool,
    force_with_lease: bool,
    opts: &RunOptions,
) -> Result<String, GitError> {
    if let Some(r) = remote {
        assert_safe_git_arg(r, "remote")?;
    }
    if let Some(r) = refspec {
        assert_safe_git_arg(r, "refspec")?;
    }
    let mut args = vec!["push".to_string()];
    if set_upstream {
        args.push("--set-upstream".into());
    }
    if force_with_lease {
        args.push("--force-with-lease".into());
    }
    if let Some(r) = remote {
        args.push(r.to_string());
    }
    if let Some(r) = refspec {
        args.push(r.to_string());
    }
    Ok(git_ok(repo_root, &args, opts)?.trim().to_string())
}

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    /// Fetch URL with any embedded credentials stripped.
    pub url: String,
    /// Push URL with any embedded credentials stripped; falls back to `url`.
    pub push_url: String,
}

/// Remove userinfo (and any token in it) from a remote URL. Mirrors
/// `sanitizeRemoteUrl`: parseable scheme URLs lose userinfo; scp-like and
/// unparsable forms are returned verbatim.
pub fn sanitize_remote_url(url: &str) -> String {
    if let Some(colon) = url.find("://") {
        let rest = &url[colon + 3..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let path = &rest[authority_end..];
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        return format!("{}{}", hostport, path);
    }
    url.to_string()
}

pub fn collect_remotes(repo_root: &Path, opts: &RunOptions) -> Result<Vec<RemoteInfo>, GitError> {
    let names_out = git_ok(repo_root, &["remote".into()], opts)?;
    let names: Vec<String> = names_out
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut remotes = Vec::new();
    for name in names {
        let url = git_ok(
            repo_root,
            &["remote".into(), "get-url".into(), name.clone()],
            opts,
        )?
        .trim()
        .to_string();
        let push = run_git(
            repo_root,
            &[
                "remote".into(),
                "get-url".into(),
                "--push".into(),
                name.clone(),
            ],
            opts,
        )?;
        let push_url = if push.code == 0 {
            push.stdout.trim().to_string()
        } else {
            String::new()
        };
        remotes.push(RemoteInfo {
            name,
            url: sanitize_remote_url(&url),
            push_url: sanitize_remote_url(if push_url.is_empty() { &url } else { &push_url }),
        });
    }
    Ok(remotes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFormat {
    Patch,
    Stat,
    NameStatus,
    Numstat,
}

impl DiffFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiffFormat::Patch => "patch",
            DiffFormat::Stat => "stat",
            DiffFormat::NameStatus => "name-status",
            DiffFormat::Numstat => "numstat",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffRequest {
    pub staged: bool,
    pub stat: bool,
    pub format: Option<DiffFormat>,
    pub revisions: Vec<String>,
    /// Pathspec limiting the diff (`git diff -- <path>...`).
    pub paths: Vec<String>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DiffResult {
    pub output: String,
    pub truncated: bool,
    pub bytes: usize,
}

const DIFF_DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// Run `git diff` bounded by `max_bytes`. Mirrors `collectDiff`.
pub fn collect_diff(
    repo_root: &Path,
    request: &DiffRequest,
    opts: &RunOptions,
) -> Result<DiffResult, GitError> {
    let max_bytes = request.max_bytes.unwrap_or(DIFF_DEFAULT_MAX_BYTES);
    let format = request.format.unwrap_or(if request.stat {
        DiffFormat::Stat
    } else {
        DiffFormat::Patch
    });
    let mut args = vec![
        "diff".to_string(),
        "--no-color".into(),
        "--no-ext-diff".into(),
    ];
    if request.staged {
        args.push("--staged".into());
    }
    match format {
        DiffFormat::Stat => args.push("--stat".into()),
        DiffFormat::NameStatus => {
            args.push("--name-status".into());
            args.push("-z".into());
        }
        DiffFormat::Numstat => {
            args.push("--numstat".into());
            args.push("-z".into());
        }
        DiffFormat::Patch => {}
    }
    if !request.revisions.is_empty() {
        args.extend(request.revisions.iter().cloned());
    }
    if !request.paths.is_empty() {
        args.push("--".into());
        args.extend(request.paths.iter().cloned());
    }
    let timeout_ms = opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let result = run_stream_bounded(
        "git",
        &args,
        repo_root,
        git_env(&opts.env),
        None,
        timeout_ms,
        max_bytes,
        "git diff",
        "git diff",
        opts.cancel.clone(),
    )?;
    Ok(DiffResult {
        output: result.stdout,
        truncated: result.truncated,
        bytes: result.bytes,
    })
}

// ---------------------------------------------------------------------------
// TUI-only collectors (port of tui/repo.ts)
// ---------------------------------------------------------------------------

const BRANCH_FORMAT: &str =
    "%(refname:short)%09%(HEAD)%09%(upstream:short)%09%(upstream:track)%09%(objectname)";

/// One local branch row from `git for-each-ref` (see tui/model parseBranches).
#[derive(Debug, Clone)]
pub struct BranchEntry {
    pub name: String,
    pub sha: String,
    pub current: bool,
    pub upstream: Option<String>,
    pub track: Option<String>,
}

/// List local branches (`refs/heads`), newest first, with current marker,
/// upstream, and `[ahead/behind]` track info when available.
pub fn collect_branches(repo_root: &Path, opts: &RunOptions) -> Result<Vec<BranchEntry>, GitError> {
    let out = git_ok(
        repo_root,
        &[
            "for-each-ref".to_string(),
            format!("--format={BRANCH_FORMAT}"),
            "--sort=-committerdate".to_string(),
            "refs/heads".to_string(),
        ],
        opts,
    )?;
    Ok(parse_branches(&out))
}

/// Parse `for-each-ref` tab-separated output (one branch per line).
/// Port of `parseBranches` in tui/model.ts.
pub fn parse_branches(output: &str) -> Vec<BranchEntry> {
    let mut out = Vec::new();
    for line in output.split('\n') {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let Some(name) = f.first() else { continue };
        if name.is_empty() {
            continue;
        }
        let get = |i: usize| f.get(i).copied().unwrap_or("");
        out.push(BranchEntry {
            name: name.to_string(),
            sha: get(4).to_string(),
            current: get(1) == "*",
            upstream: {
                let u = get(2);
                if u.is_empty() {
                    None
                } else {
                    Some(u.to_string())
                }
            },
            track: {
                let t = get(3);
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            },
        });
    }
    out
}

/// Current HEAD sha; errors on unborn branches / failures.
pub fn head_sha(repo_root: &Path, opts: &RunOptions) -> Result<String, GitError> {
    Ok(
        git_ok(repo_root, &["rev-parse".into(), "HEAD".into()], opts)?
            .trim()
            .to_string(),
    )
}

/// HEAD sha or null on unborn branches / any failure (never throws).
pub fn head_sha_or_null(repo_root: &Path, opts: &RunOptions) -> Option<String> {
    head_sha(repo_root, opts).ok()
}

/// Current branch name via `symbolic-ref` — correct on unborn branches
/// where `rev-parse --abbrev-ref HEAD` reports "HEAD". Returns "HEAD"
/// for detached HEAD, null when even symbolic-ref fails.
pub fn current_branch_name(repo_root: &Path, opts: &RunOptions) -> Option<String> {
    let sym = run_git(
        repo_root,
        &[
            "symbolic-ref".into(),
            "--short".into(),
            "-q".into(),
            "HEAD".into(),
        ],
        opts,
    )
    .ok()?;
    if sym.code == 0 {
        let name = sym.stdout.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let head = run_git(
        repo_root,
        &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        opts,
    )
    .ok()?;
    if head.code == 0 {
        let name = head.stdout.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Bounded `git show` for a single commit (metadata + patch), used for
/// the log detail view. Throws GitError("git_error") when the output
/// exceeds the cap — callers show the honest error rather than a
/// silently truncated patch.
pub fn collect_commit_show(
    repo_root: &Path,
    sha: &str,
    opts: &RunOptions,
) -> Result<String, GitError> {
    let valid = sha.len() >= 4 && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return Err(GitError::new(
            GitErrorKind::GitError,
            format!(
                "invalid commit sha: {}",
                serde_json::to_string(sha).unwrap_or_default()
            ),
        ));
    }
    let mut o = opts.clone();
    o.timeout_ms = Some(opts.timeout_ms.unwrap_or(10_000));
    o.max_buffer = Some(opts.max_buffer.unwrap_or(512 * 1024));
    let result = run_git(
        repo_root,
        &[
            "show".into(),
            "--no-color".into(),
            "--format=fuller".into(),
            sha.to_string(),
        ],
        &o,
    )?;
    if result.code != 0 {
        let detail = {
            let stderr = result.stderr.trim();
            let stdout = result.stdout.trim();
            if !stderr.is_empty() {
                stderr.to_string()
            } else if !stdout.is_empty() {
                stdout.to_string()
            } else {
                format!("exit {}", result.code)
            }
        };
        return Err(GitError::new(
            GitErrorKind::GitError,
            format!("git show {}: {}", &sha[..sha.len().min(12)], detail),
        ));
    }
    Ok(result.stdout)
}
