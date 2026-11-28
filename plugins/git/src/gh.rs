//! Forge access for rebuildup.git — GitHub pull requests and issues via
//! the host's `gh` CLI (ADR-0008). Rust port of `plugins/git/src/gh.ts`.
//!
//! Same spawn discipline as git.rs: argv only, no shell, hard timeout,
//! prompt suppression. Authentication and host selection come from the
//! user's existing `gh auth` configuration — the plugin never handles
//! tokens itself.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhErrorKind {
    GhMissing,
    Timeout,
    GhError,
    StaleHead,
    MergeBlocked,
    ReleaseBoundary,
}

impl GhErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GhErrorKind::GhMissing => "gh_missing",
            GhErrorKind::Timeout => "timeout",
            GhErrorKind::GhError => "gh_error",
            GhErrorKind::StaleHead => "stale_head",
            GhErrorKind::MergeBlocked => "merge_blocked",
            GhErrorKind::ReleaseBoundary => "release_boundary",
        }
    }
}

#[derive(Debug)]
pub struct GhError {
    pub kind: GhErrorKind,
    pub message: String,
}

impl GhError {
    pub fn new(kind: GhErrorKind, message: impl Into<String>) -> Self {
        GhError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GhError {}

#[derive(Debug, Clone)]
pub struct GhResult {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Floor for `gh --version`; older releases lack stable --json surfaces.
pub const MIN_GH_VERSION: &str = "2.81.0";
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_BUFFER: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct GhOptions {
    pub timeout_ms: Option<u64>,
    pub max_buffer: Option<usize>,
    /// Body text piped to the child's stdin (`--body-file -`, `gh api
    /// --input -`).
    pub input: Option<String>,
    /// Resolved body text for `pr/issue create` when `--body-file` was
    /// used (file already read into memory).
    pub body_text: Option<String>,
}

impl GhOptions {
    fn timeout(&self) -> u64 {
        self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)
    }
}

/// Run `gh <args>` in `cwd`. Resolves on any exit code; rejects on spawn
/// failure, timeout, or output exceeding `max_buffer`. Mirrors `runGh`.
pub fn run_gh(cwd: &Path, args: &[String], opts: &GhOptions) -> Result<GhResult, GhError> {
    let timeout_ms = opts.timeout();
    let max_buffer = opts.max_buffer.unwrap_or(DEFAULT_MAX_BUFFER);
    let env = crate::gitenv::gh_child_env(&[
        ("GH_PROMPT_DISABLED".into(), "1".into()),
        ("LC_ALL".into(), "C".into()),
    ]);
    let mut command = Command::new("gh");
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
            GhError::new(GhErrorKind::GhMissing, "gh executable not found on PATH")
        } else {
            GhError::new(GhErrorKind::GhError, e.to_string())
        }
    })?;
    if let Some(input) = &opts.input {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(input.as_bytes());
        }
    }
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        tx_out.send(buf).ok();
    });
    let (tx_err, rx_err) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        tx_err.send(buf).ok();
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GhError::new(
                        GhErrorKind::Timeout,
                        format!(
                            "gh {} timed out after {timeout_ms}ms",
                            args.first().map(|s| s.as_str()).unwrap_or("")
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(GhError::new(GhErrorKind::GhError, e.to_string())),
        }
    };
    let stdout_bytes = rx_out.recv().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&rx_err.recv().unwrap_or_default()).to_string();
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    if stdout.len() > max_buffer {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "gh output exceeded buffer limit",
        ));
    }
    Ok(GhResult {
        stdout,
        stderr,
        code: status.code().unwrap_or(-1),
    })
}

/// Run gh and require exit code 0; map failures to GhError. Mirrors `ghOk`.
pub fn gh_ok(cwd: &Path, args: &[String], opts: &GhOptions) -> Result<String, GhError> {
    let result = run_gh(cwd, args, opts)?;
    if result.code != 0 {
        let stderr = result.stderr.trim();
        let stdout = result.stdout.trim();
        let detail = if !stderr.is_empty() {
            stderr.to_string()
        } else if !stdout.is_empty() {
            stdout.to_string()
        } else {
            format!("exit {}", result.code)
        };
        // Only the subcommand words go into the message — argv may carry
        // user content that must not be echoed back.
        let sub = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!("gh {sub}: {detail}"),
        ));
    }
    Ok(result.stdout)
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    // "gh version 2.81.0" — first occurrence of three dotted numbers.
    let idx = text.find("gh version ")?;
    let rest = &text[idx + "gh version ".len()..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

pub fn version_at_least(version: &str, minimum: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let t = v.trim();
        let mut parts = t.split('.');
        let a = parts.next()?.parse().ok()?;
        let b = parts.next()?.parse().ok()?;
        let c = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some((a, b, c))
    };
    let (Some(a), Some(b)) = (parse(version), parse(minimum)) else {
        return false;
    };
    a > b
}

/// Verify `gh` exists, meets MIN_GH_VERSION, and is authenticated.
pub fn ensure_gh(cwd: &Path, opts: &GhOptions) -> Result<(), GhError> {
    let version_out = gh_ok(cwd, &["--version".to_string()], opts)?;
    let Some(parsed) = parse_version(&version_out) else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!("unrecognized gh --version output: {}", version_out.trim()),
        ));
    };
    let version = format!("{}.{}.{}", parsed.0, parsed.1, parsed.2);
    if !version_at_least(&version, MIN_GH_VERSION) {
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!("gh {version} is below the required minimum {MIN_GH_VERSION}"),
        ));
    }
    let auth = run_gh(cwd, &["auth".to_string(), "status".to_string()], opts)?;
    if auth.code != 0 {
        let detail = if !auth.stderr.trim().is_empty() {
            auth.stderr.trim().to_string()
        } else {
            auth.stdout.trim().to_string()
        };
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!(
                "gh is not authenticated: {}",
                if detail.is_empty() {
                    format!("exit {}", auth.code)
                } else {
                    detail
                }
            ),
        ));
    }
    Ok(())
}

/// Resolve the forge repo (`owner/name`) for a pane repository.
pub fn resolve_forge_repo(
    repo_root: &Path,
    repo_override: Option<&str>,
    opts: &GhOptions,
) -> Result<String, GhError> {
    if let Some(repo) = repo_override {
        if !is_owner_repo(repo) {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("invalid --repo value: {}", json_string(repo)),
            ));
        }
        return Ok(repo.to_string());
    }
    let out = gh_ok(
        repo_root,
        &[
            "repo".to_string(),
            "view".to_string(),
            "--json".to_string(),
            "nameWithOwner".to_string(),
            "-q".to_string(),
            ".nameWithOwner".to_string(),
        ],
        opts,
    )?;
    let repo = out.trim().to_string();
    if !is_owner_repo(&repo) {
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!("could not resolve forge repo for {}", repo_root.display()),
        ));
    }
    Ok(repo)
}

fn is_owner_repo(repo: &str) -> bool {
    // ^[^\s/]+\/[^\s/]+$
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None) => {
            !a.is_empty()
                && !b.is_empty()
                && !a.chars().any(|c| c.is_whitespace())
                && !b.chars().any(|c| c.is_whitespace())
        }
        _ => false,
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

const PR_LIST_FIELDS: &str =
    "number,title,state,isDraft,author,headRefName,baseRefName,updatedAt,url";

const PR_VIEW_FIELDS: &str = "number,title,state,isDraft,author,headRefName,baseRefName,mergeable,mergeStateStatus,reviewDecision,url,body,additions,deletions,changedFiles,createdAt,updatedAt,statusCheckRollup,reviews";

const BODY_MAX_CHARS: usize = 8000;
const REVIEWS_MAX: usize = 10;
const CHECKS_MAX: usize = 50;

fn bound_pr_view(mut raw: Value) -> Value {
    let Some(obj) = raw.as_object_mut() else {
        return raw;
    };
    if let Some(body) = obj.get("body").and_then(|v| v.as_str()) {
        if body.chars().count() > BODY_MAX_CHARS {
            let truncated: String = body.chars().take(BODY_MAX_CHARS).collect();
            obj.insert("body".into(), json!(format!("{truncated}…")));
            obj.insert("body_truncated".into(), json!(true));
        }
    }
    if let Some(reviews) = obj.get("reviews").and_then(|v| v.as_array()).cloned() {
        let mut mapped: Vec<Value> = reviews
            .iter()
            .rev()
            .take(REVIEWS_MAX)
            .map(|r| {
                json!({
                    "author": r.get("author").and_then(|a| a.get("login")).cloned().unwrap_or(Value::Null),
                    "state": r.get("state").cloned().unwrap_or(Value::Null),
                    "submittedAt": r.get("submittedAt").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        mapped.reverse();
        obj.insert("reviews".into(), json!(mapped));
    }
    if let Some(rollup) = obj
        .get("statusCheckRollup")
        .and_then(|v| v.as_array())
        .cloned()
    {
        let len = rollup.len();
        let mapped: Vec<Value> = rollup
            .iter()
            .take(CHECKS_MAX)
            .map(|c| {
                json!({
                    "name": c.get("name").cloned().unwrap_or(Value::Null),
                    "status": c.get("status").cloned().unwrap_or(Value::Null),
                    "conclusion": c.get("conclusion").cloned().unwrap_or(Value::Null),
                    "workflow": c.get("workflowName").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        obj.insert("statusCheckRollup".into(), json!(mapped));
        obj.insert("checks_truncated".into(), json!(len > CHECKS_MAX));
    }
    raw
}

pub fn collect_pr_list(
    repo_root: &Path,
    repo: &str,
    limit: Option<i64>,
    state: Option<&str>,
    head: Option<&str>,
    opts: &GhOptions,
) -> Result<Vec<Value>, GhError> {
    let mut args = vec![
        "pr".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--json".to_string(),
        PR_LIST_FIELDS.to_string(),
    ];
    if let Some(limit) = limit {
        args.push("--limit".to_string());
        args.push(limit.to_string());
    }
    if let Some(state) = state {
        args.push("--state".to_string());
        args.push(state.to_string());
    }
    if let Some(head) = head {
        args.push("--head".to_string());
        args.push(head.to_string());
    }
    let out = gh_ok(repo_root, &args, opts)?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh pr list: invalid JSON: {e}"),
        )
    })?;
    Ok(parsed.as_array().cloned().unwrap_or_default())
}

pub fn collect_pr_view(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let out = gh_ok(
        repo_root,
        &[
            "pr".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--json".to_string(),
            PR_VIEW_FIELDS.to_string(),
        ],
        opts,
    )?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh pr view: invalid JSON: {e}"),
        )
    })?;
    Ok(bound_pr_view(parsed))
}

/// `pr status`: find the open PR for the current branch, then return its
/// bounded view. None when the branch has no open PR.
pub fn collect_pr_status(
    repo_root: &Path,
    repo: &str,
    branch: &str,
    opts: &GhOptions,
) -> Result<Option<Value>, GhError> {
    let list_out = gh_ok(
        repo_root,
        &[
            "pr".to_string(),
            "list".to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--head".to_string(),
            branch.to_string(),
            "--state".to_string(),
            "open".to_string(),
            "--json".to_string(),
            "number".to_string(),
            "--limit".to_string(),
            "1".to_string(),
        ],
        opts,
    )?;
    let list: Value = serde_json::from_str(&list_out).unwrap_or(Value::Null);
    let number = list
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e.get("number"))
        .and_then(|n| n.as_i64());
    match number {
        Some(n) => Ok(Some(collect_pr_view(repo_root, repo, n, opts)?)),
        None => Ok(None),
    }
}

pub fn collect_pr_create(
    repo_root: &Path,
    repo: &str,
    title: &str,
    body: Option<&str>,
    body_file: Option<&str>,
    draft: bool,
    base: Option<&str>,
    head: Option<&str>,
    opts: &GhOptions,
) -> Result<(String, Option<i64>), GhError> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--title".to_string(),
        title.to_string(),
    ];
    if body_file == Some("-") {
        args.push("--body-file".to_string());
        args.push("-".to_string());
    } else if let Some(bf) = body_file {
        args.push("--body-file".to_string());
        args.push(bf.to_string());
    } else {
        // gh prompts for a body when neither flag is given; pass an
        // explicit empty body to stay non-interactive.
        args.push("--body".to_string());
        args.push(body.unwrap_or("").to_string());
    }
    if draft {
        args.push("--draft".to_string());
    }
    if let Some(b) = base {
        args.push("--base".to_string());
        args.push(b.to_string());
    }
    if let Some(h) = head {
        args.push("--head".to_string());
        args.push(h.to_string());
    }
    let mut call_opts = opts.clone();
    if body_file == Some("-") {
        call_opts.input = opts.body_text.clone();
    }
    let out = gh_ok(repo_root, &args, &call_opts)?;
    let url = out.trim().lines().last().unwrap_or("").to_string();
    let number = extract_trailing_number(&url, "/pull/");
    Ok((url, number))
}

fn extract_trailing_number(url: &str, marker: &str) -> Option<i64> {
    let idx = url.rfind(marker)?;
    let rest = &url[idx + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

// ---------------------------------------------------------------------------
// PR review/read extensions (Issue #171, read-only, ADR-0008).
// ---------------------------------------------------------------------------

const GH_API_PAGE: usize = 100;
const PR_DETAIL_ITEMS_MAX: usize = 50;
const PR_BODY_MAX_CHARS: usize = 4000;
const PR_DIFF_HUNK_MAX_CHARS: usize = 8000;
const PR_CHECKS_MAX: usize = 100;
const PR_LOG_LINES_DEFAULT: i64 = 200;
const PR_LOG_LINES_MAX: i64 = 2000;
const PR_LOG_BYTES_MAX: usize = 256 * 1024;
const PR_LOG_FETCH_MAX_BUFFER: usize = 64 * 1024 * 1024;
const PR_RUNS_MAX: usize = 20;
const PR_RUN_JOBS_MAX: usize = 50;

const PR_CHECKS_FIELDS: &str =
    "bucket,completedAt,description,event,link,name,startedAt,state,workflow";
const PR_RUN_LIST_FIELDS: &str =
    "databaseId,name,workflowName,status,conclusion,url,headSha,createdAt,event,displayTitle";
const PR_RUN_VIEW_FIELDS: &str =
    "databaseId,name,workflowName,status,conclusion,url,headSha,createdAt,jobs";

fn gh_api_json(
    repo_root: &Path,
    repo: &str,
    endpoint: &str,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let out = gh_ok(
        repo_root,
        &["api".to_string(), format!("repos/{repo}/{endpoint}")],
        opts,
    )?;
    serde_json::from_str(&out)
        .map_err(|e| GhError::new(GhErrorKind::GhError, format!("gh api: invalid JSON: {e}")))
}

/// `gh api` mutation with a JSON body piped through stdin (`--input -`).
fn gh_api_call(
    repo_root: &Path,
    repo: &str,
    endpoint: &str,
    method: &str,
    payload: Option<&Value>,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let mut args = vec![
        "api".to_string(),
        format!("repos/{repo}/{endpoint}"),
        "--method".to_string(),
        method.to_string(),
    ];
    let mut call_opts = opts.clone();
    if let Some(p) = payload {
        args.push("--input".to_string());
        args.push("-".to_string());
        call_opts.input = Some(serde_json::to_string(p).unwrap_or_default());
    }
    let out = gh_ok(repo_root, &args, &call_opts)?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        if method == "DELETE" {
            return Ok(json!({}));
        }
        return Err(GhError::new(
            GhErrorKind::GhError,
            format!("gh api {endpoint}: empty response body"),
        ));
    }
    serde_json::from_str(trimmed)
        .map_err(|e| GhError::new(GhErrorKind::GhError, format!("gh api: invalid JSON: {e}")))
}

/// Truncate long text bodies; `*_truncated` is set only when cut.
fn bound_body(value: Option<&Value>, max_chars: usize) -> (Option<String>, bool) {
    let Some(s) = value.and_then(|v| v.as_str()) else {
        return (None, false);
    };
    if s.chars().count() <= max_chars {
        return (Some(s.to_string()), false);
    }
    let truncated: String = s.chars().take(max_chars).collect();
    (Some(format!("{truncated}…")), true)
}

/// Keep the newest `max` entries (REST lists are chronological ascending).
/// `truncated` is honest-conservative: a full page may hide further pages.
fn bound_page<T: Clone>(raw: &[T], max: usize) -> (Vec<T>, bool) {
    let start = raw.len().saturating_sub(max);
    (
        raw[start..].to_vec(),
        raw.len() > max || raw.len() >= GH_API_PAGE,
    )
}

struct PrRefs {
    head: Value,
    base: Value,
    requested_reviewers: Vec<Value>,
    requested_teams: Vec<Value>,
}

/// `GET pulls/{n}` — head/base freshness plus review requests.
fn fetch_pr_refs(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<PrRefs, GhError> {
    let raw = gh_api_json(repo_root, repo, &format!("pulls/{number}"), opts)?;
    let head = raw.get("head").cloned().unwrap_or(Value::Null);
    let base = raw.get("base").cloned().unwrap_or(Value::Null);
    let reviewers = raw
        .get("requested_reviewers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let teams = raw
        .get("requested_teams")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(PrRefs {
        head: json!({
            "ref": head.get("ref").cloned().unwrap_or(Value::Null),
            "sha": head.get("sha").cloned().unwrap_or(Value::Null),
        }),
        base: json!({
            "ref": base.get("ref").cloned().unwrap_or(Value::Null),
            "sha": base.get("sha").cloned().unwrap_or(Value::Null),
        }),
        requested_reviewers: reviewers,
        requested_teams: teams,
    })
}

/// `pr reviews`: submitted reviews plus requested reviewers/teams.
pub fn collect_pr_reviews(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = fetch_pr_refs(repo_root, repo, number, opts)?;
    let raw = gh_api_json(
        repo_root,
        repo,
        &format!("pulls/{number}/reviews?per_page={GH_API_PAGE}"),
        opts,
    )?;
    let Some(arr) = raw.as_array() else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "unexpected gh api response for pull request reviews",
        ));
    };
    let reviews: Vec<Value> = arr
        .iter()
        .map(|entry| {
            let r = entry.as_object();
            let get = |k: &str| r.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            let (body, truncated) = bound_body(r.and_then(|o| o.get("body")), PR_BODY_MAX_CHARS);
            let mut item = json!({
                "id": get("id"),
                "author": get("user").get("login").cloned().unwrap_or(Value::Null),
                "state": get("state"),
                "body": body,
                "submitted_at": get("submitted_at"),
                "commit_id": get("commit_id"),
                "url": get("html_url"),
            });
            if truncated {
                item["body_truncated"] = json!(true);
            }
            item
        })
        .collect();
    let (bounded, truncated_flag) = bound_page(&reviews, PR_DETAIL_ITEMS_MAX);
    Ok(json!({
        "number": number,
        "head": refs.head,
        "base": refs.base,
        "reviews": bounded,
        "review_requests": {
            "users": refs.requested_reviewers.iter().map(|u| u.get("login").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
            "teams": refs.requested_teams.iter().map(|t| {
                let name = t.get("name").and_then(|v| v.as_str());
                let slug = t.get("slug").and_then(|v| v.as_str());
                json!(name.or(slug))
            }).collect::<Vec<_>>(),
        },
        "reviews_truncated": truncated_flag,
    }))
}

/// `pr comments`: PR conversation and line-level review comments.
pub fn collect_pr_comments(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = fetch_pr_refs(repo_root, repo, number, opts)?;
    let issue_raw = gh_api_json(
        repo_root,
        repo,
        &format!("issues/{number}/comments?per_page={GH_API_PAGE}"),
        opts,
    )?;
    let line_raw = gh_api_json(
        repo_root,
        repo,
        &format!("pulls/{number}/comments?per_page={GH_API_PAGE}"),
        opts,
    )?;
    let (Some(issue_arr), Some(line_arr)) = (issue_raw.as_array(), line_raw.as_array()) else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "unexpected gh api response for pull request comments",
        ));
    };

    let issue_comments: Vec<Value> = issue_arr
        .iter()
        .map(|entry| {
            let c = entry.as_object();
            let get = |k: &str| c.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            let (body, truncated) = bound_body(c.and_then(|o| o.get("body")), PR_BODY_MAX_CHARS);
            let mut item = json!({
                "id": get("id"),
                "author": get("user").get("login").cloned().unwrap_or(Value::Null),
                "body": body,
                "created_at": get("created_at"),
                "updated_at": get("updated_at"),
                "url": get("html_url"),
            });
            if truncated {
                item["body_truncated"] = json!(true);
            }
            item
        })
        .collect();
    let (bounded_issue, issue_trunc) = bound_page(&issue_comments, PR_DETAIL_ITEMS_MAX);

    let line_comments: Vec<Value> = line_arr
        .iter()
        .map(|entry| {
            let c = entry.as_object();
            let get = |k: &str| c.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            let (body, body_trunc) = bound_body(c.and_then(|o| o.get("body")), PR_BODY_MAX_CHARS);
            let (hunk, hunk_trunc) =
                bound_body(c.and_then(|o| o.get("diff_hunk")), PR_DIFF_HUNK_MAX_CHARS);
            let mut item = json!({
                "id": get("id"),
                "author": get("user").get("login").cloned().unwrap_or(Value::Null),
                "path": get("path"),
                "line": get("line"),
                "original_line": get("original_line"),
                "side": get("side"),
                "in_reply_to_id": get("in_reply_to_id"),
                "body": body,
                "diff_hunk": hunk,
                "created_at": get("created_at"),
                "updated_at": get("updated_at"),
                "url": get("html_url"),
            });
            if body_trunc {
                item["body_truncated"] = json!(true);
            }
            if hunk_trunc {
                item["diff_hunk_truncated"] = json!(true);
            }
            item
        })
        .collect();
    let (bounded_line, line_trunc) = bound_page(&line_comments, PR_DETAIL_ITEMS_MAX);

    Ok(json!({
        "number": number,
        "head": refs.head,
        "base": refs.base,
        "issue_comments": bounded_issue,
        "line_comments": bounded_line,
        "issue_comments_truncated": issue_trunc,
        "line_comments_truncated": line_trunc,
    }))
}

/// `pr checks`: per-check detail via `gh pr checks --json`.
pub fn collect_pr_checks(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = fetch_pr_refs(repo_root, repo, number, opts)?;
    let raw_text = match gh_ok(
        repo_root,
        &[
            "pr".to_string(),
            "checks".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--json".to_string(),
            PR_CHECKS_FIELDS.to_string(),
        ],
        opts,
    ) {
        Ok(t) => t,
        Err(e) => {
            if e.kind == GhErrorKind::GhError && e.message.contains("no checks reported") {
                "[]".to_string()
            } else {
                return Err(e);
            }
        }
    };
    let parsed: Value = serde_json::from_str(&raw_text).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh pr checks: invalid JSON: {e}"),
        )
    })?;
    let arr = parsed.as_array().cloned().unwrap_or_default();
    let checks: Vec<Value> = arr
        .iter()
        .take(PR_CHECKS_MAX)
        .map(|entry| {
            let c = entry.as_object();
            let get = |k: &str| c.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            json!({
                "name": get("name"),
                "bucket": get("bucket"),
                "state": get("state"),
                "workflow": get("workflow"),
                "link": get("link"),
                "description": get("description"),
                "event": get("event"),
                "started_at": get("startedAt"),
                "completed_at": get("completedAt"),
            })
        })
        .collect();
    Ok(json!({
        "number": number,
        "head": refs.head,
        "base": refs.base,
        "checks": checks,
        "checks_truncated": arr.len() > PR_CHECKS_MAX,
    }))
}

pub struct PrLogsRequest {
    pub repo: String,
    pub number: i64,
    pub run_id: Option<String>,
    pub job_id: Option<String>,
    pub lines: Option<i64>,
}

fn map_gh_run(raw: &Value) -> Value {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": get("databaseId"),
        "name": get("name"),
        "workflow": get("workflowName"),
        "status": get("status"),
        "conclusion": get("conclusion"),
        "url": get("url"),
        "head_sha": get("headSha"),
        "created_at": get("createdAt"),
        "event": get("event"),
        "display_title": get("displayTitle"),
    })
}

fn map_gh_job(raw: &Value) -> Value {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": get("databaseId"),
        "name": get("name"),
        "status": get("status"),
        "conclusion": get("conclusion"),
        "url": get("url"),
        "started_at": get("startedAt"),
        "completed_at": get("completedAt"),
    })
}

/// `pr logs`: workflow runs for the PR head, a run's job list, or a
/// job's bounded log tail. All modes echo head/base freshness.
pub fn collect_pr_logs(
    repo_root: &Path,
    request: &PrLogsRequest,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = fetch_pr_refs(repo_root, &request.repo, request.number, opts)?;
    let mut base = json!({
        "number": request.number,
        "head": refs.head,
        "base": refs.base,
    });
    if let Some(obj) = base.as_object_mut() {
        obj.insert("head".into(), refs.head.clone());
        obj.insert("base".into(), refs.base.clone());
    }

    if let Some(job_id) = &request.job_id {
        let head_sha = refs.head.get("sha").and_then(|v| v.as_str());
        let Some(head_sha) = head_sha else {
            return Err(GhError::new(
                GhErrorKind::GhError,
                "pull request head sha unavailable for log lookup",
            ));
        };
        let mut call_opts = opts.clone();
        call_opts.max_buffer = Some(PR_LOG_FETCH_MAX_BUFFER);
        let log = gh_ok(
            repo_root,
            &[
                "run".to_string(),
                "view".to_string(),
                "--job".to_string(),
                job_id.clone(),
                "--repo".to_string(),
                request.repo.clone(),
                "--log".to_string(),
            ],
            &call_opts,
        )?;
        let lines = request
            .lines
            .unwrap_or(PR_LOG_LINES_DEFAULT)
            .clamp(1, PR_LOG_LINES_MAX);
        let normalized = log.replace("\r\n", "\n");
        let all_lines: Vec<&str> = normalized.split('\n').collect();
        let tail_start = all_lines.len().saturating_sub(lines as usize);
        let tail_lines = &all_lines[tail_start..];
        let mut text = tail_lines.join("\n");
        let mut truncated = all_lines.len() > tail_lines.len();
        if text.len() > PR_LOG_BYTES_MAX {
            // Take the last PR_LOG_BYTES_MAX bytes (may split a UTF-8
            // boundary; TS used Buffer.subarray which does the same).
            let start = text.len() - PR_LOG_BYTES_MAX;
            while start > 0 && !text.is_char_boundary(start) {
                // find next boundary
            }
            let mut s = start;
            while s < text.len() && !text.is_char_boundary(s) {
                s += 1;
            }
            text = text[s..].to_string();
            truncated = true;
        }
        let bytes = text.len();
        let lines_returned = tail_lines.len();
        let total_lines = all_lines.len();
        if let Some(obj) = base.as_object_mut() {
            obj.insert("mode".into(), json!("job"));
            obj.insert("job_id".into(), json!(job_id));
            obj.insert("head_sha".into(), json!(head_sha));
            obj.insert("log".into(), json!(text));
            obj.insert("lines_returned".into(), json!(lines_returned));
            obj.insert("total_lines".into(), json!(total_lines));
            obj.insert("bytes".into(), json!(bytes));
            obj.insert("truncated".into(), json!(truncated));
        }
        return Ok(base);
    }

    if let Some(run_id) = &request.run_id {
        let out = gh_ok(
            repo_root,
            &[
                "run".to_string(),
                "view".to_string(),
                run_id.clone(),
                "--repo".to_string(),
                request.repo.clone(),
                "--json".to_string(),
                PR_RUN_VIEW_FIELDS.to_string(),
            ],
            opts,
        )?;
        let parsed: Value = serde_json::from_str(&out).map_err(|e| {
            GhError::new(
                GhErrorKind::GhError,
                format!("gh run view: invalid JSON: {e}"),
            )
        })?;
        let jobs_raw = parsed
            .get("jobs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let jobs_len = jobs_raw.len();
        let bounded: Vec<Value> = jobs_raw
            .iter()
            .take(PR_RUN_JOBS_MAX)
            .map(map_gh_job)
            .collect();
        if let Some(obj) = base.as_object_mut() {
            obj.insert("mode".into(), json!("run"));
            obj.insert("run".into(), map_gh_run(&parsed));
            obj.insert("jobs".into(), json!(bounded));
            obj.insert("jobs_truncated".into(), json!(jobs_len > PR_RUN_JOBS_MAX));
        }
        return Ok(base);
    }

    let head_sha = refs.head.get("sha").and_then(|v| v.as_str());
    let Some(head_sha) = head_sha else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "pull request head sha unavailable for run lookup",
        ));
    };
    let out = gh_ok(
        repo_root,
        &[
            "run".to_string(),
            "list".to_string(),
            "--repo".to_string(),
            request.repo.clone(),
            "--commit".to_string(),
            head_sha.to_string(),
            "--limit".to_string(),
            PR_RUNS_MAX.to_string(),
            "--json".to_string(),
            PR_RUN_LIST_FIELDS.to_string(),
        ],
        opts,
    )?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh run list: invalid JSON: {e}"),
        )
    })?;
    let arr = parsed.as_array().cloned().unwrap_or_default();
    let runs: Vec<Value> = arr.iter().take(PR_RUNS_MAX).map(map_gh_run).collect();
    if let Some(obj) = base.as_object_mut() {
        obj.insert("mode".into(), json!("runs"));
        obj.insert("head_sha".into(), json!(head_sha));
        obj.insert("runs".into(), json!(runs));
        obj.insert("runs_truncated".into(), json!(arr.len() > PR_RUNS_MAX));
    }
    Ok(base)
}

// ---------------------------------------------------------------------------
// PR review/comment mutations (Issue #172, ADR-0008 gh-CLI discipline).
// ---------------------------------------------------------------------------

/// 40-hex (sha1) or 64-hex (sha256) object id; short shas are rejected.
pub fn is_valid_head_sha(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Re-fetch the PR and require `head.sha === expect_head`.
fn assert_head_fresh(
    repo_root: &Path,
    repo: &str,
    number: i64,
    expect_head: &str,
    opts: &GhOptions,
) -> Result<PrRefs, GhError> {
    let refs = fetch_pr_refs(repo_root, repo, number, opts)?;
    let head_sha = refs.head.get("sha").and_then(|v| v.as_str());
    let Some(head_sha) = head_sha else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "pull request head sha unavailable for mutation",
        ));
    };
    if head_sha != expect_head {
        return Err(GhError::new(
            GhErrorKind::StaleHead,
            format!(
                "pull request #{number} head moved: expected {expect_head}, current {head_sha}"
            ),
        ));
    }
    Ok(refs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl PrReviewEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrReviewEvent::Approve => "APPROVE",
            PrReviewEvent::RequestChanges => "REQUEST_CHANGES",
            PrReviewEvent::Comment => "COMMENT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PrLineComment {
    pub path: String,
    pub line: i64,
    pub side: Option<String>,
    pub body: String,
}

fn validate_line_comments(comments: &[PrLineComment]) -> Result<(), GhError> {
    for (i, c) in comments.iter().enumerate() {
        if c.path.is_empty() || c.path.chars().count() > 4096 {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("line comment {i}: invalid path"),
            ));
        }
        if c.line < 1 {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("line comment {i}: line must be a positive integer"),
            ));
        }
        if let Some(side) = &c.side {
            if side != "RIGHT" && side != "LEFT" {
                return Err(GhError::new(
                    GhErrorKind::GhError,
                    format!("line comment {i}: side must be RIGHT or LEFT"),
                ));
            }
        }
        if c.body.trim().is_empty() {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("line comment {i}: body must be non-empty"),
            ));
        }
    }
    Ok(())
}

fn map_gh_review(raw: &Value) -> Value {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    let (body, truncated) = bound_body(raw.get("body"), PR_BODY_MAX_CHARS);
    let mut item = json!({
        "id": get("id"),
        "author": get("user").get("login").cloned().unwrap_or(Value::Null),
        "state": get("state"),
        "commit_id": get("commit_id"),
        "submitted_at": get("submitted_at"),
        "url": get("html_url"),
        "body": body,
    });
    if truncated {
        item["body_truncated"] = json!(true);
    }
    item
}

fn map_gh_comment(raw: &Value) -> Value {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    let (body, truncated) = bound_body(raw.get("body"), PR_BODY_MAX_CHARS);
    let mut item = json!({
        "id": get("id"),
        "author": get("user").get("login").cloned().unwrap_or(Value::Null),
        "body": body,
        "created_at": get("created_at"),
        "updated_at": get("updated_at"),
        "url": get("html_url"),
    });
    if truncated {
        item["body_truncated"] = json!(true);
    }
    item
}

fn map_review_requests(refs: &PrRefs) -> Value {
    json!({
        "users": refs.requested_reviewers.iter().map(|u| u.get("login").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "teams": refs.requested_teams.iter().map(|t| {
            let name = t.get("name").and_then(|v| v.as_str());
            let slug = t.get("slug").and_then(|v| v.as_str());
            json!(name.or(slug))
        }).collect::<Vec<_>>(),
    })
}

/// `pr review`: submit APPROVE / REQUEST_CHANGES / COMMENT.
pub fn submit_pr_review(
    repo_root: &Path,
    repo: &str,
    number: i64,
    expect_head: &str,
    event: PrReviewEvent,
    body: Option<&str>,
    comments: &[PrLineComment],
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = assert_head_fresh(repo_root, repo, number, expect_head, opts)?;
    validate_line_comments(comments)?;
    let mut payload = Map::new();
    payload.insert(
        "commit_id".into(),
        refs.head.get("sha").cloned().unwrap_or(Value::Null),
    );
    payload.insert("event".into(), json!(event.as_str()));
    if let Some(b) = body {
        payload.insert("body".into(), json!(b));
    }
    if !comments.is_empty() {
        let arr: Vec<Value> = comments
            .iter()
            .map(|c| {
                let mut entry = Map::new();
                entry.insert("path".into(), json!(c.path));
                entry.insert("line".into(), json!(c.line));
                entry.insert("body".into(), json!(c.body));
                if let Some(side) = &c.side {
                    entry.insert("side".into(), json!(side));
                }
                Value::Object(entry)
            })
            .collect();
        payload.insert("comments".into(), json!(arr));
    }
    let review = gh_api_call(
        repo_root,
        repo,
        &format!("pulls/{number}/reviews"),
        "POST",
        Some(&Value::Object(payload)),
        opts,
    )?;
    Ok(json!({
        "number": number,
        "head": refs.head,
        "base": refs.base,
        "review": map_gh_review(&review),
    }))
}

/// `pr comment`: post a PR-conversation comment.
pub fn submit_pr_comment(
    repo_root: &Path,
    repo: &str,
    number: i64,
    expect_head: &str,
    body: &str,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = assert_head_fresh(repo_root, repo, number, expect_head, opts)?;
    let comment = gh_api_call(
        repo_root,
        repo,
        &format!("issues/{number}/comments"),
        "POST",
        Some(&json!({ "body": body })),
        opts,
    )?;
    Ok(json!({
        "number": number,
        "head": refs.head,
        "base": refs.base,
        "comment": map_gh_comment(&comment),
    }))
}

/// `pr reply`: reply inside a review thread.
pub fn submit_pr_reply(
    repo_root: &Path,
    repo: &str,
    number: i64,
    expect_head: &str,
    comment_id: i64,
    body: &str,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let refs = assert_head_fresh(repo_root, repo, number, expect_head, opts)?;
    let comment = gh_api_call(
        repo_root,
        repo,
        &format!("pulls/{number}/comments/{comment_id}/replies"),
        "POST",
        Some(&json!({ "body": body })),
        opts,
    )?;
    Ok(json!({
        "number": number,
        "comment_id": comment_id,
        "head": refs.head,
        "base": refs.base,
        "comment": map_gh_comment(&comment),
    }))
}

const REVIEWER_LOGIN_PATTERN: &str = r"^[A-Za-z0-9](?:[A-Za-z0-9_.-]*[A-Za-z0-9_])?$";

/// `pr reviewers`: request/dismiss reviewers via `gh pr edit`.
pub fn update_pr_reviewers(
    repo_root: &Path,
    repo: &str,
    number: i64,
    expect_head: &str,
    add: &[String],
    remove: &[String],
    opts: &GhOptions,
) -> Result<Value, GhError> {
    assert_head_fresh(repo_root, repo, number, expect_head, opts)?;
    let re = regex::Regex::new(REVIEWER_LOGIN_PATTERN).unwrap();
    for (list, what) in [(add, "--add-reviewer"), (remove, "--remove-reviewer")] {
        for login in list {
            if !re.is_match(login) {
                return Err(GhError::new(
                    GhErrorKind::GhError,
                    format!("invalid reviewer login for {what}: {}", json_string(login)),
                ));
            }
        }
    }
    if add.is_empty() && remove.is_empty() {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "at least one of --add-reviewer/--remove-reviewer is required",
        ));
    }
    let mut args = vec![
        "pr".to_string(),
        "edit".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
    ];
    if !add.is_empty() {
        args.push("--add-reviewer".to_string());
        args.push(add.join(","));
    }
    if !remove.is_empty() {
        args.push("--remove-reviewer".to_string());
        args.push(remove.join(","));
    }
    gh_ok(repo_root, &args, opts)?;
    let after = fetch_pr_refs(repo_root, repo, number, opts)?;
    Ok(json!({
        "number": number,
        "head": after.head,
        "base": after.base,
        "review_requests": map_review_requests(&after),
    }))
}

// ---------------------------------------------------------------------------
// GitHub issue surface (Issue #174, ADR-0008 gh-CLI discipline).
// ---------------------------------------------------------------------------

const ISSUE_LIST_FIELDS: &str =
    "number,title,state,labels,assignees,author,createdAt,updatedAt,url";
const ISSUE_VIEW_FIELDS: &str =
    "number,title,state,stateReason,closed,author,labels,assignees,milestone,body,comments,reactionGroups,createdAt,updatedAt,url";
const ISSUE_COMMENTS_MAX: usize = 50;
const ISSUE_COMMENT_BODY_MAX_CHARS: usize = 4000;
const ISSUE_REACTIONS_MAX: usize = 20;

fn issue_login_list(value: Option<&Value>) -> Vec<String> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            entry
                .get("login")
                .and_then(|v| v.as_str())
                .or_else(|| entry.get("name").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
        })
        .collect()
}

/// Bounded projection of `gh issue view --json`.
pub fn bound_issue_view(raw: &Value) -> Value {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
    let author = match raw.get("author") {
        Some(Value::String(s)) => json!(s),
        Some(v) => v.get("login").cloned().unwrap_or(Value::Null),
        None => Value::Null,
    };
    let labels: Vec<String> = raw
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.as_str().map(|s| s.to_string()).or_else(|| {
                        l.get("name")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (body, body_trunc) = bound_body(raw.get("body"), BODY_MAX_CHARS);
    let mut out = json!({
        "number": get("number"),
        "title": get("title"),
        "state": get("state"),
        "state_reason": get("stateReason"),
        "closed": get("closed"),
        "author": author,
        "labels": labels,
        "assignees": issue_login_list(raw.get("assignees")),
        "milestone": raw.get("milestone").and_then(|m| m.get("title")).cloned().unwrap_or(Value::Null),
        "created_at": get("createdAt"),
        "updated_at": get("updatedAt"),
        "url": get("url"),
        "body": body,
    });
    if body_trunc {
        out["body_truncated"] = json!(true);
    }
    let comments_raw = raw
        .get("comments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let comments: Vec<Value> = comments_raw
        .iter()
        .map(|entry| {
            let c = entry.as_object();
            let getc = |k: &str| c.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            let who = c.and_then(|o| o.get("author")).or_else(|| c.and_then(|o| o.get("user")));
            let comment_author = match who {
                Some(Value::String(s)) => json!(s),
                Some(v) => v.get("login").cloned().unwrap_or(Value::Null),
                None => Value::Null,
            };
            let (text, trunc) = bound_body(c.and_then(|o| o.get("body")), ISSUE_COMMENT_BODY_MAX_CHARS);
            let mut item = json!({
                "id": getc("id"),
                "author": comment_author,
                "body": text,
                "created_at": if getc("createdAt").is_null() { getc("created_at") } else { getc("createdAt") },
                "updated_at": if getc("updatedAt").is_null() { getc("updated_at") } else { getc("updatedAt") },
                "url": getc("url"),
            });
            if trunc {
                item["body_truncated"] = json!(true);
            }
            item
        })
        .collect();
    let comments_len = comments.len();
    if let Some(obj) = out.as_object_mut() {
        let start = comments_len.saturating_sub(ISSUE_COMMENTS_MAX);
        obj.insert("comments".into(), json!(comments[start..].to_vec()));
        obj.insert(
            "comments_truncated".into(),
            json!(comments_len > ISSUE_COMMENTS_MAX),
        );
    }
    let reactions_raw = raw
        .get("reactionGroups")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let reactions: Vec<Value> = reactions_raw
        .iter()
        .take(ISSUE_REACTIONS_MAX)
        .map(|entry| {
            let r = entry.as_object();
            let getr = |k: &str| r.and_then(|o| o.get(k)).cloned().unwrap_or(Value::Null);
            let count = r
                .and_then(|o| o.get("users"))
                .and_then(|u| u.get("totalCount"))
                .cloned()
                .unwrap_or_else(|| getr("count"));
            json!({
                "content": getr("content"),
                "count": count,
            })
        })
        .collect();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("reactions".into(), json!(reactions));
        if reactions_raw.len() > ISSUE_REACTIONS_MAX {
            obj.insert("reactions_truncated".into(), json!(true));
        }
    }
    out
}

pub struct IssueListRequest<'a> {
    pub repo: &'a str,
    pub limit: Option<i64>,
    pub state: Option<&'a str>,
    pub label: &'a [String],
    pub assignee: &'a [String],
    pub author: Option<&'a str>,
}

/// `issue list`: fixed `--json` field set; gh's own filtering applies.
pub fn collect_issue_list(
    repo_root: &Path,
    request: &IssueListRequest<'_>,
    opts: &GhOptions,
) -> Result<Vec<Value>, GhError> {
    let mut args = vec![
        "issue".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        request.repo.to_string(),
        "--json".to_string(),
        ISSUE_LIST_FIELDS.to_string(),
    ];
    if let Some(limit) = request.limit {
        args.push("--limit".to_string());
        args.push(limit.to_string());
    }
    if let Some(state) = request.state {
        args.push("--state".to_string());
        args.push(state.to_string());
    }
    for label in request.label {
        args.push("--label".to_string());
        args.push(label.clone());
    }
    for assignee in request.assignee {
        args.push("--assignee".to_string());
        args.push(assignee.clone());
    }
    if let Some(author) = request.author {
        args.push("--author".to_string());
        args.push(author.to_string());
    }
    let out = gh_ok(repo_root, &args, opts)?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh issue list: invalid JSON: {e}"),
        )
    })?;
    Ok(parsed.as_array().cloned().unwrap_or_default())
}

pub fn collect_issue_view(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let out = gh_ok(
        repo_root,
        &[
            "issue".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--json".to_string(),
            ISSUE_VIEW_FIELDS.to_string(),
        ],
        opts,
    )?;
    let parsed: Value = serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh issue view: invalid JSON: {e}"),
        )
    })?;
    Ok(bound_issue_view(&parsed))
}

pub struct IssueCreateRequest<'a> {
    pub repo: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub body_file: Option<&'a str>,
    pub labels: &'a [String],
    pub assignees: &'a [String],
}

/// `gh issue create` prints the new issue URL on stdout.
pub fn collect_issue_create(
    repo_root: &Path,
    request: &IssueCreateRequest<'_>,
    opts: &GhOptions,
) -> Result<(String, Option<i64>), GhError> {
    let mut args = vec![
        "issue".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        request.repo.to_string(),
        "--title".to_string(),
        request.title.to_string(),
    ];
    if request.body_file == Some("-") {
        args.push("--body-file".to_string());
        args.push("-".to_string());
    } else if let Some(bf) = request.body_file {
        args.push("--body-file".to_string());
        args.push(bf.to_string());
    } else {
        args.push("--body".to_string());
        args.push(request.body.unwrap_or("").to_string());
    }
    for label in request.labels {
        args.push("--label".to_string());
        args.push(label.clone());
    }
    for assignee in request.assignees {
        args.push("--assignee".to_string());
        args.push(assignee.clone());
    }
    let mut call_opts = opts.clone();
    if request.body_file == Some("-") {
        call_opts.input = opts.body_text.clone();
    }
    let out = gh_ok(repo_root, &args, &call_opts)?;
    let url = out.trim().lines().last().unwrap_or("").to_string();
    let number = extract_trailing_number(&url, "/issues/");
    Ok((url, number))
}

/// `issue comment`: post an issue comment via `gh api --input -`.
pub fn submit_issue_comment(
    repo_root: &Path,
    repo: &str,
    number: i64,
    body: &str,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let comment = gh_api_call(
        repo_root,
        repo,
        &format!("issues/{number}/comments"),
        "POST",
        Some(&json!({ "body": body })),
        opts,
    )?;
    Ok(json!({
        "number": number,
        "comment": map_gh_comment(&comment),
    }))
}

pub struct IssueEditRequest<'a> {
    pub repo: &'a str,
    pub number: i64,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub body_file: Option<&'a str>,
    pub add_labels: &'a [String],
    pub remove_labels: &'a [String],
    pub add_assignees: &'a [String],
    pub remove_assignees: &'a [String],
    /// `"closed"`/`"open"` map to `gh issue close`/`reopen`.
    pub state: Option<&'a str>,
    /// `gh issue close --reason`; only meaningful with state "closed".
    pub reason: Option<&'a str>,
}

const ISSUE_CLOSE_REASONS: &[&str] = &["completed", "not planned"];

/// `issue edit` / `issue close`: `gh issue edit` for fields,
/// `gh issue close|reopen` for state, then a bounded `issue view`
/// re-fetch so callers get the post-mutation state.
pub fn update_issue(
    repo_root: &Path,
    request: &IssueEditRequest<'_>,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let mut edit_args = vec![
        "issue".to_string(),
        "edit".to_string(),
        request.number.to_string(),
        "--repo".to_string(),
        request.repo.to_string(),
    ];
    let mut has_edit = false;
    if let Some(title) = request.title {
        edit_args.push("--title".to_string());
        edit_args.push(title.to_string());
        has_edit = true;
    }
    if request.body_file == Some("-") {
        edit_args.push("--body-file".to_string());
        edit_args.push("-".to_string());
        has_edit = true;
    } else if let Some(bf) = request.body_file {
        edit_args.push("--body-file".to_string());
        edit_args.push(bf.to_string());
        has_edit = true;
    } else if let Some(body) = request.body {
        edit_args.push("--body".to_string());
        edit_args.push(body.to_string());
        has_edit = true;
    }
    for label in request.add_labels {
        edit_args.push("--add-label".to_string());
        edit_args.push(label.clone());
        has_edit = true;
    }
    for label in request.remove_labels {
        edit_args.push("--remove-label".to_string());
        edit_args.push(label.clone());
        has_edit = true;
    }
    for assignee in request.add_assignees {
        edit_args.push("--add-assignee".to_string());
        edit_args.push(assignee.clone());
        has_edit = true;
    }
    for assignee in request.remove_assignees {
        edit_args.push("--remove-assignee".to_string());
        edit_args.push(assignee.clone());
        has_edit = true;
    }
    if let Some(state) = request.state {
        if state != "open" && state != "closed" {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("invalid issue state: {}", json_string(state)),
            ));
        }
    }
    if let Some(reason) = request.reason {
        if !ISSUE_CLOSE_REASONS.contains(&reason) {
            return Err(GhError::new(
                GhErrorKind::GhError,
                format!("invalid issue close reason: {}", json_string(reason)),
            ));
        }
        if request.state != Some("closed") {
            return Err(GhError::new(
                GhErrorKind::GhError,
                "--reason is only valid when closing an issue",
            ));
        }
    }
    if has_edit {
        let mut call_opts = opts.clone();
        if request.body_file == Some("-") {
            call_opts.input = opts.body_text.clone();
        }
        gh_ok(repo_root, &edit_args, &call_opts)?;
    }
    match request.state {
        Some("closed") => {
            let mut close_args = vec![
                "issue".to_string(),
                "close".to_string(),
                request.number.to_string(),
                "--repo".to_string(),
                request.repo.to_string(),
            ];
            if let Some(reason) = request.reason {
                close_args.push("--reason".to_string());
                close_args.push(reason.to_string());
            }
            gh_ok(repo_root, &close_args, opts)?;
        }
        Some("open") => {
            gh_ok(
                repo_root,
                &[
                    "issue".to_string(),
                    "reopen".to_string(),
                    request.number.to_string(),
                    "--repo".to_string(),
                    request.repo.to_string(),
                ],
                opts,
            )?;
        }
        _ => {}
    }
    let issue = collect_issue_view(repo_root, request.repo, request.number, opts)?;
    Ok(json!({
        "number": request.number,
        "issue": issue,
    }))
}

// ---------------------------------------------------------------------------
// PR merge / auto-merge (Issue #173, ADR-0030).
// ---------------------------------------------------------------------------

const PR_MERGE_VIEW_FIELDS: &str =
    "number,state,isDraft,mergeable,mergeStateStatus,reviewDecision,headRefOid,headRefName,baseRefName,url,autoMergeRequest";

/// Release-train head branches (`release-0-9-0`, `release-1`, …).
fn is_release_head(head_ref: &str) -> bool {
    let re = regex::Regex::new(r"(?i)^release-\d+(-\d+)*$").unwrap();
    re.is_match(head_ref)
}

/// `gh pr view --json` merge-state projection used by the merge guard.
fn fetch_pr_merge_state(
    repo_root: &Path,
    repo: &str,
    number: i64,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    let out = gh_ok(
        repo_root,
        &[
            "pr".to_string(),
            "view".to_string(),
            number.to_string(),
            "--repo".to_string(),
            repo.to_string(),
            "--json".to_string(),
            PR_MERGE_VIEW_FIELDS.to_string(),
        ],
        opts,
    )?;
    serde_json::from_str(&out).map_err(|e| {
        GhError::new(
            GhErrorKind::GhError,
            format!("gh pr view: invalid JSON: {e}"),
        )
    })
}

fn upper_or_null(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
}

fn map_auto_merge_request(raw: Option<&Value>) -> Value {
    match raw {
        Some(v) if v.is_object() => json!({
            "merge_method": v.get("mergeMethod").cloned().unwrap_or(Value::Null),
            "enabled_at": v.get("enabledAt").cloned().unwrap_or(Value::Null),
        }),
        _ => Value::Null,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl PrMergeMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrMergeMethod::Merge => "merge",
            PrMergeMethod::Squash => "squash",
            PrMergeMethod::Rebase => "rebase",
        }
    }
}

pub struct PrMergeRequest<'a> {
    pub repo: &'a str,
    pub number: i64,
    pub expect_head: &'a str,
    pub method: Option<PrMergeMethod>,
    pub auto: bool,
    pub disable_auto: bool,
}

const MERGE_STATE_REFUSED: &[&str] = &[
    "BEHIND",
    "BLOCKED",
    "DIRTY",
    "DRAFT",
    "HAS_HOOKS",
    "UNKNOWN",
];
const AUTO_MERGE_STATE_REFUSED: &[&str] = &["DIRTY", "DRAFT", "UNKNOWN"];

fn merge_blocked_message(number: i64, view: &Value, reason: &str) -> String {
    let get = |k: &str| view.get(k);
    let mut parts = vec![
        format!("pull request #{number} {reason}"),
        format!(
            "mergeStateStatus={}",
            upper_or_null(get("mergeStateStatus")).unwrap_or_else(|| "null".into())
        ),
        format!(
            "reviewDecision={}",
            upper_or_null(get("reviewDecision")).unwrap_or_else(|| "null".into())
        ),
        format!(
            "mergeable={}",
            upper_or_null(get("mergeable")).unwrap_or_else(|| "null".into())
        ),
        format!(
            "isDraft={}",
            get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false)
        ),
    ];
    if let Some(name) = get("headRefName").and_then(|v| v.as_str()) {
        parts.push(format!("head={name}"));
    }
    if let Some(name) = get("baseRefName").and_then(|v| v.as_str()) {
        parts.push(format!("base={name}"));
    }
    parts.join(" ")
}

/// Map gh's HTTP failure for a merge write to the honest taxonomy:
/// 405/409/422 are GitHub refusing the merge itself → `merge_blocked`.
fn merge_write<T, F>(f: F) -> Result<T, GhError>
where
    F: FnOnce() -> Result<T, GhError>,
{
    match f() {
        Ok(v) => Ok(v),
        Err(e) => {
            if e.kind == GhErrorKind::GhError {
                let re = regex::Regex::new(r"HTTP (405|409|422)\b").unwrap();
                if re.is_match(&e.message) {
                    return Err(GhError::new(GhErrorKind::MergeBlocked, e.message));
                }
            }
            Err(e)
        }
    }
}

/// `pr merge`: immediate merge, auto-merge enable, or auto-merge disable.
pub fn merge_pr(
    repo_root: &Path,
    request: &PrMergeRequest<'_>,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    if request.auto && request.disable_auto {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "--auto and --disable-auto are mutually exclusive",
        ));
    }
    if request.auto && request.method.is_none() {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "auto-merge requires an explicit merge method",
        ));
    }
    if request.disable_auto && request.method.is_some() {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "merge method is meaningless with --disable-auto",
        ));
    }

    let view = fetch_pr_merge_state(repo_root, request.repo, request.number, opts)?;
    let head_sha = view
        .get("headRefOid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(head_sha) = head_sha else {
        return Err(GhError::new(
            GhErrorKind::GhError,
            "pull request head sha unavailable for merge",
        ));
    };
    if head_sha != request.expect_head {
        return Err(GhError::new(
            GhErrorKind::StaleHead,
            format!(
                "pull request #{} head moved: expected {}, current {}",
                request.number, request.expect_head, head_sha
            ),
        ));
    }
    let head_ref = view
        .get("headRefName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pr_state = upper_or_null(view.get("state"));
    let merge_state = upper_or_null(view.get("mergeStateStatus"));
    let is_draft = view
        .get("isDraft")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // github-delivery boundary: never merge or arm auto-merge on a
    // release-train head branch.
    if is_release_head(&head_ref) {
        return Err(GhError::new(
            GhErrorKind::ReleaseBoundary,
            format!(
                "refusing to merge release-train head '{head_ref}': release approval is a maintainer decision (github-delivery)"
            ),
        ));
    }
    if pr_state.as_deref() != Some("OPEN") {
        let reason = match &pr_state {
            Some(s) => format!("is {}, not open", s.to_lowercase()),
            None => "is unknown, not open".to_string(),
        };
        return Err(GhError::new(
            GhErrorKind::MergeBlocked,
            merge_blocked_message(request.number, &view, &reason),
        ));
    }

    let endpoint = format!("pulls/{}", request.number);
    let result: String;
    let mut merge_echo: Option<Value> = None;

    if request.disable_auto {
        merge_write(|| {
            gh_api_delete(
                repo_root,
                request.repo,
                &format!("{endpoint}/auto-merge"),
                opts,
            )
        })?;
        result = "auto_merge_disabled".to_string();
    } else if request.auto {
        if is_draft {
            return Err(GhError::new(
                GhErrorKind::MergeBlocked,
                merge_blocked_message(request.number, &view, "is a draft"),
            ));
        }
        if let Some(ms) = &merge_state {
            if AUTO_MERGE_STATE_REFUSED.contains(&ms.as_str()) {
                return Err(GhError::new(
                    GhErrorKind::MergeBlocked,
                    merge_blocked_message(request.number, &view, "cannot be queued for auto-merge"),
                ));
            }
        }
        let payload = json!({ "merge_method": request.method.map(|m| m.as_str()) });
        merge_echo = Some(merge_write(|| {
            gh_api_call(
                repo_root,
                request.repo,
                &format!("{endpoint}/auto-merge"),
                "POST",
                Some(&payload),
                opts,
            )
        })?);
        result = "auto_merge_enabled".to_string();
    } else {
        if is_draft {
            return Err(GhError::new(
                GhErrorKind::MergeBlocked,
                merge_blocked_message(request.number, &view, "is a draft"),
            ));
        }
        match &merge_state {
            None => {
                return Err(GhError::new(
                    GhErrorKind::MergeBlocked,
                    merge_blocked_message(request.number, &view, "is not mergeable"),
                ));
            }
            Some(ms) if MERGE_STATE_REFUSED.contains(&ms.as_str()) => {
                return Err(GhError::new(
                    GhErrorKind::MergeBlocked,
                    merge_blocked_message(request.number, &view, "is not mergeable"),
                ));
            }
            _ => {}
        }
        let mut payload = Map::new();
        payload.insert("sha".into(), json!(head_sha));
        if let Some(m) = request.method {
            payload.insert("merge_method".into(), json!(m.as_str()));
        }
        let echo = merge_write(|| {
            gh_api_call(
                repo_root,
                request.repo,
                &format!("{endpoint}/merge"),
                "PUT",
                Some(&Value::Object(payload.clone())),
                opts,
            )
        })?;
        result = if echo.get("merged").and_then(|v| v.as_bool()) == Some(true) {
            "merged".to_string()
        } else {
            "pending".to_string()
        };
        merge_echo = Some(echo);
    }

    // Post-write truth: re-fetch state + refs.
    let after = fetch_pr_merge_state(repo_root, request.repo, request.number, opts)?;
    let refs = fetch_pr_refs(repo_root, request.repo, request.number, opts)?;
    let action = if request.disable_auto {
        "auto_merge_disable"
    } else if request.auto {
        "auto_merge_enable"
    } else {
        "merge"
    };
    Ok(json!({
        "number": request.number,
        "head": refs.head,
        "base": refs.base,
        "action": action,
        "method": request.method.map(|m| m.as_str()),
        "result": result,
        "pre_merge_state": {
            "merge_state_status": merge_state,
            "review_decision": upper_or_null(view.get("reviewDecision")),
            "mergeable": upper_or_null(view.get("mergeable")),
        },
        "state": upper_or_null(after.get("state")),
        "is_draft": after.get("isDraft").and_then(|v| v.as_bool()).unwrap_or(false),
        "auto_merge_request": map_auto_merge_request(after.get("autoMergeRequest")),
        "merge": merge_echo,
    }))
}

fn gh_api_delete(
    repo_root: &Path,
    repo: &str,
    endpoint: &str,
    opts: &GhOptions,
) -> Result<Value, GhError> {
    gh_api_call(repo_root, repo, endpoint, "DELETE", None, opts)
}
