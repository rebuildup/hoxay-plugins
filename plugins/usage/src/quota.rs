//! Subscription quota/reset probing for rebuildup.usage (Issue #91 /
//! parent #28 T2, credential boundary ADR-0014). Rust port of
//! `plugins/usage/src/quota.ts` (retired; git history preserves it).
//!
//! Reads provider-owned credential artifacts *only* to authenticate
//! read-only quota endpoints, and reports usage windows + reset
//! instants. Credential values never leave the fetch closure: they are
//! not logged, not persisted, not cached, and never appear in output or
//! errors (ADR-0014). Successful readings are cached under the plugin
//! state dir for a short TTL; stale cache is served — explicitly
//! flagged — when the endpoint cannot be reached. Providers without a
//! quota artifact report `unsupported`, never a fabricated zero.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hoxay_herdr_client::dirs::plugin_dirs;
use serde_json::{json, Map, Value};

use crate::model::{QuotaEndpoints, QuotaWindow};
use crate::state::{gc_plugin_state_cache, PLUGIN_CACHE_RETENTION_SECONDS};

const PLUGIN_ID: &str = "rebuildup.usage";

/// Short-term cache for successful quota readings (seconds).
pub const QUOTA_CACHE_TTL_SECONDS: i64 = 300;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

const CLAUDE_CREDENTIALS_SERVICE: &str = "Claude Code-credentials";
const CLAUDE_DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_DEFAULT_USAGE_ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_DEFAULT_CREDITS_ENDPOINT: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

// --- provider registry ---------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QuotaProviderSpec {
    pub id: String,
    pub implemented: bool,
    /// Why the provider cannot report quota (absent when implemented).
    pub unsupported_reason: Option<String>,
}

/// Providers with a known subscription-quota artifact. `opencode` is
/// declared unsupported up front: its auth store holds per-provider
/// API keys, and no subscription quota/reset artifact exists to read.
pub fn quota_providers() -> Vec<QuotaProviderSpec> {
    vec![
        QuotaProviderSpec {
            id: "claude".to_string(),
            implemented: true,
            unsupported_reason: None,
        },
        QuotaProviderSpec {
            id: "codex".to_string(),
            implemented: true,
            unsupported_reason: None,
        },
        QuotaProviderSpec {
            id: "opencode".to_string(),
            implemented: false,
            unsupported_reason: Some(
                "opencode auth.json stores per-provider API keys; no subscription quota artifact exists".to_string(),
            ),
        },
    ]
}

// --- shared helpers ------------------------------------------------------------

fn clamp_percent(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

/// `new Date(ms).toISOString()` equivalent (millis precision, `Z`).
pub fn iso_from_ms(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64)?;
    Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn iso_from_epoch_seconds(value: Option<f64>) -> Option<String> {
    let v = value?;
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    iso_from_ms(v * 1000.0)
}

fn iso_from_string(value: Option<&Value>) -> Option<String> {
    let s = value.and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    let ms = crate::cli::parse_js_date(s)?;
    iso_from_ms(ms)
}

pub fn as_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn num_or_null(value: Option<&Value>) -> Option<f64> {
    value.and_then(|v| v.as_f64()).filter(|f| f.is_finite())
}

// --- credential boundary (ADR-0014) --------------------------------------------
//
// The only credential material this module may hold is an access token
// inside a `CredentialOk` returned by a `read_*_credential` function,
// used solely as an Authorization header for the provider's own quota
// endpoint. Tokens are never: written to state/config, included in
// error messages or logs, returned in command output, or passed to any
// subprocess other than the platform credential reader (`security`).

#[derive(Debug)]
pub enum CredentialFailure {
    Unsupported,
    NoCredential,
    CredentialExpired,
}

impl CredentialFailure {
    fn as_str(&self) -> &'static str {
        match self {
            CredentialFailure::Unsupported => "unsupported",
            CredentialFailure::NoCredential => "no_credential",
            CredentialFailure::CredentialExpired => "credential_expired",
        }
    }
}

#[derive(Debug)]
pub struct CredentialOk {
    /// Access token — Authorization header only, never emitted.
    pub token: String,
    /// Credential expiry in ms epoch; null when the store does not say.
    pub expires_at_ms: Option<f64>,
    /// Provider plan label safe to report (e.g. subscription type).
    pub plan: Option<String>,
    /// Extra non-secret request headers (e.g. codex account id).
    pub headers: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum CredentialResult {
    Ok(CredentialOk),
    Err { status: CredentialFailure, detail: String },
}

fn home_dir() -> String {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "/".to_string())
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn env_get(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_json_file(file: &Path) -> Option<Value> {
    let text = fs::read_to_string(file).ok()?;
    serde_json::from_str::<Value>(&text).ok()
}

/// Claude credential: `$CLAUDE_CONFIG_DIR/.credentials.json`
/// (`claudeAiOauth.accessToken`/`expiresAt`/`subscriptionType`),
/// falling back to the macOS Keychain item `Claude Code-credentials`
/// only when `CLAUDE_CONFIG_DIR` is unset — an explicit config dir is
/// an explicit credential location and must not be second-guessed by
/// the Keychain. `expiresAt` past → `credential_expired`; the plugin
/// never refreshes tokens (that would be a credential mutation).
fn read_claude_credential() -> CredentialResult {
    let explicit_dir = env_get("CLAUDE_CONFIG_DIR");
    let config_dir = explicit_dir
        .clone()
        .unwrap_or_else(|| Path::new(&home_dir()).join(".claude").to_string_lossy().into_owned());
    let file = Path::new(&config_dir).join(".credentials.json");

    let mut parsed = read_json_file(&file);
    #[cfg(target_os = "macos")]
    if parsed.is_none() && explicit_dir.is_none() {
        // macOS Claude Code keeps credentials in the login Keychain; the
        // `security` CLI prints the JSON blob on stdout. Failure is a
        // normal "no credential" outcome, never an error dump.
        parsed = run_security_find();
    }

    let Some(root) = parsed else {
        return CredentialResult::Err {
            status: CredentialFailure::NoCredential,
            detail: format!(
                "no Claude credential at {} (and no Keychain item)",
                file.display()
            ),
        };
    };

    let oauth = root
        .get("claudeAiOauth")
        .and_then(|v| v.as_object());
    let token = oauth
        .and_then(|o| o.get("accessToken"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        return CredentialResult::Err {
            status: CredentialFailure::NoCredential,
            detail: "Claude credential file has no claudeAiOauth.accessToken".to_string(),
        };
    }
    let expires_at_ms = oauth
        .and_then(|o| o.get("expiresAt"))
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite());
    if let Some(exp) = expires_at_ms {
        if exp > 0.0 && exp <= now_ms() {
            let iso = iso_from_ms(exp).unwrap_or_default();
            return CredentialResult::Err {
                status: CredentialFailure::CredentialExpired,
                detail: format!(
                    "Claude OAuth token expired at {}; token refresh is out of scope",
                    iso
                ),
            };
        }
    }
    let plan = oauth
        .and_then(|o| o.get("subscriptionType"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    CredentialResult::Ok(CredentialOk {
        token,
        expires_at_ms,
        plan,
        headers: Vec::new(),
    })
}

/// `security find-generic-password -s "Claude Code-credentials" -w` →
/// parsed JSON, or None. 5s timeout, stdout only. The deadline is
/// enforced by `run_with_timeout` (reader threads drain the pipe while
/// `try_wait` polls) — a plain blocking `read` would park forever if
/// macOS shows a keychain authorization dialog and the child produces
/// no output.
#[cfg(target_os = "macos")]
fn run_security_find() -> Option<Value> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("security");
    cmd.args([
        "find-generic-password",
        "-s",
        CLAUDE_CREDENTIALS_SERVICE,
        "-w",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let output = crate::source::run_with_timeout(cmd, Duration::from_secs(5))?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(text.trim()).ok()
}

/// Decode a JWT payload without verifying; returns whitelisted claims
/// only (the full claims object — callers pick fields).
pub fn decode_jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload.len() % 4 {
        2 => format!("{}==", payload),
        3 => format!("{}=", payload),
        _ => payload,
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let parsed = serde_json::from_str::<Value>(&text).ok()?;
    if parsed.is_object() {
        Some(parsed)
    } else {
        None
    }
}

const CODEX_PLAN_LABELS: &[(&str, &str)] = &[
    ("free", "Free"),
    ("go", "Go"),
    ("plus", "Plus"),
    ("pro", "Pro"),
    ("team", "Team"),
    ("business", "Business"),
    ("enterprise", "Enterprise"),
];

fn codex_plan_label(raw: &str) -> String {
    CODEX_PLAN_LABELS
        .iter()
        .find(|(k, _)| *k == raw)
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Codex credential: `$CODEX_HOME/auth.json`
/// (`tokens.access_token` + `tokens.id_token` claims). Only
/// ChatGPT-account auth has a subscription quota endpoint;
/// `auth_mode: "apikey"` is reported `unsupported`, and a
/// missing/empty store is `no_credential`.
fn read_codex_credential() -> CredentialResult {
    let home = env_get("CODEX_HOME")
        .unwrap_or_else(|| Path::new(&home_dir()).join(".codex").to_string_lossy().into_owned());
    let file = Path::new(&home).join("auth.json");
    let Some(parsed) = read_json_file(&file) else {
        return CredentialResult::Err {
            status: CredentialFailure::NoCredential,
            detail: format!("no Codex credential at {}", file.display()),
        };
    };
    let auth_mode = parsed
        .get("auth_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tokens = parsed.get("tokens").and_then(|v| v.as_object());
    let access_token = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if auth_mode == "apikey" || (access_token.is_empty() && auth_mode != "chatgpt") {
        return CredentialResult::Err {
            status: CredentialFailure::Unsupported,
            detail: format!(
                "codex auth_mode \"{}\" has no subscription quota artifact",
                if auth_mode.is_empty() { "unknown" } else { &auth_mode }
            ),
        };
    }
    if access_token.is_empty() {
        return CredentialResult::Err {
            status: CredentialFailure::NoCredential,
            detail: "codex auth.json has no tokens.access_token".to_string(),
        };
    }

    // Whitelisted id_token claims only — the token itself is never
    // kept beyond the Authorization header and never reported.
    let id_token = tokens
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let claims = if !id_token.is_empty() {
        decode_jwt_claims(id_token)
    } else {
        None
    };
    let account_id = claims
        .as_ref()
        .and_then(|c| c.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let plan_raw = claims
        .as_ref()
        .and_then(|c| c.get("plan_type").or_else(|| c.get("chatgpt_plan_type")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut headers = vec![
        ("OpenAI-Beta".to_string(), "codex-1".to_string()),
        ("Originator".to_string(), "Codex Desktop".to_string()),
    ];
    if !account_id.is_empty() {
        headers.push(("Chatgpt-Account-Id".to_string(), account_id));
    }
    CredentialResult::Ok(CredentialOk {
        token: access_token,
        expires_at_ms: None,
        plan: plan_raw.map(|p| codex_plan_label(&p)),
        headers,
    })
}

// --- window mappers (pure, unit-tested) -----------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuotaWindowKind {
    Session,
    Weekly,
    Monthly,
    Other,
}

impl QuotaWindowKind {
    fn as_str(&self) -> &'static str {
        match self {
            QuotaWindowKind::Session => "session",
            QuotaWindowKind::Weekly => "weekly",
            QuotaWindowKind::Monthly => "monthly",
            QuotaWindowKind::Other => "other",
        }
    }
}

fn window_kind_for_duration(mins: Option<f64>) -> QuotaWindowKind {
    let Some(mins) = mins else {
        return QuotaWindowKind::Other;
    };
    if mins >= 43_200.0 {
        return QuotaWindowKind::Monthly; // >= 30 days
    }
    if mins >= 10_080.0 {
        return QuotaWindowKind::Weekly; // >= 7 days
    }
    QuotaWindowKind::Session
}

fn label_for_kind(kind: QuotaWindowKind) -> &'static str {
    match kind {
        QuotaWindowKind::Session => "Session",
        QuotaWindowKind::Weekly => "Weekly",
        QuotaWindowKind::Monthly => "Monthly",
        QuotaWindowKind::Other => "Quota",
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = true; // trim leading underscores
    for ch in value.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[derive(Debug, Default)]
pub struct MappedQuota {
    pub windows: Vec<QuotaWindow>,
    pub plan: Option<String>,
}

fn quota_window(
    id: &str,
    label: &str,
    kind: QuotaWindowKind,
    used_percent: f64,
    resets_at: Option<String>,
    window_duration_mins: Option<f64>,
) -> QuotaWindow {
    QuotaWindow {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.as_str().to_string(),
        used_percent: clamp_percent(used_percent),
        resets_at,
        window_duration_mins,
    }
}

/// `GET /api/oauth/usage` → windows. Percentages are already 0-100.
pub fn claude_usage_to_windows(body: &Value) -> MappedQuota {
    let mut out = MappedQuota::default();
    let specs: [(&str, &str, QuotaWindowKind, f64); 2] = [
        ("five_hour", "Session (5-hour)", QuotaWindowKind::Session, 300.0),
        ("seven_day", "Weekly", QuotaWindowKind::Weekly, 10_080.0),
    ];
    for (key, label, kind, mins) in specs {
        let Some(w) = body.get(key).and_then(|v| v.as_object()) else {
            continue;
        };
        let used = num_or_null(w.get("utilization"));
        let Some(used) = used else { continue };
        out.windows.push(quota_window(
            key,
            label,
            kind,
            used,
            iso_from_string(w.get("resets_at")),
            Some(mins),
        ));
    }
    // Model-scoped weekly allowances ship in `limits[]`.
    if let Some(limits) = body.get("limits").and_then(|v| v.as_array()) {
        for entry in limits {
            let Some(e) = entry.as_object() else { continue };
            if e.get("kind").and_then(|v| v.as_str()) != Some("weekly_scoped") {
                continue;
            }
            let model = e
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.as_object());
            let display_name = model
                .and_then(|m| m.get("display_name"))
                .and_then(|v| v.as_str());
            let Some(display_name) = display_name.filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(percent) = num_or_null(e.get("percent")) else {
                continue;
            };
            out.windows.push(quota_window(
                &format!("seven_day_{}", slugify(display_name)),
                &format!("Weekly · {}", display_name),
                QuotaWindowKind::Weekly,
                percent,
                iso_from_string(e.get("resets_at")),
                Some(10_080.0),
            ));
        }
    }
    // Extra-usage / spend caps are monthly quota windows when enabled.
    if let Some(extra) = body.get("extra_usage").and_then(|v| v.as_object()) {
        if extra.get("is_enabled").and_then(|v| v.as_bool()) == Some(true) {
            let mut used = num_or_null(extra.get("utilization"));
            if used.is_none() {
                let used_credits = num_or_null(extra.get("used_credits"));
                let limit = num_or_null(extra.get("monthly_limit"));
                if let (Some(c), Some(l)) = (used_credits, limit) {
                    if l > 0.0 {
                        used = Some((c / l) * 100.0);
                    }
                }
            }
            if let Some(used) = used {
                out.windows.push(quota_window(
                    "extra_usage",
                    "Extra usage",
                    QuotaWindowKind::Monthly,
                    used,
                    iso_from_string(extra.get("resets_at")),
                    Some(43_200.0),
                ));
            }
        }
    }
    if let Some(spend) = body.get("spend").and_then(|v| v.as_object()) {
        if spend.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
            let used_amt = spend.get("used").and_then(|v| v.as_object());
            let limit_amt = spend.get("limit").and_then(|v| v.as_object());
            let used_minor = used_amt.and_then(|u| num_or_null(u.get("amount_minor")));
            let limit_minor = limit_amt.and_then(|l| num_or_null(l.get("amount_minor")));
            let currency = used_amt
                .and_then(|u| u.get("currency"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let same_scale = currency.is_some()
                && used_amt
                    .and_then(|u| u.get("currency"))
                    .and_then(|v| v.as_str())
                    == limit_amt.and_then(|l| l.get("currency")).and_then(|v| v.as_str())
                && used_amt.and_then(|u| u.get("exponent"))
                    == limit_amt.and_then(|l| l.get("exponent"));
            if same_scale {
                if let (Some(u), Some(l)) = (used_minor, limit_minor) {
                    if l > 0.0 {
                        out.windows.push(quota_window(
                            "monthly_spend",
                            "Monthly spend",
                            QuotaWindowKind::Monthly,
                            (u / l) * 100.0,
                            None,
                            Some(43_200.0),
                        ));
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug)]
struct CodexWindowRaw {
    used_percent: f64,
    reset_at: Option<f64>,
    window_duration_mins: Option<f64>,
}

fn codex_window(raw: Option<&Value>, fallback_mins: f64) -> Option<CodexWindowRaw> {
    let w = raw?.as_object()?;
    let used = num_or_null(w.get("used_percent"))?;
    let limit_seconds = num_or_null(w.get("limit_window_seconds"));
    Some(CodexWindowRaw {
        used_percent: used,
        reset_at: num_or_null(w.get("reset_at")),
        window_duration_mins: limit_seconds.map(|s| s / 60.0).or(Some(fallback_mins)),
    })
}

/// `GET /backend-api/wham/usage` → windows (primary/secondary positions).
pub fn codex_usage_to_windows(body: &Value) -> MappedQuota {
    let mut out = MappedQuota::default();
    let rate_limit = body.get("rate_limit").and_then(|v| v.as_object());
    let plan_raw = body
        .get("plan_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    out.plan = plan_raw.as_deref().map(codex_plan_label);
    let is_monthly_plan = matches!(plan_raw.as_deref(), Some("free") | Some("go"));
    let positions: [(&str, Option<&Value>, f64); 2] = [
        (
            "primary",
            rate_limit.and_then(|r| r.get("primary_window")),
            if is_monthly_plan { 43_200.0 } else { 300.0 },
        ),
        (
            "secondary",
            rate_limit.and_then(|r| r.get("secondary_window")),
            10_080.0,
        ),
    ];
    for (id, raw, fallback) in positions {
        let Some(w) = codex_window(raw, fallback) else {
            continue;
        };
        let mins = w.window_duration_mins.unwrap_or(fallback);
        let kind = window_kind_for_duration(Some(mins));
        out.windows.push(quota_window(
            id,
            label_for_kind(kind),
            kind,
            w.used_percent,
            iso_from_epoch_seconds(w.reset_at),
            if mins.is_finite() {
                Some(mins.round())
            } else {
                None
            },
        ));
    }
    out
}

// --- HTTP -----------------------------------------------------------------------

/// Outcome of one HTTP request. `detail` is a sanitized failure
/// summary — never contains headers, tokens, or bodies.
#[derive(Debug)]
pub struct FetchOutcome {
    pub ok: bool,
    /// HTTP status when the request completed; None on transport failure.
    pub status: Option<u16>,
    pub json: Option<Value>,
    pub detail: Option<String>,
}

/// GET/POST a URL expecting a JSON document. `headers` are applied
/// verbatim (callers own credential hygiene). `body` switches to POST.
pub fn fetch_json(
    url: &str,
    headers: &[(String, String)],
    method: &str,
    body: Option<&str>,
    timeout_ms: u64,
) -> FetchOutcome {
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();

    let result: Result<ureq::Response, ureq::Error> = match method {
        "POST" => {
            let mut req = agent.post(url);
            for (k, v) in headers {
                req = req.set(k, v);
            }
            match body {
                Some(b) => req.send_string(b),
                None => req.call(),
            }
        }
        _ => {
            let mut req = agent.get(url);
            for (k, v) in headers {
                req = req.set(k, v);
            }
            req.call()
        }
    };

    let response = match result {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _resp)) => {
            return FetchOutcome {
                ok: false,
                status: Some(code),
                json: None,
                detail: Some(format!("HTTP {}", code)),
            };
        }
        Err(e) => {
            let message = e.to_string();
            let clipped: String = message.chars().take(200).collect();
            return FetchOutcome {
                ok: false,
                status: None,
                json: None,
                detail: Some(format!("request failed: {}", clipped)),
            };
        }
    };

    let status = response.status();
    let text = match response.into_string() {
        Ok(t) => t,
        Err(e) => {
            return FetchOutcome {
                ok: false,
                status: Some(status),
                json: None,
                detail: Some(format!("response read failed: {}", e)),
            };
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(json) => FetchOutcome {
            ok: true,
            status: Some(status),
            json: Some(json),
            detail: None,
        },
        Err(_) => FetchOutcome {
            ok: false,
            status: Some(status),
            json: None,
            detail: Some("response was not JSON".to_string()),
        },
    }
}

// --- cache -----------------------------------------------------------------------

/// Serializable quota cache payload (`quota-cache/<provider>.json`).
/// `windows` stays `Vec<Value>` so cached rows are served verbatim —
/// the same pass-through semantics as the TS `QuotaWindow[]` cast.
#[derive(Debug, Clone)]
pub struct CachePayload {
    pub checked_at: String,
    pub plan: Option<String>,
    pub windows: Vec<Value>,
    pub reset_credits_available: Option<f64>,
}

fn quota_cache_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("quota-cache")
}

fn quota_cache_file(state_dir: &Path, provider: &str) -> PathBuf {
    quota_cache_dir(state_dir).join(format!("{}.json", provider))
}

struct CacheRead {
    payload: CachePayload,
    age_seconds: i64,
}

fn read_cache(state_dir: &Path, provider: &str) -> Option<CacheRead> {
    let text = fs::read_to_string(quota_cache_file(state_dir, provider)).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let obj = parsed.as_object()?;
    let checked_at = obj.get("checked_at").and_then(|v| v.as_str())?;
    let windows = obj.get("windows").and_then(|v| v.as_array())?.clone();
    let checked_ms = crate::cli::parse_js_date(checked_at)?;
    let age_ms = now_ms() - checked_ms;
    if !age_ms.is_finite() || age_ms < 0.0 {
        return None;
    }
    // Same leniency as TS readCache: only top-level shape is checked;
    // plan/reset fields coerce to null when not the expected type.
    let payload = CachePayload {
        checked_at: checked_at.to_string(),
        plan: obj
            .get("plan")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        windows,
        reset_credits_available: obj
            .get("reset_credits_available")
            .and_then(|v| v.as_f64())
            .filter(|f| f.is_finite()),
    };
    Some(CacheRead {
        payload,
        age_seconds: (age_ms / 1000.0).floor() as i64,
    })
}

fn write_cache(state_dir: &Path, provider: &str, payload: &CachePayload) {
    let _ = (|| -> std::io::Result<()> {
        let dir = quota_cache_dir(state_dir);
        fs::create_dir_all(&dir)?;
        let file = quota_cache_file(state_dir, provider);
        // TS writes `${file}.tmp-${pid}` then renames — same shape.
        let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
        let body = serde_json::json!({
            "checked_at": payload.checked_at,
            "plan": payload.plan,
            "windows": payload.windows,
            "reset_credits_available": payload.reset_credits_available,
        });
        fs::write(&tmp, format!("{}\n", serde_json::to_string(&body).unwrap_or_default()))?;
        fs::rename(&tmp, &file)?;
        Ok(())
    })();
    // cache is best-effort; a write failure must not fail the report
}

// --- endpoints --------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ResolvedQuotaEndpoints {
    pub claude: String,
    pub codex_usage: String,
    /// None when the codex usage endpoint is overridden without an
    /// explicit credits endpoint — a mock/proxy must opt in.
    pub codex_credits: Option<String>,
}

pub fn resolve_endpoints(overrides: Option<&QuotaEndpoints>) -> ResolvedQuotaEndpoints {
    let claude = overrides
        .and_then(|o| o.claude.clone())
        .or_else(|| env_get("HERDR_USAGE_QUOTA_CLAUDE_URL"))
        .unwrap_or_else(|| CLAUDE_DEFAULT_ENDPOINT.to_string());
    let codex_usage = overrides
        .and_then(|o| o.codex_usage.clone())
        .or_else(|| env_get("HERDR_USAGE_QUOTA_CODEX_USAGE_URL"))
        .unwrap_or_else(|| CODEX_DEFAULT_USAGE_ENDPOINT.to_string());
    let codex_credits = overrides
        .and_then(|o| o.codex_credits.clone())
        .or_else(|| env_get("HERDR_USAGE_QUOTA_CODEX_CREDITS_URL"))
        .or_else(|| {
            if codex_usage == CODEX_DEFAULT_USAGE_ENDPOINT {
                Some(CODEX_DEFAULT_CREDITS_ENDPOINT.to_string())
            } else {
                None
            }
        });
    ResolvedQuotaEndpoints {
        claude,
        codex_usage,
        codex_credits,
    }
}

// --- per-provider probe ------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct QuotaOptions {
    /// Provider ids to report; None = all registered providers.
    pub providers: Option<Vec<String>>,
    /// Bypass the fresh-cache read (still writes on success).
    pub refresh: bool,
    pub timeout_ms: Option<u64>,
    /// Plugin state dir override (tests); default HERDR_PLUGIN_STATE_DIR layout.
    pub state_dir: Option<PathBuf>,
    pub endpoints: Option<QuotaEndpoints>,
    /// Cache TTL override in seconds (config `quotaCacheTtlSeconds`).
    pub cache_ttl_seconds: Option<i64>,
}

fn base_result(provider: &str, fetched_at: &str, ttl: i64) -> Value {
    json!({
        "provider": provider,
        "status": "request_failed",
        "detail": Value::Null,
        "plan": Value::Null,
        "windows": [],
        "checked_at": Value::Null,
        "fetched_at": fetched_at,
        "stale": false,
        "stale_reason": Value::Null,
        "reset_credits_available": Value::Null,
        "cache": { "hit": false, "age_seconds": Value::Null, "ttl_seconds": ttl },
    })
}

fn cached_result(
    provider: &str,
    fetched_at: &str,
    ttl: i64,
    cache: &CacheRead,
    stale: bool,
    stale_reason: Option<String>,
) -> Value {
    json!({
        "provider": provider,
        "status": "ok",
        "detail": Value::Null,
        "plan": cache.payload.plan.clone(),
        "windows": cache.payload.windows.clone(),
        "checked_at": cache.payload.checked_at.clone(),
        "fetched_at": fetched_at,
        "stale": stale,
        "stale_reason": stale_reason,
        "reset_credits_available": cache.payload.reset_credits_available,
        "cache": { "hit": true, "age_seconds": cache.age_seconds, "ttl_seconds": ttl },
    })
}

fn fetch_claude_quota(
    cred: &CredentialOk,
    endpoint: &str,
    timeout_ms: u64,
) -> Result<CachePayload, String> {
    let headers = vec![
        ("authorization".to_string(), format!("Bearer {}", cred.token)),
        ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
        (
            "user-agent".to_string(),
            "claude-cli/2.1.0 (external, cli)".to_string(),
        ),
    ];
    let res = fetch_json(endpoint, &headers, "GET", None, timeout_ms);
    if !res.ok {
        return Err(res.detail.unwrap_or_else(|| "request failed".to_string()));
    }
    let body = res.json.unwrap_or(Value::Null);
    let mapped = claude_usage_to_windows(&body);
    if mapped.windows.is_empty() {
        return Err("quota endpoint returned no usage windows".to_string());
    }
    Ok(CachePayload {
        checked_at: iso_from_ms(now_ms()).unwrap_or_default(),
        plan: cred.plan.clone().or(mapped.plan),
        windows: mapped.windows.iter().map(|w| w.to_value()).collect(),
        reset_credits_available: None,
    })
}

/// Count banked Codex reset credits from a `rate-limit-reset-credits`
/// response body: `reset_type === "codex_rate_limits"`, `status ===
/// "available"`, and not yet expired. Pure — shared by the direct
/// quota probe and the hub (api-call) path. Returns None when the
/// body is not the expected shape (missing is never zero-fabricated).
pub fn codex_reset_credits_available(body: &Value, now_ms: f64) -> Option<f64> {
    let credits = body.get("credits").and_then(|v| v.as_array())?;
    let mut count = 0u64;
    for entry in credits {
        let Some(c) = entry.as_object() else { continue };
        if c.get("reset_type").and_then(|v| v.as_str()) != Some("codex_rate_limits")
            || c.get("status").and_then(|v| v.as_str()) != Some("available")
        {
            continue;
        }
        let expires = c
            .get("expires_at")
            .and_then(|v| v.as_str())
            .and_then(crate::cli::parse_js_date);
        match expires {
            Some(ms) if ms.is_finite() && ms > now_ms => count += 1,
            _ => continue,
        }
    }
    Some(count as f64)
}

fn fetch_codex_credits(
    cred: &CredentialOk,
    endpoint: &str,
    timeout_ms: u64,
) -> Option<f64> {
    let mut headers = vec![(
        "authorization".to_string(),
        format!("Bearer {}", cred.token),
    )];
    headers.extend(cred.headers.iter().cloned());
    let res = fetch_json(endpoint, &headers, "GET", None, timeout_ms);
    if !res.ok {
        return None;
    }
    codex_reset_credits_available(&res.json?, now_ms())
}

fn fetch_codex_quota(
    cred: &CredentialOk,
    endpoints: &ResolvedQuotaEndpoints,
    timeout_ms: u64,
) -> Result<CachePayload, String> {
    let mut headers = vec![(
        "authorization".to_string(),
        format!("Bearer {}", cred.token),
    )];
    headers.extend(cred.headers.iter().cloned());
    let res = fetch_json(&endpoints.codex_usage, &headers, "GET", None, timeout_ms);
    if !res.ok {
        return Err(res.detail.unwrap_or_else(|| "request failed".to_string()));
    }
    let body = res.json.unwrap_or(Value::Null);
    let mapped = codex_usage_to_windows(&body);
    if mapped.windows.is_empty() {
        return Err("quota endpoint returned no rate-limit windows".to_string());
    }
    let credits = endpoints
        .codex_credits
        .as_deref()
        .and_then(|url| fetch_codex_credits(cred, url, timeout_ms));
    Ok(CachePayload {
        checked_at: iso_from_ms(now_ms()).unwrap_or_default(),
        plan: cred.plan.clone().or(mapped.plan),
        windows: mapped.windows.iter().map(|w| w.to_value()).collect(),
        reset_credits_available: credits,
    })
}

fn quota_for_provider(
    spec: &QuotaProviderSpec,
    options: &QuotaOptions,
    endpoints: &ResolvedQuotaEndpoints,
    state_dir: &Path,
    ttl: i64,
    timeout_ms: u64,
) -> Value {
    let fetched_at = iso_from_ms(now_ms()).unwrap_or_default();
    let mut result = base_result(&spec.id, &fetched_at, ttl);

    if !spec.implemented {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "status".to_string(),
                Value::String("unsupported".to_string()),
            );
            obj.insert(
                "detail".to_string(),
                Value::String(
                    spec.unsupported_reason
                        .clone()
                        .unwrap_or_else(|| "unsupported".to_string()),
                ),
            );
        }
        return result;
    }

    let cred = match spec.id.as_str() {
        "claude" => read_claude_credential(),
        "codex" => read_codex_credential(),
        _ => CredentialResult::Err {
            status: CredentialFailure::Unsupported,
            detail: "no credential reader".to_string(),
        },
    };
    let cred = match cred {
        CredentialResult::Ok(c) => c,
        CredentialResult::Err { status, detail } => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("status".to_string(), Value::String(status.as_str().to_string()));
                obj.insert("detail".to_string(), Value::String(detail));
            }
            return result;
        }
    };

    let cached = read_cache(state_dir, &spec.id);
    // Retention bound (T4 / #93): a cache entry older than the shared
    // retention window is never served, even as stale — GC normally
    // removes it first, this guard covers a failed/skipped GC.
    let usable = cached.filter(|c| c.age_seconds < PLUGIN_CACHE_RETENTION_SECONDS);
    let fresh = usable
        .as_ref()
        .map(|c| c.age_seconds < ttl)
        .unwrap_or(false);
    if !options.refresh && fresh {
        // fresh ⇒ usable.is_some()
        if let Some(usable) = usable.as_ref() {
            return cached_result(&spec.id, &fetched_at, ttl, usable, false, None);
        }
    }

    let fetched = match spec.id.as_str() {
        "claude" => fetch_claude_quota(&cred, &endpoints.claude, timeout_ms),
        _ => fetch_codex_quota(&cred, endpoints, timeout_ms),
    };

    match fetched {
        Ok(payload) => {
            write_cache(state_dir, &spec.id, &payload);
            if let Some(obj) = result.as_object_mut() {
                obj.insert("status".to_string(), Value::String("ok".to_string()));
                obj.insert("plan".to_string(), payload.plan.clone().map(Value::String).unwrap_or(Value::Null));
                obj.insert(
                    "windows".to_string(),
                    Value::Array(payload.windows.clone()),
                );
                obj.insert(
                    "checked_at".to_string(),
                    Value::String(payload.checked_at.clone()),
                );
                obj.insert(
                    "reset_credits_available".to_string(),
                    payload
                        .reset_credits_available
                        .map(Value::from)
                        .unwrap_or(Value::Null),
                );
                obj.insert(
                    "cache".to_string(),
                    json!({ "hit": false, "age_seconds": Value::Null, "ttl_seconds": ttl }),
                );
            }
            result
        }
        Err(detail) => {
            // Endpoint failed: serve the last good reading, explicitly
            // stale, rather than reporting a fabricated fresh zero —
            // within retention.
            if let Some(usable) = usable {
                return cached_result(
                    &spec.id,
                    &fetched_at,
                    ttl,
                    &usable,
                    true,
                    Some(format!(
                        "fetch failed ({}); serving cached reading",
                        detail
                    )),
                );
            }
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "status".to_string(),
                    Value::String("request_failed".to_string()),
                );
                obj.insert("detail".to_string(), Value::String(detail));
            }
            result
        }
    }
}

/// Collect quota reports for the requested providers. Never throws
/// for per-provider failures — every outcome is a structured status.
pub fn collect_quota(options: &QuotaOptions) -> Vec<Value> {
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let ttl = options.cache_ttl_seconds.unwrap_or(QUOTA_CACHE_TTL_SECONDS);
    let state_dir = options
        .state_dir
        .clone()
        .unwrap_or_else(|| plugin_dirs(PLUGIN_ID, None, None).state_dir);
    let endpoints = resolve_endpoints(options.endpoints.as_ref());

    // Retention GC (T4 / #93): drop cache entries past the shared
    // retention window before reading. Best-effort — never fails.
    gc_plugin_state_cache(&state_dir, None, None);

    let specs = quota_providers();
    let requested: Vec<String> = options
        .providers
        .clone()
        .unwrap_or_else(|| specs.iter().map(|p| p.id.clone()).collect());
    let mut results = Vec::new();
    for id in requested {
        let spec = specs
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .unwrap_or(QuotaProviderSpec {
                id: id.clone(),
                implemented: false,
                unsupported_reason: Some(
                    "no quota source registered for this provider".to_string(),
                ),
            });
        results.push(quota_for_provider(
            &spec, options, &endpoints, &state_dir, ttl, timeout_ms,
        ));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_body() -> Value {
        json!({
            "five_hour": {"utilization": 42.5, "resets_at": "2026-01-01T05:00:00.000Z"},
            "seven_day": {"utilization": 10, "resets_at": "2026-01-08T00:00:00.000Z"},
            "limits": [
                {
                    "kind": "weekly_scoped",
                    "percent": 55,
                    "resets_at": "2026-01-08T00:00:00.000Z",
                    "scope": {"model": {"display_name": "Fable"}},
                },
            ],
            "extra_usage": {"is_enabled": true, "utilization": 3},
        })
    }

    fn codex_body() -> Value {
        json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.5,
                    "reset_at": 1_767_225_600,
                    "limit_window_seconds": 18_000,
                },
                "secondary_window": {"used_percent": 30, "reset_at": 1_767_657_600},
            },
        })
    }

    #[test]
    fn claude_usage_maps_all_window_kinds() {
        let mapped = claude_usage_to_windows(&claude_body());
        assert!(mapped.plan.is_none()); // plan comes from the credential store
        let by_id = |id: &str| {
            mapped
                .windows
                .iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("missing window {}", id))
        };
        assert_eq!(by_id("five_hour").kind, "session");
        assert_eq!(by_id("five_hour").used_percent, 42.5);
        assert_eq!(by_id("five_hour").window_duration_mins, Some(300.0));
        assert_eq!(by_id("seven_day").kind, "weekly");
        assert_eq!(by_id("seven_day_fable").label, "Weekly · Fable");
        assert_eq!(by_id("extra_usage").kind, "monthly");
        assert_eq!(by_id("extra_usage").used_percent, 3.0);
    }

    #[test]
    fn codex_usage_maps_primary_secondary_with_duration_kinds() {
        let mapped = codex_usage_to_windows(&codex_body());
        assert_eq!(mapped.plan.as_deref(), Some("Pro"));
        let by_id = |id: &str| {
            mapped
                .windows
                .iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("missing window {}", id))
        };
        assert_eq!(by_id("primary").kind, "session"); // 18000s = 300min
        assert_eq!(by_id("primary").used_percent, 12.5);
        assert!(by_id("primary").resets_at.is_some());
        assert_eq!(by_id("secondary").kind, "weekly"); // fallback 10080
    }

    #[test]
    fn malformed_bodies_yield_no_windows_never_fabricated() {
        assert!(claude_usage_to_windows(&Value::Null).windows.is_empty());
        assert!(claude_usage_to_windows(&json!({"five_hour": {"utilization": "x"}}))
            .windows
            .is_empty());
        assert!(codex_usage_to_windows(&json!({"rate_limit": null}))
            .windows
            .is_empty());
        assert!(codex_usage_to_windows(&json!({})).plan.is_none());
    }

    #[test]
    fn codex_reset_credits_counts_only_available_unexpired() {
        let body = json!({
            "credits": [
                {"id": "c1", "status": "available", "reset_type": "codex_rate_limits", "expires_at": "2999-01-01T00:00:00.000Z"},
                {"id": "c2", "status": "available", "reset_type": "codex_rate_limits", "expires_at": "2000-01-01T00:00:00.000Z"},
                {"id": "c3", "status": "redeemed", "reset_type": "codex_rate_limits", "expires_at": "2999-01-01T00:00:00.000Z"},
            ],
        });
        let now = crate::cli::parse_js_date("2026-01-01T00:00:00.000Z").unwrap();
        assert_eq!(codex_reset_credits_available(&body, now), Some(1.0));
        assert!(codex_reset_credits_available(&json!({}), now).is_none());
    }

    #[test]
    fn decode_jwt_claims_reads_payload_segment() {
        let enc = |s: &str| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
        };
        let token = format!(
            "{}.{}.sig",
            enc(r#"{"alg":"none"}"#),
            enc(r#"{"chatgpt_account_id":"acct-1","plan_type":"pro"}"#)
        );
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(
            claims.get("chatgpt_account_id").and_then(|v| v.as_str()),
            Some("acct-1")
        );
        assert!(decode_jwt_claims("not-a-jwt").is_none());
    }
}
