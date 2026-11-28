//! HTTP quota hub source for rebuildup.usage (Issue #92 / parent #28
//! T3). Rust port of `plugins/usage/src/hub.ts` (retired; git history
//! preserves it).
//!
//! CLIProxyAPI-equivalent integration: a user-configured hub exposes
//! the CLIProxyAPI management API (`GET /v0/management/auth-files`,
//! `POST /v0/management/api-call`). The hub holds provider credentials;
//! this plugin only holds the hub *management key* (ADR-0015) and asks
//! the hub to call each account's provider quota endpoint with the
//! `$TOKEN$` placeholder — real provider tokens never reach this
//! process.
//!
//! Boundary rules (ADR-0015, extending ADR-0014):
//! - The management key comes from an env var named by config
//!   (`keyEnv`); it is sent only as `Authorization: Bearer` to the
//!   configured hub origin. It is never logged, persisted, cached,
//!   emitted, or placed in error details.
//! - Hub responses are treated as untrusted: only whitelisted fields
//!   (account id/provider, quota windows, plan labels) reach the
//!   output; error details carry HTTP status or transport summaries
//!   only.
//! - Successful readings cache quota numbers under the plugin state dir
//!   (`quota-cache/hub-<id>.json`) for the shared TTL; stale cache is
//!   served flagged when the hub cannot be reached. Cache holds no
//!   credential material.

use std::fs;
use std::path::{Path, PathBuf};

use hoxay_herdr_client::dirs::plugin_dirs;
use serde_json::{json, Value};
use url::Url;

use crate::model::QuotaHubConfig;
use crate::quota::{
    claude_usage_to_windows, codex_reset_credits_available, codex_usage_to_windows,
    decode_jwt_claims, fetch_json, iso_from_ms, now_ms, MappedQuota,
};
use crate::state::{gc_plugin_state_cache, PLUGIN_CACHE_RETENTION_SECONDS};

const PLUGIN_ID: &str = "rebuildup.usage";

/// Env var consulted when a hub config does not name `keyEnv`.
pub const DEFAULT_HUB_KEY_ENV: &str = "HERDR_USAGE_QUOTA_HUB_KEY";

const CLAUDE_QUOTA_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";

// --- endpoint handling ---------------------------------------------------------

/// Normalized hub URL: `base` ends in "/" (the CLIProxyAPI convention:
/// users often paste the panel URL ending in `management.html`);
/// `endpoint` is origin+path with query/hash dropped and carries no
/// credentials. None for non-http(s) or unparseable input.
pub fn hub_base_url(raw: &str) -> Option<(String, String)> {
    let mut url = Url::parse(raw).ok()?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    // Strip a trailing /management.html (case-insensitive) and any
    // trailing slashes, then re-append exactly one "/".
    let path = url.path().to_string();
    let lowered = path.to_lowercase();
    let trimmed = if lowered.ends_with("/management.html") {
        &path[..path.len() - "/management.html".len()]
    } else {
        path.trim_end_matches('/')
    };
    let new_path = if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("{}/", trimmed)
    };
    url.set_path(&new_path);
    url.set_query(None);
    url.set_fragment(None);
    let endpoint = format!("{}{}", url.origin().ascii_serialization(), url.path());
    Some((url.to_string(), endpoint))
}

fn management_url(base: &str, path_: &str) -> String {
    // new URL(`v0/management/${path_}`, base) — base ends in "/".
    format!("{}v0/management/{}", base, path_)
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = true;
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

// --- provider specs --------------------------------------------------------------

struct HubProviderSpec {
    /// auth-files `provider` value this spec answers.
    provider: &'static str,
    usage_url: &'static str,
    /// Second upstream GET (codex reset credits); best-effort.
    credits_url: Option<&'static str>,
    headers: fn(&Value) -> Vec<(String, String)>,
    /// Fallback plan label when the upstream body does not report one.
    plan_fallback: Option<&'static str>,
}

fn codex_headers(account: &Value) -> Vec<(String, String)> {
    let mut headers = vec![
        // The hub substitutes the account's real token for this placeholder.
        ("authorization".to_string(), "Bearer $TOKEN$".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("OpenAI-Beta".to_string(), "codex-1".to_string()),
        ("Originator".to_string(), "Codex Desktop".to_string()),
    ];
    let id_token = account.get("id_token").and_then(|v| v.as_object());
    // Whitelisted claim only — the token itself is never read.
    let account_id = id_token
        .and_then(|t| t.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // auth-files carries parsed claims on some hub versions; fall back
    // to decoding the JWT payload (unverified, claims only).
    let claims = if !account_id.is_empty() {
        Some(json!({ "chatgpt_account_id": account_id }))
    } else {
        id_token
            .and_then(|t| t.get("raw"))
            .and_then(|v| v.as_str())
            .and_then(decode_jwt_claims)
    };
    let resolved = claims
        .as_ref()
        .and_then(|c| c.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !resolved.is_empty() {
        headers.push(("Chatgpt-Account-Id".to_string(), resolved));
    }
    headers
}

fn claude_headers(_account: &Value) -> Vec<(String, String)> {
    vec![
        ("authorization".to_string(), "Bearer $TOKEN$".to_string()),
        ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
        (
            "user-agent".to_string(),
            "claude-cli/2.1.0 (external, cli)".to_string(),
        ),
    ]
}

const HUB_PROVIDERS: &[HubProviderSpec] = &[
    HubProviderSpec {
        provider: "claude",
        usage_url: CLAUDE_QUOTA_URL,
        credits_url: None,
        headers: claude_headers,
        plan_fallback: Some("Claude Subscription"),
    },
    HubProviderSpec {
        provider: "codex",
        usage_url: CODEX_USAGE_URL,
        credits_url: Some(CODEX_CREDITS_URL),
        headers: codex_headers,
        plan_fallback: None,
    },
];

fn spec_for(provider: &str) -> Option<&'static HubProviderSpec> {
    HUB_PROVIDERS.iter().find(|s| s.provider == provider)
}

// --- hub cache -------------------------------------------------------------------

/// Serializable hub cache payload (`quota-cache/hub-<id>.json`).
/// `accounts[].windows` stays `Vec<Value>` for verbatim pass-through.
#[derive(Debug, Clone)]
pub struct HubCachePayload {
    pub checked_at: String,
    pub accounts: Vec<HubCacheAccount>,
}

#[derive(Debug, Clone)]
pub struct HubCacheAccount {
    pub account: String,
    pub provider: String,
    pub plan: Option<String>,
    pub windows: Vec<Value>,
    pub reset_credits_available: Option<f64>,
}

fn hub_cache_file(state_dir: &Path, hub_id: &str) -> PathBuf {
    let slug = slugify(hub_id);
    state_dir
        .join("quota-cache")
        .join(format!("hub-{}.json", if slug.is_empty() { "hub" } else { &slug }))
}

struct HubCacheRead {
    payload: HubCachePayload,
    age_seconds: i64,
}

fn read_hub_cache(state_dir: &Path, hub_id: &str) -> Option<HubCacheRead> {
    let text = fs::read_to_string(hub_cache_file(state_dir, hub_id)).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let obj = parsed.as_object()?;
    let checked_at = obj.get("checked_at").and_then(|v| v.as_str())?;
    let accounts_v = obj.get("accounts").and_then(|v| v.as_array())?;
    let checked_ms = crate::cli::parse_js_date(checked_at)?;
    let age_ms = now_ms() - checked_ms;
    if !age_ms.is_finite() || age_ms < 0.0 {
        return None;
    }
    let mut accounts = Vec::new();
    for a in accounts_v {
        let Some(a) = a.as_object() else { continue };
        let account = a.get("account").and_then(|v| v.as_str());
        let provider = a.get("provider").and_then(|v| v.as_str());
        let (Some(account), Some(provider)) = (account, provider) else {
            continue;
        };
        accounts.push(HubCacheAccount {
            account: account.to_string(),
            provider: provider.to_string(),
            plan: a.get("plan").and_then(|v| v.as_str()).map(|s| s.to_string()),
            windows: a
                .get("windows")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            reset_credits_available: a
                .get("reset_credits_available")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite()),
        });
    }
    Some(HubCacheRead {
        payload: HubCachePayload {
            checked_at: checked_at.to_string(),
            accounts,
        },
        age_seconds: (age_ms / 1000.0).floor() as i64,
    })
}

fn write_hub_cache(state_dir: &Path, hub_id: &str, payload: &HubCachePayload) {
    let _ = (|| -> std::io::Result<()> {
        let dir = state_dir.join("quota-cache");
        fs::create_dir_all(&dir)?;
        let file = hub_cache_file(state_dir, hub_id);
        let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
        let accounts: Vec<Value> = payload
            .accounts
            .iter()
            .map(|a| {
                json!({
                    "account": a.account,
                    "provider": a.provider,
                    "plan": a.plan,
                    "windows": a.windows,
                    "reset_credits_available": a.reset_credits_available,
                })
            })
            .collect();
        let body = json!({
            "checked_at": payload.checked_at,
            "accounts": accounts,
        });
        fs::write(&tmp, format!("{}\n", serde_json::to_string(&body).unwrap_or_default()))?;
        fs::rename(&tmp, &file)?;
        Ok(())
    })();
    // cache is best-effort; a write failure must not fail the report
}

// --- api-call ---------------------------------------------------------------------

struct ApiCallOutcome {
    ok: bool,
    /// Upstream HTTP status when the hub relayed one; None on transport
    /// failure. Carried for parity with the TS shape; never read.
    #[allow(dead_code)]
    status_code: Option<u16>,
    /// Parsed upstream JSON body (the `body` string field, JSON-decoded).
    body: Option<Value>,
    /// Sanitized failure detail — never contains keys, tokens, or bodies.
    detail: Option<String>,
}

/// `POST /v0/management/api-call` — ask the hub to call `url` as the
/// account, with `header` (the `$TOKEN$` placeholder is substituted
/// hub-side). Response envelope: `{status_code, body}` where `body` is
/// a JSON-encoded string.
fn hub_api_call(
    base: &str,
    key: &str,
    auth_index: &str,
    url: &str,
    header: &[(String, String)],
    timeout_ms: u64,
) -> ApiCallOutcome {
    let mut headers = vec![
        ("authorization".to_string(), format!("Bearer {}", key)),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    headers.extend(header.iter().cloned());
    let request_body = serde_json::json!({
        "auth_index": auth_index,
        "method": "GET",
        "url": url,
        "header": header.iter().cloned().collect::<std::collections::HashMap<_,_>>(),
    })
    .to_string();
    let res = fetch_json(
        &management_url(base, "api-call"),
        &headers,
        "POST",
        Some(&request_body),
        timeout_ms,
    );
    if !res.ok {
        return ApiCallOutcome {
            ok: false,
            status_code: res.status,
            body: None,
            detail: res.detail.or_else(|| Some("request failed".to_string())),
        };
    }
    let envelope = res.json.unwrap_or(Value::Null);
    let status_code = envelope
        .get("status_code")
        .and_then(|v| v.as_u64())
        .map(|c| c as u16);
    let body_raw = envelope.get("body").and_then(|v| v.as_str());
    let (Some(status_code), Some(body_raw)) = (status_code, body_raw) else {
        return ApiCallOutcome {
            ok: false,
            status_code: None,
            body: None,
            detail: Some("api-call response was not the expected shape".to_string()),
        };
    };
    if !(200..300).contains(&status_code) {
        return ApiCallOutcome {
            ok: false,
            status_code: Some(status_code),
            body: None,
            detail: Some(format!(
                "provider refused the hub request (HTTP {})",
                status_code
            )),
        };
    }
    match serde_json::from_str::<Value>(body_raw) {
        Ok(body) => ApiCallOutcome {
            ok: true,
            status_code: Some(status_code),
            body: Some(body),
            detail: None,
        },
        Err(_) => ApiCallOutcome {
            ok: false,
            status_code: Some(status_code),
            body: None,
            detail: Some("provider response body was not JSON".to_string()),
        },
    }
}

// --- per-account probe --------------------------------------------------------------

struct AccountReading {
    status: &'static str,
    detail: Option<String>,
    plan: Option<String>,
    windows: Vec<Value>,
    checked_at: Option<String>,
    reset_credits_available: Option<f64>,
}

fn fail_reading(detail: &str) -> AccountReading {
    AccountReading {
        status: "request_failed",
        detail: Some(detail.to_string()),
        plan: None,
        windows: Vec::new(),
        checked_at: None,
        reset_credits_available: None,
    }
}

fn read_hub_account(
    base: &str,
    key: &str,
    account: &Value,
    spec: &'static HubProviderSpec,
    timeout_ms: u64,
    now_ms: f64,
) -> AccountReading {
    let auth_index = account
        .get("auth_index")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if auth_index.is_empty() {
        return fail_reading("auth file has no auth_index");
    }

    let call = hub_api_call(
        base,
        key,
        auth_index,
        spec.usage_url,
        &(spec.headers)(account),
        timeout_ms,
    );
    if !call.ok {
        return fail_reading(&call.detail.unwrap_or_else(|| "request failed".to_string()));
    }

    let mapped: MappedQuota = match spec.provider {
        "claude" => claude_usage_to_windows(&call.body.clone().unwrap_or(Value::Null)),
        _ => codex_usage_to_windows(&call.body.clone().unwrap_or(Value::Null)),
    };
    if mapped.windows.is_empty() {
        return fail_reading("provider reported no usage windows");
    }
    let mut credits: Option<f64> = None;
    if let Some(credits_url) = spec.credits_url {
        let credit_call = hub_api_call(
            base,
            key,
            auth_index,
            credits_url,
            &(spec.headers)(account),
            timeout_ms,
        );
        if credit_call.ok {
            credits = credit_call
                .body
                .as_ref()
                .and_then(|b| codex_reset_credits_available(b, now_ms));
        }
    }
    AccountReading {
        status: "ok",
        detail: None,
        plan: mapped.plan.clone().or_else(|| spec.plan_fallback.map(|s| s.to_string())),
        windows: mapped.windows.iter().map(|w| w.to_value()).collect(),
        checked_at: iso_from_ms(now_ms),
        reset_credits_available: credits,
    }
}

// --- hub probe -----------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct HubOptions {
    pub hubs: Option<Vec<QuotaHubConfig>>,
    /// Bypass the fresh-cache read (still writes on success).
    pub refresh: bool,
    pub timeout_ms: Option<u64>,
    pub state_dir: Option<PathBuf>,
    pub cache_ttl_seconds: Option<i64>,
}

fn base_hub_result(
    config: &QuotaHubConfig,
    endpoint: Option<&str>,
    fetched_at: &str,
    ttl: i64,
) -> Value {
    json!({
        "id": config.id,
        "kind": "cliproxy-api",
        "endpoint": endpoint,
        "status": "request_failed",
        "detail": Value::Null,
        "checked_at": Value::Null,
        "fetched_at": fetched_at,
        "stale": false,
        "stale_reason": Value::Null,
        "cache": { "hit": false, "age_seconds": Value::Null, "ttl_seconds": ttl },
        "accounts": [],
    })
}

fn cached_hub_result(
    config: &QuotaHubConfig,
    endpoint: Option<&str>,
    fetched_at: &str,
    ttl: i64,
    cache: &HubCacheRead,
    stale: bool,
    stale_reason: Option<String>,
) -> Value {
    let accounts: Vec<Value> = cache
        .payload
        .accounts
        .iter()
        .map(|a| {
            json!({
                "account": a.account,
                "provider": a.provider,
                "status": "ok",
                "detail": Value::Null,
                "plan": a.plan,
                "windows": a.windows,
                "checked_at": cache.payload.checked_at,
                "reset_credits_available": a.reset_credits_available,
            })
        })
        .collect();
    json!({
        "id": config.id,
        "kind": "cliproxy-api",
        "endpoint": endpoint,
        "status": "ok",
        "detail": Value::Null,
        "checked_at": cache.payload.checked_at,
        "fetched_at": fetched_at,
        "stale": stale,
        "stale_reason": stale_reason,
        "cache": { "hit": true, "age_seconds": cache.age_seconds, "ttl_seconds": ttl },
        "accounts": accounts,
    })
}

fn env_get(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn quota_for_hub(
    config: &QuotaHubConfig,
    options: &HubOptions,
    state_dir: &Path,
    ttl: i64,
    timeout_ms: u64,
) -> Value {
    let now_ms = now_ms();
    let fetched_at = iso_from_ms(now_ms).unwrap_or_default();

    let normalized = hub_base_url(&config.url);
    let endpoint = normalized.as_ref().map(|(_, e)| e.clone());
    let mut result = base_hub_result(config, endpoint.as_deref(), &fetched_at, ttl);
    let Some((base, _)) = normalized else {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "detail".to_string(),
                Value::String("hub URL is not a valid http(s) URL".to_string()),
            );
        }
        return result;
    };

    // `keyEnv: ""` is preserved verbatim (TS `??` semantics): only an
    // absent key_env falls back to the default env name.
    let key_env = config
        .key_env
        .clone()
        .unwrap_or_else(|| DEFAULT_HUB_KEY_ENV.to_string());
    let key = env_get(&key_env).unwrap_or_default();
    if key.is_empty() {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("status".to_string(), Value::String("no_credential".to_string()));
            obj.insert(
                "detail".to_string(),
                Value::String(format!("hub management key env {} is not set", key_env)),
            );
        }
        return result;
    }

    let cached = read_hub_cache(state_dir, &config.id);
    // Retention bound (T4 / #93): entries past the shared retention
    // window are never served, even as stale (see quota.rs).
    let usable = cached.filter(|c| c.age_seconds < PLUGIN_CACHE_RETENTION_SECONDS);
    let fresh = usable
        .as_ref()
        .map(|c| c.age_seconds < ttl)
        .unwrap_or(false);
    if !options.refresh && fresh {
        if let Some(usable) = usable.as_ref() {
            return cached_hub_result(
                config,
                endpoint.as_deref(),
                &fetched_at,
                ttl,
                usable,
                false,
                None,
            );
        }
    }

    // fail_with_cache closure equivalent: serve stale cache flagged,
    // else the base failure status/detail.
    macro_rules! fail_with_cache {
        ($status:expr, $detail:expr) => {{
            if let Some(usable) = usable.as_ref() {
                return cached_hub_result(
                    config,
                    endpoint.as_deref(),
                    &fetched_at,
                    ttl,
                    usable,
                    true,
                    Some(format!(
                        "hub request failed ({}); serving cached reading",
                        $detail
                    )),
                );
            }
            if let Some(obj) = result.as_object_mut() {
                obj.insert("status".to_string(), Value::String($status.to_string()));
                obj.insert("detail".to_string(), Value::String($detail.to_string()));
            }
            return result;
        }};
    }

    let auth_files = fetch_json(
        &management_url(&base, "auth-files"),
        &[("authorization".to_string(), format!("Bearer {}", key))],
        "GET",
        None,
        timeout_ms,
    );
    if !auth_files.ok {
        if auth_files.status == Some(401) || auth_files.status == Some(403) {
            fail_with_cache!(
                "unauthorized",
                format!(
                    "hub rejected the management key (HTTP {})",
                    auth_files.status.unwrap_or(0)
                )
            );
        }
        fail_with_cache!(
            "request_failed",
            auth_files.detail.unwrap_or_else(|| "request failed".to_string())
        );
    }
    let files_raw = auth_files
        .json
        .as_ref()
        .and_then(|j| j.get("files"))
        .and_then(|v| v.as_array());
    let Some(files_raw) = files_raw else {
        fail_with_cache!(
            "request_failed",
            "auth-files response was not the expected shape".to_string()
        );
    };

    let mut accounts: Vec<Value> = Vec::new();
    let mut cache_accounts: Vec<HubCacheAccount> = Vec::new();
    for entry in files_raw {
        let Some(account) = entry.as_object() else { continue };
        if account.get("disabled").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let account_id = account
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let provider = account
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if account_id.is_empty() || provider.is_empty() {
            continue;
        }
        let Some(spec) = spec_for(&provider) else {
            accounts.push(json!({
                "account": account_id,
                "provider": provider,
                "status": "unsupported",
                "detail": format!("hub provider \"{}\" has no quota mapper in this plugin", provider),
                "plan": Value::Null,
                "windows": [],
                "checked_at": Value::Null,
                "reset_credits_available": Value::Null,
            }));
            continue;
        };
        let reading = read_hub_account(&base, &key, entry, spec, timeout_ms, now_ms);
        // Only durable readings enter the cache: `ok` windows and
        // stable `unsupported` verdicts. Transient request_failed
        // entries are not cached — a stale serve must not resurrect a
        // failure as fresh.
        if reading.status == "ok" {
            cache_accounts.push(HubCacheAccount {
                account: account_id.clone(),
                provider: provider.clone(),
                plan: reading.plan.clone(),
                windows: reading.windows.clone(),
                reset_credits_available: reading.reset_credits_available,
            });
        }
        accounts.push(json!({
            "account": account_id,
            "provider": provider,
            "status": reading.status,
            "detail": reading.detail,
            "plan": reading.plan,
            "windows": reading.windows,
            "checked_at": reading.checked_at,
            "reset_credits_available": reading.reset_credits_available,
        }));
    }

    let checked_at = iso_from_ms(now_ms).unwrap_or_default();
    write_hub_cache(
        state_dir,
        &config.id,
        &HubCachePayload {
            checked_at: checked_at.clone(),
            accounts: cache_accounts,
        },
    );
    if let Some(obj) = result.as_object_mut() {
        obj.insert("status".to_string(), Value::String("ok".to_string()));
        obj.insert("checked_at".to_string(), Value::String(checked_at));
        obj.insert("accounts".to_string(), Value::Array(accounts));
    }
    result
}

/// Collect quota reports for every configured hub. Never throws for
/// per-hub failures — every outcome is a structured status.
pub fn collect_quota_hubs(options: &HubOptions) -> Vec<Value> {
    let timeout_ms = options.timeout_ms.unwrap_or(10_000);
    let ttl = options.cache_ttl_seconds.unwrap_or(300);
    let state_dir = options
        .state_dir
        .clone()
        .unwrap_or_else(|| plugin_dirs(PLUGIN_ID, None, None).state_dir);
    // Retention GC (T4 / #93) — shared with collect_quota; best-effort.
    gc_plugin_state_cache(&state_dir, None, None);
    let mut results = Vec::new();
    for config in options.hubs.clone().unwrap_or_default() {
        if config.id.is_empty() || config.url.is_empty() {
            continue;
        }
        results.push(quota_for_hub(
            &config, options, &state_dir, ttl, timeout_ms,
        ));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_base_url_normalizes_panel_urls_and_rejects_non_http() {
        let (base, endpoint) = hub_base_url("https://hub.example.com/management.html").unwrap();
        assert_eq!(endpoint, "https://hub.example.com/");
        assert!(base.ends_with('/'));
        let (_, endpoint) = hub_base_url("http://127.0.0.1:8317/").unwrap();
        assert_eq!(endpoint, "http://127.0.0.1:8317/");
        let (_, endpoint) =
            hub_base_url("http://127.0.0.1:8317/some/path/?x=1#f").unwrap();
        assert_eq!(endpoint, "http://127.0.0.1:8317/some/path/");
        assert!(hub_base_url("not a url").is_none());
        assert!(hub_base_url("ftp://x/").is_none());
    }
}
