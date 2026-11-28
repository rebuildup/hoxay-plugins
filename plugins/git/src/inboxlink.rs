//! Pane↔tracker link delegation for `git issue link|unlink|links`
//! (Issue #174). Rust port of `plugins/git/src/inbox.ts`.
//!
//! Tracker links are pane-scoped, user-declared work associations owned by
//! `rebuildup.inbox` (`herdr-plugins.inbox/v1` link/unlink/links). The git
//! plugin exposes the same operations as a thin delegating surface over
//! the inbox plugin's public CLI — the ADR-0013 sibling-plugin pattern:
//! resolve the plugin root (explicit override → sibling `../inbox` →
//! `herdr plugin list --json`), invoke its CLI (`bin/inbox` for the native
//! implementation, `bun src/cli.ts` for Bun-era roots), and never touch
//! the inbox plugin's state file directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use hoxay_herdr_client::dirs::{herdr_config_dir, herdr_state_dir, plugin_path_component};
use serde_json::Value;

pub const INBOX_PLUGIN_ID: &str = "rebuildup.inbox";

/// Directory name of a plugin id: `rebuildup.inbox` → `inbox`.
fn plugin_dir_name(plugin_id: &str) -> String {
    match plugin_id.find('.') {
        Some(dot) => plugin_id[dot + 1..].to_string(),
        None => plugin_id.to_string(),
    }
}

fn is_plugin_root(dir: &Path, plugin_id: &str) -> bool {
    let toml_path = dir.join("herdr-plugin.toml");
    if !toml_path.is_file() {
        return false;
    }
    // Native roots carry bin/<name>; Bun-era roots carry src/cli.ts.
    let cli_ts = dir.join("src").join("cli.ts");
    let bin = dir.join("bin").join(plugin_dir_name(plugin_id));
    let bin_any = std::fs::read_dir(dir.join("bin"))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !cli_ts.is_file() && !bin.is_file() && !bin_any {
        return false;
    }
    std::fs::read_to_string(&toml_path)
        .map(|text| text.contains(&format!("id = \"{plugin_id}\"")))
        .unwrap_or(false)
}

/// `herdr plugin list --json` → plugin_root for the given plugin id.
fn discover_via_herdr(plugin_id: &str) -> Option<String> {
    let herdr_bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let mut child = Command::new(&herdr_bin)
        .args(["plugin", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take()?;
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = std::time::Instant::now() + Duration::from_millis(15_000);
    let out = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(buf) => break Some(buf),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    }?;
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out);
    let parsed: Value = serde_json::from_str(&text).ok()?;
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
        if id == Some(plugin_id) {
            if let Some(root) = root {
                if !root.is_empty() {
                    return Some(root.to_string());
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub enum InboxRootResolution {
    Ok { root: PathBuf, via: &'static str },
    Err { error: String },
}

/// Resolve the rebuildup.inbox plugin root. Never fails hard — failures
/// are reported structurally so the CLI can surface `inbox_unavailable`.
pub fn resolve_inbox_plugin_root(explicit: Option<&str>) -> InboxRootResolution {
    if let Some(explicit) = explicit {
        if !explicit.is_empty() {
            let candidate = PathBuf::from(explicit);
            if is_plugin_root(&candidate, INBOX_PLUGIN_ID) {
                return InboxRootResolution::Ok {
                    root: candidate,
                    via: "override",
                };
            }
            return InboxRootResolution::Err {
                error: format!(
                    "plugin root {} is not a {INBOX_PLUGIN_ID} plugin",
                    candidate.display()
                ),
            };
        }
    }
    let sibling = crate::pluginroot::plugin_root()
        .parent()
        .map(|p| p.join(plugin_dir_name(INBOX_PLUGIN_ID)))
        .unwrap_or_else(|| PathBuf::from(plugin_dir_name(INBOX_PLUGIN_ID)));
    if is_plugin_root(&sibling, INBOX_PLUGIN_ID) {
        return InboxRootResolution::Ok {
            root: sibling,
            via: "sibling",
        };
    }
    if let Some(discovered) = discover_via_herdr(INBOX_PLUGIN_ID) {
        let path = PathBuf::from(&discovered);
        if is_plugin_root(&path, INBOX_PLUGIN_ID) {
            return InboxRootResolution::Ok {
                root: path,
                via: "herdr-plugin-list",
            };
        }
    }
    InboxRootResolution::Err {
        error: format!(
            "{INBOX_PLUGIN_ID} plugin not found; install it or set inboxPluginRoot \
             in config.json (resolution: override → sibling → herdr plugin list)"
        ),
    }
}

#[derive(Debug, Clone, Default)]
pub struct InboxCliCallOptions {
    pub session: Option<String>,
    pub socket_path: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug)]
pub struct InboxCliRunResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub json: Option<Value>,
    pub error: Option<Value>,
}

/// Env vars scrubbed before invoking the inbox CLI (they belong to
/// rebuildup.git and must never redirect the child's state resolution).
const SCRUBBED_ENV_VARS: &[&str] = &[
    "HERDR_PLUGIN_STATE_DIR",
    "HERDR_PLUGIN_CONFIG_DIR",
    "HERDR_PLUGIN_ID",
    "HERDR_PLUGIN_ROOT",
    "HERDR_PLUGIN_CONTEXT_JSON",
    "HERDR_PANE_ID",
];

/// Run one inbox CLI command. Native roots run `bin/inbox` directly;
/// Bun-era roots run `bun <root>/src/cli.ts`. Mirrors `runInboxCli`.
pub fn run_inbox_cli(
    root: &Path,
    args: &[String],
    call: &InboxCliCallOptions,
) -> Result<InboxCliRunResult, String> {
    let bin = root.join("bin").join(plugin_dir_name(INBOX_PLUGIN_ID));
    let cli_ts = root.join("src").join("cli.ts");

    let mut cmd_args: Vec<String> = Vec::new();
    let mut command = if bin.is_file() {
        let c = Command::new(&bin);
        cmd_args.push(bin.to_string_lossy().to_string());
        c
    } else if cli_ts.is_file() {
        let mut c = Command::new("bun");
        cmd_args.push(cli_ts.to_string_lossy().to_string());
        c.arg(&cli_ts);
        c
    } else {
        return Err(format!(
            "no CLI entrypoint found for plugin {INBOX_PLUGIN_ID} at {}",
            root.display()
        ));
    };
    for arg in args {
        command.arg(arg);
        cmd_args.push(arg.clone());
    }
    if let Some(session) = &call.session {
        command.arg("--session").arg(session);
    }
    if let Some(socket) = &call.socket_path {
        command.arg("--socket").arg(socket);
    }
    if let Some(timeout) = call.timeout_ms {
        command.arg("--timeout").arg(timeout.to_string());
    }
    command.current_dir(root);

    // Scrub this plugin's runtime env, then pin the *inbox* plugin's dirs
    // — derived platform dirs, deliberately NOT plugin_dirs() which would
    // read HERDR_PLUGIN_* and hand the child rebuildup.git's directories.
    for (key, _) in std::env::vars() {
        if SCRUBBED_ENV_VARS.contains(&key.as_str()) {
            command.env_remove(&key);
        }
    }
    let component = plugin_path_component(INBOX_PLUGIN_ID);
    let config_dir = herdr_config_dir()
        .join("plugins")
        .join("config")
        .join(&component);
    let state_dir = herdr_state_dir().join("plugins").join(&component);
    command.env("HERDR_PLUGIN_STATE_DIR", &state_dir);
    command.env("HERDR_PLUGIN_CONFIG_DIR", &config_dir);
    command.env("HERDR_PLUGIN_ID", INBOX_PLUGIN_ID);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let timeout = Duration::from_millis(call.timeout_ms.unwrap_or(30_000) + 15_000);
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn plugin CLI: {e}"))?;
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        tx_out.send(buf).ok();
    });
    let (tx_err, rx_err) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        tx_err.send(buf).ok();
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "plugin CLI timed out after {}ms",
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("plugin CLI wait failed: {e}")),
        }
    };
    let stdout_bytes = rx_out.recv().unwrap_or_default();
    let stderr_bytes = rx_err.recv().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    let json = serde_json::from_str::<Value>(stdout.trim()).ok();
    let error = serde_json::from_str::<Value>(&stderr)
        .ok()
        .and_then(|v| v.get("error").cloned());
    Ok(InboxCliRunResult {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
        json,
        error,
    })
}
