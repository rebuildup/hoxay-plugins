//! Data-source resolution + export for rebuildup.usage (ADR-0013).
//!
//! Rust port of `plugins/usage/src/source.ts` (retired; git history
//! preserves it). The plugin consumes `rebuildup.conversations`
//! exclusively through its public read-only CLI contract (`capability`,
//! `usage` paging) — never its state files. The conversations plugin
//! root is resolved from:
//!   1. explicit override (`--conversations-root` / env / config),
//!   2. the sibling `../conversations` directory (repo layout and
//!      managed installs both keep `plugins/<name>` side by side),
//!   3. `herdr plugin list --json` discovery.
//!
//! The child process gets `HERDR_PLUGIN_STATE_DIR`/`_CONFIG_DIR`
//! pointed at the *conversations* plugin dirs — the ambient values
//! belong to rebuildup.usage and must never leak into the child.
//!
//! Delegation stays spawn-based (not an embedded crate): the installed
//! conversations binary is the version-matched reader of its own index
//! contract — independent install/version is preserved (Issue #294).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hoxay_herdr_client::dirs::{herdr_config_dir, herdr_state_dir, plugin_path_component};
use serde_json::Value;

use crate::aggregate::UsageEventRecord;
use crate::model::{CONVERSATIONS_PLUGIN_ID, SourceOptions};

/// Resolved data source for the conversations index.
#[derive(Debug, Clone)]
pub struct SourceResolution {
    pub plugin_root: PathBuf,
    pub cli_path: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    /// How the root was found: explicit override, sibling, or herdr discovery.
    pub resolved_via: &'static str,
}

#[derive(Debug)]
pub enum ResolveResult {
    Ok(SourceResolution),
    Err { error: String, kind: String },
}

impl ResolveResult {
    pub fn ok(&self) -> bool {
        matches!(self, ResolveResult::Ok(_))
    }
    pub fn source(&self) -> Option<&SourceResolution> {
        match self {
            ResolveResult::Ok(s) => Some(s),
            _ => None,
        }
    }
    pub fn error(&self) -> Option<&str> {
        match self {
            ResolveResult::Err { error, .. } => Some(error),
            _ => None,
        }
    }
    pub fn kind(&self) -> Option<&str> {
        match self {
            ResolveResult::Err { kind, .. } => Some(kind),
            _ => None,
        }
    }
}

/// This plugin's own root (plugins/usage): `HERDR_PLUGIN_ROOT` wins;
/// otherwise derived from the executable location. Installed layout is
/// `<root>/bin/<name>`; dev builds live at `plugins/usage/target/*/`
/// — walking ancestors for `herdr-plugin.toml` covers both.
pub fn plugin_root() -> PathBuf {
    if let Some(env_root) = std::env::var("HERDR_PLUGIN_ROOT")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(env_root);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            if ancestor.is_dir() && ancestor.join("herdr-plugin.toml").is_file() {
                return ancestor.to_path_buf();
            }
        }
        if let Some(dir) = exe.parent() {
            // <root>/bin/<name> → plugin root is the bin dir's parent.
            if dir.file_name().map(|n| n == "bin").unwrap_or(false) {
                if let Some(root) = dir.parent() {
                    return root.to_path_buf();
                }
            }
            return dir.to_path_buf();
        }
    }
    PathBuf::from(".")
}

fn default_state_dir() -> PathBuf {
    herdr_state_dir()
        .join("plugins")
        .join(plugin_path_component(CONVERSATIONS_PLUGIN_ID))
}

fn default_config_dir() -> PathBuf {
    herdr_config_dir()
        .join("plugins")
        .join("config")
        .join(plugin_path_component(CONVERSATIONS_PLUGIN_ID))
}

fn is_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Mirrors TS `isConversationsRoot`: a runnable entrypoint
/// (`bin/conversations` or legacy `src/cli.ts`) plus, when
/// `herdr-plugin.toml` exists, the matching plugin id.
fn is_conversations_root(dir: &Path) -> bool {
    let bin = dir.join("bin").join("conversations");
    let cli = if is_file(&bin) {
        bin
    } else {
        dir.join("src").join("cli.ts")
    };
    if !is_file(&cli) {
        return false;
    }
    let toml_path = dir.join("herdr-plugin.toml");
    if !is_file(&toml_path) {
        return true; // cli entrypoint is enough signal
    }
    match std::fs::read_to_string(&toml_path) {
        Ok(text) => text.contains(&format!("id = \"{}\"", CONVERSATIONS_PLUGIN_ID)),
        Err(_) => false,
    }
}

/// Prefer the native `bin/conversations`; fall back to `src/cli.ts`.
fn conversations_cli_path(root: &Path) -> PathBuf {
    let bin = root.join("bin").join("conversations");
    if is_file(&bin) {
        bin
    } else {
        root.join("src").join("cli.ts")
    }
}

/// `herdr plugin list --json` → plugin_root for rebuildup.conversations.
fn discover_via_herdr() -> Option<PathBuf> {
    let herdr_bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let mut list_cmd = Command::new(&herdr_bin);
    list_cmd
        .args(["plugin", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = run_with_timeout(list_cmd, Duration::from_secs(15))?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).ok()?;
    let list = parsed
        .as_array()
        .cloned()
        .or_else(|| parsed.get("plugins").and_then(|v| v.as_array()).cloned())?;
    for entry in list {
        let id = entry
            .get("plugin_id")
            .or_else(|| entry.get("id"))
            .and_then(|v| v.as_str());
        let root = entry
            .get("plugin_root")
            .or_else(|| entry.get("root"))
            .and_then(|v| v.as_str());
        if id == Some(CONVERSATIONS_PLUGIN_ID) {
            if let Some(r) = root {
                if !r.is_empty() {
                    return Some(PathBuf::from(r));
                }
            }
        }
    }
    None
}

/// Run a command to completion with a wall-clock timeout; kills and
/// returns None on timeout or spawn failure. stdout/stderr are drained
/// on reader threads while the child runs — `wait_with_output` alone
/// only reads after exit, so a child that fills the pipe buffer would
/// deadlock against the timeout (observed: `conversations usage` pages
/// exceed 64 KiB).
pub(crate) fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        })
    }
    let mut child = cmd.spawn().ok()?;
    let out_handle = drain(child.stdout.take());
    let err_handle = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = out_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = err_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    Some(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Resolve the conversations plugin root + its state/config dirs.
/// Never panics; failures are structured for the CLI error envelope.
pub fn resolve_conversations_source(options: &SourceOptions) -> ResolveResult {
    let explicit_root = options
        .conversations_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut root: Option<PathBuf> = None;
    let mut resolved_via: &'static str = "override";

    if let Some(explicit) = explicit_root {
        let candidate = PathBuf::from(explicit);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir()
                .map(|c| c.join(&candidate))
                .unwrap_or(candidate)
        };
        if is_conversations_root(&candidate) {
            root = Some(candidate);
        } else {
            return ResolveResult::Err {
                error: format!(
                    "--conversations-root {} is not a rebuildup.conversations plugin root",
                    candidate.display()
                ),
                kind: "conversations_unavailable".to_string(),
            };
        }
    } else {
        let sibling = plugin_root()
            .parent()
            .map(|p| p.join("conversations"))
            .unwrap_or_else(|| PathBuf::from("../conversations"));
        if is_conversations_root(&sibling) {
            root = Some(sibling);
            resolved_via = "sibling";
        } else {
            if let Some(discovered) = discover_via_herdr() {
                if is_conversations_root(&discovered) {
                    root = Some(discovered);
                    resolved_via = "herdr-plugin-list";
                }
            }
        }
    }

    let Some(root) = root else {
        return ResolveResult::Err {
            error: "rebuildup.conversations plugin not found; install it or pass --conversations-root / set conversationsRoot in config.json".to_string(),
            kind: "conversations_unavailable".to_string(),
        };
    };

    let state_dir = options
        .conversations_state_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .map(|c| c.join(&p))
                    .unwrap_or(p)
            }
        })
        .unwrap_or_else(default_state_dir);
    let config_dir = options
        .conversations_config_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .map(|c| c.join(&p))
                    .unwrap_or(p)
            }
        })
        .unwrap_or_else(default_config_dir);

    ResolveResult::Ok(SourceResolution {
        cli_path: conversations_cli_path(&root),
        plugin_root: root,
        state_dir,
        config_dir,
        resolved_via,
    })
}

// --- Export paging -----------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct CliCallOptions {
    pub session: Option<String>,
    pub socket_path: Option<String>,
    pub timeout_ms: Option<u64>,
    pub offline: bool,
}

#[derive(Debug, Default, Clone)]
pub struct UsageSelectors {
    pub pane: Option<String>,
    pub session_id: Option<String>,
    pub provider: Option<String>,
}

pub struct CliRunResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub json: Option<Value>,
}

/// Forwarded connection/selector flags shared by `capability` and `usage`.
pub fn conversations_args(call: &CliCallOptions, selectors: Option<&UsageSelectors>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if call.offline {
        args.push("--offline".to_string());
    }
    if let Some(s) = &call.session {
        args.push("--session".to_string());
        args.push(s.clone());
    }
    if let Some(s) = &call.socket_path {
        args.push("--socket".to_string());
        args.push(s.clone());
    }
    if let Some(t) = call.timeout_ms {
        args.push("--timeout".to_string());
        args.push(t.to_string());
    }
    if let Some(sel) = selectors {
        if let Some(p) = &sel.pane {
            args.push("--pane".to_string());
            args.push(p.clone());
        }
        if let Some(s) = &sel.session_id {
            args.push("--session-id".to_string());
            args.push(s.clone());
        }
        if let Some(p) = &sel.provider {
            args.push("--provider".to_string());
            args.push(p.clone());
        }
    }
    args
}

/// Spawn the conversations CLI with the plugin dirs pinned to the
/// *conversations* plugin (ambient HERDR_PLUGIN_* belong to usage).
/// `.ts` entrypoints run under `bun`; a native `bin/` path spawns
/// directly. Mirrors TS `runConversationsCli`.
pub fn run_conversations_cli(
    source: &SourceResolution,
    args: &[String],
    call: &CliCallOptions,
) -> Result<CliRunResult, String> {
    // Child env: process env minus the two plugin-dir overrides, then
    // pinned to the conversations dirs + plugin id.
    let mut env: HashMap<String, String> = HashMap::new();
    for (key, value) in std::env::vars() {
        if key == "HERDR_PLUGIN_STATE_DIR" || key == "HERDR_PLUGIN_CONFIG_DIR" {
            continue;
        }
        env.insert(key, value);
    }
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".to_string(),
        source.state_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "HERDR_PLUGIN_CONFIG_DIR".to_string(),
        source.config_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "HERDR_PLUGIN_ID".to_string(),
        CONVERSATIONS_PLUGIN_ID.to_string(),
    );

    let is_ts = source
        .cli_path
        .to_string_lossy()
        .ends_with(".ts");
    let (command, argv): (String, Vec<String>) = if is_ts {
        let mut v = vec![source.cli_path.to_string_lossy().into_owned()];
        v.extend(args.iter().cloned());
        ("bun".to_string(), v)
    } else {
        (source.cli_path.to_string_lossy().into_owned(), args.to_vec())
    };

    let timeout = Duration::from_millis(call.timeout_ms.unwrap_or(30_000) + 10_000);
    let mut cmd = Command::new(&command);
    cmd.args(&argv)
        .current_dir(&source.plugin_root)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_with_timeout(cmd, timeout)
        .ok_or_else(|| format!("conversations cli timed out or failed to spawn: {}", command))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json = serde_json::from_str::<Value>(stdout.trim()).ok();
    Ok(CliRunResult {
        status: output.status.code().unwrap_or(-1),
        stdout,
        stderr,
        json: json.filter(|v| v.is_object()),
    })
}

pub struct ExportResult {
    pub events: Vec<UsageEventRecord>,
    /// Session registry entries (last page's `sessions` array).
    pub sessions: Vec<Value>,
    pub pages: u64,
    pub complete: bool,
    pub malformed_lines: u64,
    pub duplicates_skipped: u64,
    pub restarted_sessions: Vec<String>,
    /// Conversations contract string observed on the first page.
    pub contract: Option<String>,
}

pub enum ExportOutcome {
    Ok(ExportResult),
    Err { error: String, kind: String },
}

const EXPORT_PAGE_LIMIT: u64 = 500;
/// Generous ceiling: 4000 pages × 500 events = 2M usage events.
const MAX_EXPORT_PAGES: u64 = 4_000;

/// Page through `conversations usage` until the opaque cursor is
/// exhausted, collecting every usage-carrying event plus the session
/// registry entries. Never partially silent: hitting MAX_EXPORT_PAGES
/// returns `complete: false` rather than a truncated "success".
pub fn export_usage_events(
    source: &SourceResolution,
    call: &CliCallOptions,
    selectors: Option<&UsageSelectors>,
) -> ExportOutcome {
    let mut events: Vec<UsageEventRecord> = Vec::new();
    let mut sessions: Vec<Value> = Vec::new();
    let mut contract: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut pages: u64 = 0;
    let mut malformed: u64 = 0;
    let mut duplicates: u64 = 0;
    let mut restarted: Vec<String> = Vec::new();
    let base_args = conversations_args(call, selectors);

    loop {
        if pages >= MAX_EXPORT_PAGES {
            return ExportOutcome::Ok(ExportResult {
                events,
                sessions,
                pages,
                complete: false,
                malformed_lines: malformed,
                duplicates_skipped: duplicates,
                restarted_sessions: restarted,
                contract,
            });
        }
        let mut args: Vec<String> = vec![
            "usage".to_string(),
            "--limit".to_string(),
            EXPORT_PAGE_LIMIT.to_string(),
        ];
        if let Some(c) = &cursor {
            args.push("--cursor".to_string());
            args.push(c.clone());
        }
        args.extend(base_args.iter().cloned());

        let run = match run_conversations_cli(source, &args, call) {
            Ok(r) => r,
            Err(e) => {
                return ExportOutcome::Err {
                    error: e,
                    kind: "conversations_error".to_string(),
                }
            }
        };
        let Some(page) = run.json.clone().filter(|_| run.status == 0) else {
            let mut detail = run.stderr.trim().to_string();
            if let Ok(err_json) = serde_json::from_str::<Value>(run.stderr.trim()) {
                if let Some(msg) = err_json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    detail = msg.to_string();
                }
            }
            let detail = if detail.is_empty() {
                run.stdout.chars().take(400).collect()
            } else {
                detail
            };
            return ExportOutcome::Err {
                error: format!(
                    "conversations usage failed (exit {}): {}",
                    run.status, detail
                ),
                kind: "conversations_error".to_string(),
            };
        };
        if let Some(c) = page.get("contract").and_then(|v| v.as_str()) {
            contract = Some(c.to_string());
        }
        pages += 1;

        if let Some(results) = page.get("results").and_then(|v| v.as_array()) {
            for r in results {
                let session_key = r.get("session_key").and_then(|v| v.as_str());
                let event = r.get("event").filter(|e| e.is_object());
                if let (Some(sk), Some(ev)) = (session_key, event) {
                    events.push(UsageEventRecord {
                        session_key: sk.to_string(),
                        event: ev.clone(),
                    });
                }
            }
        }
        if let Some(s) = page.get("sessions").and_then(|v| v.as_array()) {
            sessions = s.clone();
        }
        let page_info = page.get("page").cloned().unwrap_or(Value::Null);
        malformed += page_info
            .get("malformed_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        duplicates += page_info
            .get("duplicates_skipped")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if let Some(list) = page_info.get("restarted_sessions").and_then(|v| v.as_array()) {
            for key in list.iter().filter_map(|k| k.as_str()) {
                if !restarted.iter().any(|r| r == key) {
                    restarted.push(key.to_string());
                }
            }
        }

        let next = page_info.get("next_cursor").and_then(|v| v.as_str());
        let has_more = page_info.get("has_more").and_then(|v| v.as_bool()) == Some(true);
        if has_more {
            if let Some(n) = next {
                if !n.is_empty() {
                    cursor = Some(n.to_string());
                    continue;
                }
            }
        }
        return ExportOutcome::Ok(ExportResult {
            events,
            sessions,
            pages,
            complete: true,
            malformed_lines: malformed,
            duplicates_skipped: duplicates,
            restarted_sessions: restarted,
            contract,
        });
    }
}

pub struct CapabilityInfo {
    pub contract: Option<String>,
    pub providers: Vec<CapabilityProvider>,
}

pub struct CapabilityProvider {
    pub id: String,
    pub status: String,
    pub formats: Option<Vec<String>>,
    /// Adapter-declared usage accounting (e.g. cache_tokens).
    pub cache_tokens: Option<String>,
}

/// Fetch the conversations capability matrix (provider support statuses).
pub fn fetch_conversations_capability(
    source: &SourceResolution,
    call: &CliCallOptions,
) -> Option<CapabilityInfo> {
    let args = vec!["capability".to_string()];
    let run = run_conversations_cli(source, &args, call).ok()?;
    if run.status != 0 {
        return None;
    }
    let json = run.json?;
    let providers = json
        .get("providers")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    let id = p.get("id").and_then(|v| v.as_str())?.to_string();
                    let status = p
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let formats = p.get("formats").and_then(|v| v.as_array()).map(|a| {
                        a.iter()
                            .filter_map(|f| f.as_str().map(|s| s.to_string()))
                            .collect()
                    });
                    let cache_tokens = p
                        .get("usage")
                        .and_then(|u| u.get("cache_tokens"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    Some(CapabilityProvider {
                        id,
                        status,
                        formats,
                        cache_tokens,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(CapabilityInfo {
        contract: json
            .get("contract")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        providers,
    })
}
