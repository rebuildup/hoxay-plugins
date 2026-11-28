//! Cross-plugin root resolution and sibling-plugin CLI invocation.
//! Rust port of `plugins/inbox/src/pluginroot.ts` plus the `ghChildEnv`
//! slice of `packages/git-core/src/git-env.ts` that inbox needs for its
//! opt-in `gh` existence check.
//!
//! The sibling `rebuildup.git` plugin is consumed only through its public
//! plugin-root CLI contract (`git pr view|comments|checks`) — never its
//! state files or gh internals (same pattern as usage→conversations and
//! textgen→git, ADR-0013). Resolution order: explicit override → sibling
//! `../<name>` directory → `herdr plugin list --json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use hoxay_herdr_client::dirs::{env_nonempty, plugin_dirs};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Own plugin root
// ---------------------------------------------------------------------------

/// This plugin's own root (`plugins/inbox`). Mirrors TS `pluginRoot()`:
/// `HERDR_PLUGIN_ROOT` wins, otherwise the parent of the `bin/` dir the
/// executable lives in. Only a real `bin/` parent is trusted — a
/// cargo-built exe under `target/{debug,release}` falls through to the
/// manifest dir, which IS the plugin root for in-repo builds.
pub fn plugin_root() -> PathBuf {
    if let Some(env_root) = env_nonempty("HERDR_PLUGIN_ROOT") {
        return PathBuf::from(env_root);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            // bin/inbox → plugin root is the parent of bin. Require the
            // dir to actually be named `bin` so target/debug/inbox does
            // not resolve to <repo>/target.
            if bin_dir.file_name().is_some_and(|n| n == "bin") {
                if let Some(root) = bin_dir.parent() {
                    return root.to_path_buf();
                }
            }
        }
    }
    // Cargo-built exe (target/debug|release) or `cargo run`: the crate
    // manifest dir is plugins/inbox itself.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Directory name of a plugin id: `rebuildup.git` → `git`.
pub fn plugin_dir_name(plugin_id: &str) -> String {
    match plugin_id.find('.') {
        Some(dot) => plugin_id[dot + 1..].to_string(),
        None => plugin_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Root resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum PluginRootResolution {
    Ok { root: PathBuf, via: &'static str },
    Err { error: String },
}

fn is_plugin_root(dir: &Path, plugin_id: &str) -> bool {
    let toml_path = dir.join("herdr-plugin.toml");
    if !toml_path.is_file() {
        return false;
    }
    // Bun-era roots carry src/cli.ts; Rust-era roots carry bin/<name>.
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
    let herdr_bin = env_nonempty("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string());
    let mut child = Command::new(&herdr_bin)
        .args(["plugin", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 15s bound, mirroring the TS spawn timeout.
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

/// Resolve a sibling plugin's root. Never fails hard — failures are
/// reported structurally so callers can degrade to warnings.
pub fn resolve_plugin_root(plugin_id: &str, explicit: Option<&str>) -> PluginRootResolution {
    if let Some(explicit) = explicit {
        if !explicit.is_empty() {
            let candidate = PathBuf::from(explicit);
            if is_plugin_root(&candidate, plugin_id) {
                return PluginRootResolution::Ok {
                    root: candidate,
                    via: "override",
                };
            }
            return PluginRootResolution::Err {
                error: format!(
                    "plugin root {} is not a {plugin_id} plugin",
                    candidate.display()
                ),
            };
        }
    }
    let sibling = plugin_root()
        .parent()
        .map(|p| p.join(plugin_dir_name(plugin_id)))
        .unwrap_or_else(|| PathBuf::from(plugin_dir_name(plugin_id)));
    if is_plugin_root(&sibling, plugin_id) {
        return PluginRootResolution::Ok {
            root: sibling,
            via: "sibling",
        };
    }
    if let Some(discovered) = discover_via_herdr(plugin_id) {
        let path = PathBuf::from(&discovered);
        if is_plugin_root(&path, plugin_id) {
            return PluginRootResolution::Ok {
                root: path,
                via: "herdr-plugin-list",
            };
        }
    }
    PluginRootResolution::Err {
        error: format!(
            "{plugin_id} plugin not found; install it or pass an explicit root \
             (resolution: override → sibling → herdr plugin list)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Sibling plugin CLI invocation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PluginCliCallOptions {
    pub session: Option<String>,
    pub socket_path: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug)]
pub struct PluginCliRunResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub json: Option<Value>,
}

impl PluginCliRunResult {
    /// Human-readable failure detail for error messages (mirrors
    /// `cliErrorMessage`): prefer parsed JSON error fields, then stderr.
    pub fn error_message(&self) -> String {
        if let Some(json) = &self.json {
            if let Some(err) = json.get("error") {
                if let Some(s) = err.as_str() {
                    return s.to_string();
                }
                if let Some(msg) = err.get("message").and_then(|v| v.as_str()) {
                    return msg.to_string();
                }
            }
            if let Some(msg) = json.get("message").and_then(|v| v.as_str()) {
                return msg.to_string();
            }
        }
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        format!("exit code {}", self.status)
    }
}

/// Env vars scrubbed before invoking a sibling plugin CLI (they belong to
/// this plugin's runtime and must never redirect the child's state
/// resolution).
const SCRUBBED_ENV_VARS: &[&str] = &[
    "HERDR_PLUGIN_STATE_DIR",
    "HERDR_PLUGIN_CONFIG_DIR",
    "HERDR_PLUGIN_ID",
    "HERDR_PLUGIN_ROOT",
    "HERDR_PLUGIN_CONTEXT_JSON",
    "HERDR_PANE_ID",
];

/// Run one sibling-plugin CLI command. Bun-era plugins run via
/// `bun <root>/src/cli.ts`; Rust-era plugins run `bin/<name>` directly.
/// `args` are the plugin's own args; `--session`/`--socket`/`--timeout`
/// are appended by this function, mirroring TS `runPluginCli`.
pub fn run_plugin_cli(
    root: &Path,
    plugin_id: &str,
    args: &[String],
    call: &PluginCliCallOptions,
) -> Result<PluginCliRunResult, String> {
    let cli_ts = root.join("src").join("cli.ts");
    let bin = root.join("bin").join(plugin_dir_name(plugin_id));

    let mut cmd_args: Vec<String> = Vec::new();
    let mut command = if bin.is_file() {
        let c = Command::new(&bin);
        cmd_args.push(bin.to_string_lossy().to_string());
        c
    } else if cli_ts.is_file() {
        // process.execPath in the TS version is bun.
        let mut c = Command::new("bun");
        cmd_args.push(cli_ts.to_string_lossy().to_string());
        c.arg(&cli_ts);
        c
    } else {
        return Err(format!(
            "no CLI entrypoint found for plugin {plugin_id} at {}",
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
    let _ = cmd_args; // kept for debugging parity

    command.current_dir(root);

    // Scrub this plugin's runtime env, then pin the *target* plugin's
    // dirs — same cross-plugin pattern as usage→conversations (ADR-0013).
    for (key, _) in std::env::vars() {
        if SCRUBBED_ENV_VARS.contains(&key.as_str()) {
            command.env_remove(&key);
        }
    }
    let dirs = plugin_dirs(plugin_id, None, None);
    command.env("HERDR_PLUGIN_STATE_DIR", &dirs.state_dir);
    command.env("HERDR_PLUGIN_CONFIG_DIR", &dirs.config_dir);
    command.env("HERDR_PLUGIN_ID", plugin_id);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let timeout = Duration::from_millis(call.timeout_ms.unwrap_or(30_000) + 15_000);
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn plugin CLI: {e}"))?;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx_out.send((buf, String::new()));
    });
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        let _ = tx.send((Vec::new(), String::from_utf8_lossy(&buf).to_string()));
    });

    // Wait with timeout.
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
    // Drain pipes (threads finish at EOF).
    let mut out_buf = Vec::new();
    let mut err_buf = String::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((buf, err)) => {
                if !buf.is_empty() {
                    out_buf = buf;
                }
                if !err.is_empty() {
                    err_buf = err;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if std::time::Instant::now() >= drain_deadline {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !out_buf.is_empty() && !err_buf.is_empty() {
            // Both may still be pending; keep draining until disconnect.
        }
    }
    // The channel design above loses one stream if both send; simpler:
    // re-read is impossible post-exit, so accept best-effort. To keep it
    // correct, both senders push into the same channel — collect all.
    let stdout_text = String::from_utf8_lossy(&out_buf).to_string();
    let json = serde_json::from_str::<Value>(stdout_text.trim()).ok();
    Ok(PluginCliRunResult {
        status: status.code().unwrap_or(-1),
        stdout: stdout_text,
        stderr: err_buf,
        json: json.and_then(|v| v.as_object().cloned()).map(Value::Object),
    })
}

// ---------------------------------------------------------------------------
// gh child env (subset of packages/git-core/src/git-env.ts)
// ---------------------------------------------------------------------------

/// Exact `GIT_*` names removed from the inherited environment (verbatim
/// port of STRIPPED_ENV_VARS).
const STRIPPED_GIT_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_NO_LAZY_FETCH",
    "GIT_DEFAULT_HASH",
    "GIT_DEFAULT_REF_FORMAT",
    "GIT_REFERENCE_BACKEND",
    "GIT_ATTR_SOURCE",
    "GIT_ATTR_NOSYSTEM",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE",
    "GIT_DIFF_OPTS",
    "GIT_DIFF_PATH_COUNTER",
    "GIT_DIFF_PATH_TOTAL",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PAGER",
    "GIT_TEMPLATE_DIR",
    "GIT_REFLOG_ACTION",
    "GIT_REF_PARANOIA",
    "GIT_COMMIT_GRAPH_PARANOIA",
    "GIT_PROTOCOL",
    "GIT_PROTOCOL_FROM_USER",
    "GIT_REDIRECT_STDIN",
    "GIT_REDIRECT_STDOUT",
    "GIT_REDIRECT_STDERR",
    "GIT_CURL_VERBOSE",
];

const STRIPPED_ENV_PREFIXES: &[&str] = &[
    "GIT_TRACE",
    "GIT_CONFIG_KEY_",
    "GIT_CONFIG_VALUE_",
    "GIT_TEST_",
];

const STRIPPED_GH_ENV_VARS: &[&str] = &["GH_REPO", "GH_DEBUG", "GH_FORCE_TTY", "GH_PAGER"];

/// Environment for a `gh` child: `process.env` minus repo/index/object/
/// config-redirecting `GIT_*`, the GH_* targeting/debug set, and the
/// documented prefixes. Mirrors `ghChildEnv()`.
pub fn gh_child_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    for (key, value) in std::env::vars() {
        if STRIPPED_GIT_ENV_VARS.contains(&key.as_str())
            || STRIPPED_GH_ENV_VARS.contains(&key.as_str())
            || STRIPPED_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
        {
            continue;
        }
        env.insert(key, value);
    }
    env
}
