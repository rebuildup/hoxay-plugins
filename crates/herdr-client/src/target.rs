//! Target resolution, session namespacing, and ambient pane identity.
//! Mirrors `packages/herdr-client/src/socket.ts` (resolveTarget,
//! sessionKeyFor, ambientIdentity, assertAmbientPaneIdentity,
//! socketPathForSession, pluginDirs).

use std::path::Path;

use serde_json::Value;

use crate::dirs::{env_nonempty, herdr_config_dir};

/// How the resolved socket target was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// Explicit `--socket`/`--session` CLI options.
    Flag,
    /// Ambient `HERDR_SOCKET_PATH`/`HERDR_SESSION` (consistent pair).
    Env,
    /// No explicit or ambient target — the implicit default session
    /// socket. Mutating operations must not use this.
    Default,
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub socket_path: String,
    pub source: TargetSource,
    /// Session name when the target is a named-session socket
    /// (`default` for the default session); `None` for custom socket
    /// paths whose session cannot be named.
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetErrorCode {
    TargetConflict,
    TargetRequired,
    InvalidSession,
    AmbientIdentityMismatch,
}

impl TargetErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetErrorCode::TargetConflict => "target_conflict",
            TargetErrorCode::TargetRequired => "target_required",
            TargetErrorCode::InvalidSession => "invalid_session",
            TargetErrorCode::AmbientIdentityMismatch => "ambient_identity_mismatch",
        }
    }
}

/// Structured target-resolution failure. `usage` marks errors caused by
/// an explicit CLI value (callers map them to exit code 2); ambient
/// conflicts and missing mutation targets are runtime errors (exit 1).
#[derive(Debug)]
pub struct TargetResolutionError {
    pub code: TargetErrorCode,
    pub message: String,
    pub usage: bool,
}

impl TargetResolutionError {
    pub fn new(code: TargetErrorCode, message: impl Into<String>) -> Self {
        TargetResolutionError {
            code,
            message: message.into(),
            usage: false,
        }
    }
    pub fn usage(code: TargetErrorCode, message: impl Into<String>) -> Self {
        TargetResolutionError {
            code,
            message: message.into(),
            usage: true,
        }
    }
}

impl std::fmt::Display for TargetResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}
impl std::error::Error for TargetResolutionError {}

#[derive(Debug, Clone, Default)]
pub struct ResolveTargetOptions {
    /// Explicit `--session NAME`.
    pub session: Option<String>,
    /// Explicit `--socket PATH`.
    pub socket_path: Option<String>,
    /// Mutating operation: fail closed with `target_required` when
    /// resolution would fall back to the implicit default session socket.
    pub mutation: bool,
}

/// Mirrors herdr's `validate_name` (src/session.rs): ASCII alnum + `.` `_` `-`.
const SESSION_NAME_PATTERN: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-";

/// Mirrors herdr's `validate_name` (src/session.rs): ASCII alnum + `.` `_` `-`.
pub fn is_valid_session_name(name: &str) -> bool {
    // "default" is a valid name upstream; socket_path_for_session maps it
    // to the default session socket, mirroring herdr's active_name() filter.
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name.bytes().all(|b| SESSION_NAME_PATTERN.contains(&b))
}

/// Default session socket: `<herdr config dir>/herdr.sock`.
pub fn default_socket_path() -> String {
    Path::new(&herdr_config_dir())
        .join("herdr.sock")
        .to_string_lossy()
        .into_owned()
}

/// Named-session socket: `<herdr config dir>/sessions/<name>/herdr.sock`.
pub fn session_socket_path(session: &str) -> String {
    Path::new(&herdr_config_dir())
        .join("sessions")
        .join(session)
        .join("herdr.sock")
        .to_string_lossy()
        .into_owned()
}

/// Session-name → socket path. `default` maps to the default session
/// socket, mirroring herdr's `data_dir_for(None)`.
pub fn socket_path_for_session(session: &str) -> String {
    if session == "default" {
        default_socket_path()
    } else {
        session_socket_path(session)
    }
}

/// Canonicalize a socket path for equality checks: resolve symlinks when
/// the path exists, otherwise normalize lexically (path.resolve).
fn canonical_path(p: &str) -> String {
    if let Ok(canon) = std::fs::canonicalize(p) {
        return canon.to_string_lossy().into_owned();
    }
    let path = Path::new(p);
    if path.is_absolute() {
        return p.to_string();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

fn same_path(a: &str, b: &str) -> bool {
    canonical_path(a) == canonical_path(b)
}

fn validate_session_name(name: &str, flag: bool) -> Result<String, TargetResolutionError> {
    if !is_valid_session_name(name) {
        return Err(if flag {
            TargetResolutionError::usage(
                TargetErrorCode::InvalidSession,
                format!(
                    "--session is not a valid herdr session name: {}",
                    serde_json::to_string(name).unwrap_or_else(|_| format!("\"{}\"", name))
                ),
            )
        } else {
            TargetResolutionError::new(
                TargetErrorCode::InvalidSession,
                format!(
                    "HERDR_SESSION is not a valid herdr session name: {}",
                    serde_json::to_string(name).unwrap_or_else(|_| format!("\"{}\"", name))
                ),
            )
        });
    }
    Ok(name.to_string())
}

/// Resolve which herdr socket to talk to.
///
/// Precedence mirrors herdr's `active_api_socket_path` with explicit
/// plugin flags promoted above ambient environment (upstream:
/// `--session` > `HERDR_SOCKET_PATH` > `HERDR_SESSION` > default).
/// Any contradiction fails closed with `target_conflict`.
pub fn resolve_target(
    options: &ResolveTargetOptions,
) -> Result<ResolvedTarget, TargetResolutionError> {
    let flag_socket = env_nonempty_opt(options.socket_path.as_deref());
    let flag_session_raw = env_nonempty_opt(options.session.as_deref());
    let env_socket = env_nonempty("HERDR_SOCKET_PATH");
    let env_session_raw = env_nonempty("HERDR_SESSION");

    let flag_session = match &flag_session_raw {
        Some(name) => Some(validate_session_name(name, true)?),
        None => None,
    };
    let env_session = match &env_session_raw {
        Some(name) => Some(validate_session_name(name, false)?),
        None => None,
    };

    if let (Some(fs), Some(fses)) = (&flag_socket, &flag_session) {
        let session_socket = socket_path_for_session(fses);
        if !same_path(fs, &session_socket) {
            return Err(TargetResolutionError::new(
                TargetErrorCode::TargetConflict,
                format!(
                    "--socket {} does not match --session {} ({})",
                    fs, fses, session_socket
                ),
            ));
        }
    }

    if let Some(fs) = &flag_socket {
        if let Some(es) = &env_socket {
            if !same_path(fs, es) {
                return Err(TargetResolutionError::new(
                    TargetErrorCode::TargetConflict,
                    format!(
                        "--socket {} conflicts with ambient HERDR_SOCKET_PATH {}; \
                         unset the ambient variable or drop the explicit flag",
                        fs, es
                    ),
                ));
            }
        }
        if let Some(es) = &env_session {
            let env_socket_path = socket_path_for_session(es);
            if !same_path(fs, &env_socket_path) {
                return Err(TargetResolutionError::new(
                    TargetErrorCode::TargetConflict,
                    format!(
                        "--socket {} conflicts with ambient HERDR_SESSION {} ({}); \
                         unset the ambient variable or drop the explicit flag",
                        fs, es, env_socket_path
                    ),
                ));
            }
        }
        return Ok(ResolvedTarget {
            socket_path: fs.clone(),
            source: TargetSource::Flag,
            session: flag_session.or(env_session),
        });
    }

    if let Some(fses) = &flag_session {
        let resolved = socket_path_for_session(fses);
        if let Some(es) = &env_socket {
            if !same_path(&resolved, es) {
                return Err(TargetResolutionError::new(
                    TargetErrorCode::TargetConflict,
                    format!(
                        "--session {} ({}) conflicts with ambient HERDR_SOCKET_PATH {}; \
                         unset the ambient variable or drop the explicit flag",
                        fses, resolved, es
                    ),
                ));
            }
        }
        if let Some(es) = &env_session {
            if es != fses {
                return Err(TargetResolutionError::new(
                    TargetErrorCode::TargetConflict,
                    format!(
                        "--session {} conflicts with ambient HERDR_SESSION {}; \
                         unset the ambient variable or drop the explicit flag",
                        fses, es
                    ),
                ));
            }
        }
        return Ok(ResolvedTarget {
            socket_path: resolved,
            source: TargetSource::Flag,
            session: Some(fses.clone()),
        });
    }

    if let Some(es) = &env_socket {
        if let Some(eses) = &env_session {
            let session_socket = socket_path_for_session(eses);
            if !same_path(es, &session_socket) {
                return Err(TargetResolutionError::new(
                    TargetErrorCode::TargetConflict,
                    format!(
                        "ambient HERDR_SOCKET_PATH {} is inconsistent with ambient \
                         HERDR_SESSION {} ({})",
                        es, eses, session_socket
                    ),
                ));
            }
        }
        return Ok(ResolvedTarget {
            socket_path: es.clone(),
            source: TargetSource::Env,
            session: env_session.clone(),
        });
    }

    if let Some(eses) = &env_session {
        return Ok(ResolvedTarget {
            socket_path: socket_path_for_session(eses),
            source: TargetSource::Env,
            session: Some(eses.clone()),
        });
    }

    if options.mutation {
        return Err(TargetResolutionError::new(
            TargetErrorCode::TargetRequired,
            "mutating operations require an explicit target: pass --socket/--session or run \
             inside a herdr context that exports HERDR_SOCKET_PATH/HERDR_SESSION",
        ));
    }
    Ok(ResolvedTarget {
        socket_path: default_socket_path(),
        source: TargetSource::Default,
        session: None,
    })
}

/// Treat an explicit option value like `env_nonempty` treats env vars:
/// trim and drop empties. (NOT `env_nonempty` itself — that reads a
/// variable *named* by its argument, which would silently discard every
/// explicit `--socket`/`--session` value.)
fn env_nonempty_opt(value: Option<&str>) -> Option<String> {
    value.and_then(|v| {
        let t = v.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

/// Resolve which herdr socket to talk to. Kept for callers that only
/// need the path; prefer `resolve_target` when source/session matters.
pub fn resolve_socket_path(
    options: &ResolveTargetOptions,
) -> Result<String, TargetResolutionError> {
    Ok(resolve_target(options)?.socket_path)
}

/// Session discriminator for cross-plugin state namespacing: hostname
/// plus the canonical (symlink-resolved) socket path. Shared by plugins
/// that namespace per-session state.
pub fn session_key_for(options: &ResolveTargetOptions) -> Result<String, TargetResolutionError> {
    let socket_path = resolve_target(options)?.socket_path;
    let canonical = canonical_path(&socket_path);
    Ok(format!("{} {}", hostname(), canonical))
}

/// Hostname used in session keys (os.hostname() equivalent).
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        // SAFETY: gethostname writes at most buf.len() bytes into buf and
        // reports the length via rc; the buffer is only read when rc==0.
        unsafe {
            let mut buf = [0i8; 256];
            let rc = libc::gethostname(buf.as_mut_ptr(), buf.len());
            if rc == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                let bytes: Vec<u8> = buf[..end].iter().map(|&b| b as u8).collect();
                if let Ok(s) = String::from_utf8(bytes) {
                    let t = s.trim().to_string();
                    if !t.is_empty() {
                        return t;
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(s) = std::env::var("COMPUTERNAME") {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
    }
    "localhost".to_string()
}

/// Ambient pane/workspace/tab identity exported by herdr into agent
/// panes (`HERDR_PANE_ID`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`) and
/// plugin invocations (`HERDR_PLUGIN_CONTEXT_JSON`).
#[derive(Debug, Clone, Default)]
pub struct AmbientIdentity {
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
}

fn plugin_context_identity() -> Value {
    let Some(raw) = env_nonempty("HERDR_PLUGIN_CONTEXT_JSON") else {
        return Value::Null;
    };
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn context_str(context: &Value, key: &str) -> Option<String> {
    context
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

/// Read the ambient identity bundle. Explicit env vars win over the
/// plugin context JSON for each field; a pane id present in both with
/// different values is a contradiction and throws
/// `ambient_identity_mismatch`.
pub fn ambient_identity() -> Result<AmbientIdentity, TargetResolutionError> {
    let context = plugin_context_identity();
    let env_pane = env_nonempty("HERDR_PANE_ID");
    let ctx_pane = context_str(&context, "focused_pane_id");
    if let (Some(ep), Some(cp)) = (&env_pane, &ctx_pane) {
        if ep != cp {
            return Err(TargetResolutionError::new(
                TargetErrorCode::AmbientIdentityMismatch,
                format!(
                    "HERDR_PANE_ID {} conflicts with \
                     HERDR_PLUGIN_CONTEXT_JSON.focused_pane_id {}",
                    ep, cp
                ),
            ));
        }
    }
    Ok(AmbientIdentity {
        pane_id: env_pane.or(ctx_pane),
        workspace_id: env_nonempty("HERDR_WORKSPACE_ID")
            .or_else(|| context_str(&context, "workspace_id")),
        tab_id: env_nonempty("HERDR_TAB_ID").or_else(|| context_str(&context, "tab_id")),
    })
}

/// Minimal pane shape needed for the ambient consistency check.
pub trait AmbientPane {
    fn pane_id(&self) -> &str;
    fn workspace_id(&self) -> Option<&str>;
    fn tab_id(&self) -> Option<&str>;
}

/// Verify that ambient workspace/tab identity is consistent with the
/// pane resolved in the target session. Call only when the pane id
/// itself came from the ambient identity. A missing pane field cannot
/// be verified and fails closed.
pub fn assert_ambient_pane_identity<P: AmbientPane>(
    pane: &P,
    identity: &AmbientIdentity,
) -> Result<(), TargetResolutionError> {
    let mut mismatches: Vec<String> = Vec::new();
    if let Some(expected) = &identity.workspace_id {
        let actual = pane.workspace_id();
        if actual != Some(expected.as_str()) {
            mismatches.push(format!(
                "workspace_id {} != ambient {}",
                json_str_opt(actual),
                serde_json::to_string(expected).unwrap_or_else(|_| format!("\"{}\"", expected))
            ));
        }
    }
    if let Some(expected) = &identity.tab_id {
        let actual = pane.tab_id();
        if actual != Some(expected.as_str()) {
            mismatches.push(format!(
                "tab_id {} != ambient {}",
                json_str_opt(actual),
                serde_json::to_string(expected).unwrap_or_else(|_| format!("\"{}\"", expected))
            ));
        }
    }
    if !mismatches.is_empty() {
        return Err(TargetResolutionError::new(
            TargetErrorCode::AmbientIdentityMismatch,
            format!(
                "ambient identity does not belong to resolved pane {} in the target \
                 session: {}",
                pane.pane_id(),
                mismatches.join("; ")
            ),
        ));
    }
    Ok(())
}

fn json_str_opt(value: Option<&str>) -> String {
    match value {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s)),
        None => "null".to_string(),
    }
}
