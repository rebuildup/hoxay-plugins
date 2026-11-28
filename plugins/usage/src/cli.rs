//! rebuildup.usage CLI — token usage aggregation contract
//! (`herdr-plugins.usage/v1`) for agents and external clients.
//! Single JSON document on stdout; diagnostics on stderr.
//! Exit 0 success, 1 handled error, 2 usage.
//!
//! Rust port of `plugins/usage/src/cli.ts` (retired; git history
//! preserves it). Aggregation consumes the `rebuildup.conversations`
//! read-only export (`usage` event paging + `capability` provider
//! matrix) — never its state files, never PTY text (ADR-0013). The
//! plugin is read-only and keeps no durable state: totals are
//! recomputed from the index on every call.
//!
//! `run_domain` is the in-process dispatch the TUI uses — the same
//! `cmd_*` cores the CLI entrypoint uses (no self-respawn), per the
//! #291/#293 pattern.

use chrono::TimeZone;
use std::collections::{HashMap, HashSet};

use hoxay_herdr_client::dirs::plugin_dirs;
use serde_json::{json, Map, Value};

use crate::aggregate::{aggregate_usage, build_cache_tokens};
use crate::cost::compute_model_costs;
use crate::divergence::build_divergence;
use crate::hub::{collect_quota_hubs, hub_base_url, HubOptions, DEFAULT_HUB_KEY_ENV};
use crate::model::{
    ModelPriceOverride, QuotaEndpoints, QuotaHubConfig, SourceOptions, UsageWindow, CONTRACT,
    CONVERSATIONS_CONTRACT, PLUGIN_ID, PLUGIN_VERSION,
};
use crate::pricing::{
    collect_pricing, PricingOptions, PRICING_CACHE_TTL_SECONDS, PRICING_SOURCE_URL,
};
use crate::quota::{collect_quota, quota_providers, QuotaOptions, QUOTA_CACHE_TTL_SECONDS};
use crate::source::{
    export_usage_events, fetch_conversations_capability, resolve_conversations_source,
    CliCallOptions, ExportOutcome, SourceResolution, UsageSelectors,
};

const COST_DISCLAIMER: &str = "estimated_cost_usd is a list-price ESTIMATE computed from the model pricing table (LiteLLM snapshot or user modelPrices overrides) over standard token rates. It is not a bill, invoice, or subscription charge: it excludes speed premiums, context-tier repricing, provider discounts, taxes, and quota consumption. provider_reported_cost_usd is a separate provider-reported figure and is never blended in. Models without a rate report unpriced — see cost.unpriced_models.";

// --- errors ---------------------------------------------------------------------

/// Structured command failure — mirrors TS `fail(message, code, kind, extra)`.
#[derive(Debug)]
pub struct RunError {
    pub message: String,
    pub code: i32,
    pub kind: Option<String>,
    pub extra: Option<Map<String, Value>>,
}

impl RunError {
    pub fn new(message: impl Into<String>, code: i32) -> Self {
        RunError {
            message: message.into(),
            code,
            kind: None,
            extra: None,
        }
    }
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }
    pub fn with_extra(mut self, extra: Map<String, Value>) -> Self {
        self.extra = Some(extra);
        self
    }
    /// The stderr error envelope line (single JSON document).
    pub fn stderr_doc(&self) -> String {
        let mut error = Map::new();
        error.insert(
            "code".to_string(),
            json!(if self.code == 2 { "usage" } else { "error" }),
        );
        error.insert("message".to_string(), json!(self.message));
        if let Some(kind) = &self.kind {
            error.insert("kind".to_string(), json!(kind));
        }
        if let Some(extra) = &self.extra {
            for (k, v) in extra {
                error.insert(k.clone(), v.clone());
            }
        }
        json!({
            "contract": CONTRACT,
            "ok": false,
            "error": Value::Object(error),
        })
        .to_string()
    }
}

// --- args ------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CliOptions {
    command: String,
    args: Vec<String>,
    session: Option<String>,
    socket_path: Option<String>,
    timeout_ms: Option<f64>,
    offline: bool,
    pane: Option<String>,
    session_id: Option<String>,
    provider: Option<String>,
    from: Option<String>,
    to: Option<String>,
    conversations_root: Option<String>,
    conversations_state_dir: Option<String>,
    conversations_config_dir: Option<String>,
    refresh: bool,
}

fn parse_args(argv: &[String]) -> CliOptions {
    let mut iter = argv.iter();
    let command = iter.next().cloned().unwrap_or_else(|| "help".to_string());
    let rest: Vec<String> = iter.cloned().collect();
    let mut options = CliOptions {
        command,
        ..Default::default()
    };
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        let next = |i: &mut usize| -> Option<String> {
            *i += 1;
            rest.get(*i).cloned()
        };
        match arg.as_str() {
            "--session" => options.session = next(&mut i),
            "--session-id" => options.session_id = next(&mut i),
            "--pane" => options.pane = next(&mut i),
            "--provider" => options.provider = next(&mut i),
            "--from" => options.from = next(&mut i),
            "--to" => options.to = next(&mut i),
            "--socket" => options.socket_path = next(&mut i),
            "--timeout" => {
                if let Some(v) = next(&mut i).and_then(|s| s.parse::<f64>().ok()) {
                    if v.is_finite() && v > 0.0 {
                        options.timeout_ms = Some(v);
                    }
                }
            }
            "--offline" => options.offline = true,
            "--conversations-root" => options.conversations_root = next(&mut i),
            "--conversations-state-dir" => options.conversations_state_dir = next(&mut i),
            "--conversations-config-dir" => options.conversations_config_dir = next(&mut i),
            "--refresh" => options.refresh = true,
            _ => positional.push(arg.clone()),
        }
        i += 1;
    }
    options.args = positional;
    options
}

// --- config ----------------------------------------------------------------------

/// Parsed `config.json` under the plugin config dir. Unknown/malformed
/// fields are ignored exactly like the TS typed cast.
#[derive(Debug, Default)]
struct PluginConfig {
    timeout_ms: Option<f64>,
    conversations_root: Option<String>,
    conversations_state_dir: Option<String>,
    conversations_config_dir: Option<String>,
    quota_cache_ttl_seconds: Option<i64>,
    quota_endpoints: Option<QuotaEndpoints>,
    /// CLIProxyAPI-equivalent quota hubs (T3 / #92, ADR-0015).
    quota_hubs: Vec<QuotaHubConfig>,
    /// Estimated-cost pricing (T4 / #93): source override + TTL + user prices.
    pricing_url: Option<String>,
    pricing_cache_ttl_seconds: Option<i64>,
    model_prices: Option<HashMap<String, ModelPriceOverride>>,
}

fn str_field(v: Option<&Value>) -> Option<String> {
    v.and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn load_config() -> PluginConfig {
    let file = plugin_dirs(PLUGIN_ID, None, None)
        .config_dir
        .join("config.json");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return PluginConfig::default();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return PluginConfig::default();
    };
    let Some(obj) = parsed.as_object() else {
        return PluginConfig::default();
    };
    let get = |k: &str| obj.get(k);

    let quota_endpoints = get("quotaEndpoints").and_then(|v| v.as_object()).map(|e| {
        QuotaEndpoints {
            claude: str_field(e.get("claude")),
            codex_usage: str_field(e.get("codexUsage")),
            codex_credits: str_field(e.get("codexCredits")),
        }
    });

    // Validated hub configs from config.json (malformed entries dropped).
    let quota_hubs: Vec<QuotaHubConfig> = get("quotaHubs")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|h| {
                    let id = h.get("id").and_then(|v| v.as_str())?;
                    let url = h.get("url").and_then(|v| v.as_str())?;
                    if id.is_empty() || url.is_empty() {
                        return None;
                    }
                    Some(QuotaHubConfig {
                        id: id.to_string(),
                        url: url.to_string(),
                        // Preserve an explicit `keyEnv: ""` verbatim —
                        // TS `??` keeps "" (→ key_env "", key_configured
                        // false) while only an absent key falls back to
                        // the default env name downstream.
                        key_env: h
                            .get("keyEnv")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Per-entry parse: one malformed override drops only that entry,
    // matching the TS `Object.entries` + try/catch-per-value behavior —
    // a single bad value must not discard every valid override.
    let model_prices = get("modelPrices")
        .and_then(|v| v.as_object())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(k, val)| {
                    serde_json::from_value::<ModelPriceOverride>(val.clone())
                        .ok()
                        .map(|p| (k.clone(), p))
                })
                .collect::<HashMap<String, ModelPriceOverride>>()
        });

    PluginConfig {
        timeout_ms: get("timeoutMs").and_then(|v| v.as_f64()),
        conversations_root: str_field(get("conversationsRoot")),
        conversations_state_dir: str_field(get("conversationsStateDir")),
        conversations_config_dir: str_field(get("conversationsConfigDir")),
        quota_cache_ttl_seconds: get("quotaCacheTtlSeconds").and_then(|v| v.as_i64()),
        quota_endpoints,
        quota_hubs,
        pricing_url: str_field(get("pricingUrl")),
        pricing_cache_ttl_seconds: get("pricingCacheTtlSeconds").and_then(|v| v.as_i64()),
        model_prices,
    }
}

/// CLI flag > env > config.json > default for each source override.
fn source_options(flags: &CliOptions, config: &PluginConfig) -> SourceOptions {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    SourceOptions {
        conversations_root: flags
            .conversations_root
            .clone()
            .or_else(|| env("HERDR_USAGE_CONVERSATIONS_ROOT"))
            .or_else(|| config.conversations_root.clone()),
        conversations_state_dir: flags
            .conversations_state_dir
            .clone()
            .or_else(|| env("HERDR_USAGE_CONVERSATIONS_STATE_DIR"))
            .or_else(|| config.conversations_state_dir.clone()),
        conversations_config_dir: flags
            .conversations_config_dir
            .clone()
            .or_else(|| env("HERDR_USAGE_CONVERSATIONS_CONFIG_DIR"))
            .or_else(|| config.conversations_config_dir.clone()),
    }
}

fn call_options(options: &CliOptions, config: &PluginConfig) -> CliCallOptions {
    CliCallOptions {
        session: options.session.clone(),
        socket_path: options.socket_path.clone(),
        timeout_ms: options
            .timeout_ms
            .or(config.timeout_ms)
            .map(|t| t.max(1.0) as u64),
        offline: options.offline,
    }
}

// --- helpers ---------------------------------------------------------------------

fn now_iso() -> String {
    crate::quota::iso_from_ms(crate::quota::now_ms()).unwrap_or_default()
}

/// JS `Date.parse` subset: ISO-8601 date / date-time forms.
///
/// - `YYYY-MM-DD` → UTC midnight (ECMAScript date-only rule).
/// - `YYYY-MM-DD[T ]HH:mm[:ss[.fff…]]` with `Z` or `±HH:mm`/`±HHmm`/`±HH`
///   → that offset; without a zone → **local** time (ECMAScript rule).
/// - Anything else → None (mirrors `NaN`).
///
/// Fractional seconds are truncated/padded to milliseconds; out-of-range
/// fields are rejected (V8 returns NaN for invalid ISO dates).
pub fn parse_js_date(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let b = t.as_bytes();
    // Minimal shape check: at least YYYY-MM-DD.
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i32 = t.get(0..4)?.parse().ok()?;
    let mo: u32 = t.get(5..7)?.parse().ok()?;
    let d: u32 = t.get(8..10)?.parse().ok()?;
    let date = chrono::NaiveDate::from_ymd_opt(y, mo, d)?;

    if b.len() == 10 {
        // Date-only → UTC midnight.
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Some(dt.and_utc().timestamp_millis() as f64);
    }

    // Date-time: separator T or space at index 10.
    if !(b[10] == b'T' || b[10] == b' ') {
        return None;
    }
    let time_part = &t[11..];
    // Split zone suffix: trailing Z or ±HH[:mm|mm|H].
    let (time_core, offset) = split_zone(time_part)?;
    // time_core = HH:mm[:ss[.frac]]
    let (hms, frac_ms) = parse_hms_frac(time_core)?;
    let naive = date.and_time(hms);

    let ms = match offset {
        Some(off_ms) => {
            // Local wall time interpreted at (UTC+off): epoch = naive_utc - off
            let utc_naive = naive - chrono::Duration::milliseconds(off_ms);
            utc_naive.and_utc().timestamp_millis() as f64
        }
        None => {
            // No zone → local time.
            let local = chrono::Local
                .from_local_datetime(&naive)
                .earliest()?;
            local.timestamp_millis() as f64
        }
    } + frac_ms as f64;
    Some(ms)
}

/// Split a trailing zone designator off `time_part`.
/// Returns (core, offset_ms_from_utc). `Z` → Some(0); `±HH…` → signed
/// offset in ms; absent → None (local time).
fn split_zone(time_part: &str) -> Option<(&str, Option<i64>)> {
    let b = time_part.as_bytes();
    // Trailing 'Z'/'z' (JS accepts only uppercase Z in ISO; V8 also
    // accepts lowercase in practice — accept both).
    if let Some(last) = b.last() {
        if *last == b'Z' || *last == b'z' {
            return Some((&time_part[..time_part.len() - 1], Some(0)));
        }
    }
    // Find a '+'/'-' that begins the zone: it must come after the
    // HH:mm[:ss[.frac]] core — i.e. after at least 5 chars.
    for (i, ch) in time_part.char_indices() {
        if (ch == '+' || ch == '-') && i >= 5 {
            let core = &time_part[..i];
            let zone = &time_part[i..];
            let off = parse_zone_offset(zone)?;
            return Some((core, Some(off)));
        }
    }
    Some((time_part, None))
}

/// Parse `±HH`, `±HHMM`, `±HH:MM` → offset ms (sign * (hh*3600+mm*60)*1000).
fn parse_zone_offset(zone: &str) -> Option<i64> {
    let b = zone.as_bytes();
    if b.len() < 3 {
        return None;
    }
    let sign: i64 = if b[0] == b'-' { -1 } else { 1 };
    let rest = &zone[1..];
    let (hh_s, mm_s) = if let Some(colon) = rest.find(':') {
        (&rest[..colon], &rest[colon + 1..])
    } else if rest.len() >= 4 {
        (&rest[..2], &rest[2..4])
    } else {
        (rest, "0")
    };
    let hh: i64 = hh_s.parse().ok()?;
    let mm: i64 = if mm_s.is_empty() { 0 } else { mm_s.parse().ok()? };
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3600 + mm * 60) * 1000)
}

/// Parse `HH:mm[:ss[.frac]]` → (NaiveTime, extra_ms).
fn parse_hms_frac(s: &str) -> Option<(chrono::NaiveTime, i64)> {
    let mut parts = s.splitn(3, ':');
    let hh: u32 = parts.next()?.parse().ok()?;
    let mm: u32 = parts.next()?.parse().ok()?;
    let sec_part = parts.next();
    let (ss, frac_ms) = match sec_part {
        None => (0u32, 0i64),
        Some(sp) => {
            let (sec_s, frac_s) = match sp.find('.') {
                Some(dot) => (&sp[..dot], &sp[dot + 1..]),
                None => (sp, ""),
            };
            let ss: u32 = sec_s.parse().ok()?;
            // JS keeps sub-ms precision in the double; we truncate to ms
            // (contract timestamps are ms-precision anyway).
            let frac_ms = if frac_s.is_empty() {
                0i64
            } else {
                let digits: String = frac_s.chars().take(3).collect();
                let mut padded = digits;
                while padded.len() < 3 {
                    padded.push('0');
                }
                padded.parse::<i64>().ok()?
            };
            (ss, frac_ms)
        }
    };
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let time = chrono::NaiveTime::from_hms_opt(hh, mm, ss)?;
    Some((time, frac_ms))
}

// --- capability ---------------------------------------------------------------

fn cmd_capability() -> Result<Value, RunError> {
    let config = load_config();
    let resolved = resolve_conversations_source(&source_options_flags_empty(&config));
    // capability must never fail on source resolution — report it.
    let source_entry = match resolved.source() {
        Some(source) => json!({
            "id": "conversations-index",
            "kind": "normalized-events",
            "contract": CONVERSATIONS_CONTRACT,
            "status": "available",
            "plugin_root": source.plugin_root.to_string_lossy(),
            "resolved_via": source.resolved_via,
            "freshness": "as of the last `rebuildup.conversations index`/`refresh` run; run it to pick up new transcripts",
        }),
        None => json!({
            "id": "conversations-index",
            "kind": "normalized-events",
            "contract": CONVERSATIONS_CONTRACT,
            "status": "unavailable",
            "detail": resolved.error().unwrap_or("unavailable"),
        }),
    };

    let hubs: Vec<Value> = config
        .quota_hubs
        .iter()
        .map(|h| {
            let key_env = h
                .key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_HUB_KEY_ENV.to_string());
            json!({
                "id": h.id,
                "endpoint": hub_base_url(&h.url).map(|(_, e)| e),
                "key_env": key_env,
                "key_configured": std::env::var(&key_env)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false),
            })
        })
        .collect();

    Ok(json!({
        "contract": CONTRACT,
        "command": "capability",
        "plugin": PLUGIN_ID,
        "version": PLUGIN_VERSION,
        "read_only": true,
        "mutation_commands": [],
        "commands": ["capability", "usage", "quota", "divergence", "cost"],
        "sources": [
            source_entry,
            {
                "id": "subscription-quota-api",
                "kind": "provider-quota",
                "status": "available",
                "credential_boundary": "docs/adr/0014-usage-quota-credential-boundary.md",
                "providers": {
                    "claude": "implemented (oauth usage endpoint, ~/.claude credentials / macOS Keychain)",
                    "codex": "implemented (wham usage endpoint, ChatGPT auth.json only)",
                    "opencode": "unsupported (per-provider API keys; no subscription quota artifact)",
                },
                "freshness": "short-term cache (default 300s) under the plugin state dir; stale readings are flagged, never silently refreshed",
                "detail": "Quota probes never emit credential material; see ADR-0014 for the read/logging boundary.",
            },
            {
                "id": "cliproxy-api",
                "kind": "http-quota-hub",
                "status": "available",
                "credential_boundary": "docs/adr/0015-usage-hub-credential-boundary.md",
                "hubs": hubs,
                "providers": {
                    "claude": "implemented via hub api-call (provider token stays hub-side)",
                    "codex": "implemented via hub api-call (provider token stays hub-side)",
                    "note": "hub accounts of other providers are reported per-account as unsupported",
                },
                "freshness": "shares the quota cache TTL under the plugin state dir; stale readings are flagged, never silently refreshed",
                "detail": "Hub management keys come from env vars named by config `quotaHubs[].keyEnv`; they are sent only to the configured hub origin and never logged, persisted, cached, or emitted (ADR-0015).",
            },
            {
                "id": "model-pricing",
                "kind": "price-table",
                "status": "available",
                "source": PRICING_SOURCE_URL,
                "freshness": "cached under the plugin state dir (default 24h TTL, 30-day retention); stale tables are flagged, never silently trusted",
                "detail": "Estimated API cost only — list-price math over standard rates, never a bill. Unknown models report unpriced, never a guessed price. User overrides via config `modelPrices`.",
            },
        ],
        "aggregation": {
            "dimensions": ["provider", "model", "host", "project", "session", "pane"],
            "window": "--from/--to ISO-8601 bounds on event timestamps (inclusive)",
            "dedup": "(session_key, event_id); index rewrites are flagged, never hidden",
            "honesty": "missing usage is never zero-fabricated; provider_reported_cost_usd is not a billing amount",
        },
        "generated_at": now_iso(),
    }))
}

/// `capability` takes no CLI flags — only config/env source overrides.
fn source_options_flags_empty(config: &PluginConfig) -> SourceOptions {
    source_options(&CliOptions::default(), config)
}

// --- window -----------------------------------------------------------------------

fn parse_window(options: &CliOptions) -> Result<UsageWindow, RunError> {
    let mut from_ms = None;
    let mut to_ms = None;
    if let Some(from) = &options.from {
        match parse_js_date(from) {
            Some(ms) => from_ms = Some(ms),
            None => {
                return Err(RunError::new(
                    format!("invalid --from timestamp: {}", from),
                    2,
                ))
            }
        }
    }
    if let Some(to) = &options.to {
        match parse_js_date(to) {
            Some(ms) => to_ms = Some(ms),
            None => {
                return Err(RunError::new(
                    format!("invalid --to timestamp: {}", to),
                    2,
                ))
            }
        }
    }
    if let (Some(f), Some(t)) = (from_ms, to_ms) {
        if f > t {
            return Err(RunError::new(
                format!(
                    "--from ({}) is after --to ({})",
                    options.from.clone().unwrap_or_default(),
                    options.to.clone().unwrap_or_default()
                ),
                2,
            ));
        }
    }
    Ok(UsageWindow { from_ms, to_ms })
}

// --- usage report -------------------------------------------------------------------

/// Build the `usage` command payload (shared by `usage`, `cost`, and
/// `divergence`). Returns Err (→ fail) on source/export errors.
fn build_usage_report(
    options: &CliOptions,
    config: &PluginConfig,
    window: &UsageWindow,
) -> Result<Value, RunError> {
    let resolved = resolve_conversations_source(&source_options(options, config));
    let source: SourceResolution = match resolved.source() {
        Some(s) => s.clone(),
        None => {
            let mut extra = Map::new();
            extra.insert(
                "hint".to_string(),
                json!("install rebuildup.conversations or set conversationsRoot in config.json"),
            );
            return Err(
                RunError::new(resolved.error().unwrap_or("conversations unavailable"), 1)
                    .with_kind(resolved.kind().unwrap_or("conversations_unavailable"))
                    .with_extra(extra),
            );
        }
    };
    let call = call_options(options, config);
    let selectors = UsageSelectors {
        pane: options.pane.clone(),
        session_id: options.session_id.clone(),
        provider: options.provider.clone(),
    };

    let exported = match export_usage_events(&source, &call, Some(&selectors)) {
        ExportOutcome::Ok(e) => e,
        ExportOutcome::Err { error, kind } => {
            return Err(RunError::new(error, 1).with_kind(kind));
        }
    };

    let capability = fetch_conversations_capability(&source, &call);
    let mut provider_status: HashMap<String, String> = HashMap::new();
    let mut cache_capability: HashMap<String, String> = HashMap::new();
    if let Some(cap) = &capability {
        for p in &cap.providers {
            provider_status.insert(p.id.clone(), p.status.clone());
            if let Some(ct) = &p.cache_tokens {
                cache_capability.insert(p.id.clone(), ct.clone());
            }
        }
    }

    let provider_status_ref = provider_status;
    let mut aggregate = aggregate_usage(&crate::aggregate::AggregateInput {
        events: &exported.events,
        sessions: &exported.sessions,
        window: *window,
        provider_status: &provider_status_ref,
    });

    // Providers declared by conversations but with zero indexed
    // sessions still belong in coverage — "supported but nothing
    // indexed" is a different state from "unsupported".
    let coverage_providers = aggregate
        .get("coverage")
        .and_then(|c| c.get("providers"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let seen: HashSet<String> = coverage_providers
        .iter()
        .filter_map(|p| p.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    let mut providers = coverage_providers;
    if let Some(cap) = &capability {
        for p in &cap.providers {
            if seen.contains(&p.id) {
                continue;
            }
            providers.push(json!({
                "provider": p.id,
                "provider_status": p.status,
                "sessions": 0,
                "sessions_ok": 0,
                "sessions_with_issues": 0,
                "usage_events": 0,
            }));
        }
    }
    providers.sort_by(|a, b| {
        let au = a.get("usage_events").and_then(|v| v.as_u64()).unwrap_or(0);
        let bu = b.get("usage_events").and_then(|v| v.as_u64()).unwrap_or(0);
        bu.cmp(&au).then_with(|| {
            let ap = a.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let bp = b.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            ap.cmp(bp)
        })
    });
    if let Some(coverage) = aggregate
        .as_object_mut()
        .and_then(|o| o.get_mut("coverage"))
        .and_then(|c| c.as_object_mut())
    {
        coverage.insert("providers".to_string(), Value::Array(providers.clone()));
    }

    // Cache-token breakdown: measured sums from the provider buckets,
    // accounting status from the adapter capability matrix (T3 / #92).
    let measured = aggregate
        .get("breakdowns")
        .and_then(|b| b.get("by_provider"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let cache_tokens = build_cache_tokens(&providers, &measured, &cache_capability);
    if let Some(obj) = aggregate.as_object_mut() {
        obj.insert("cache_tokens".to_string(), cache_tokens);
    }

    let totals = aggregate
        .get("totals")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut warnings: Vec<String> = Vec::new();
    if !exported.complete {
        warnings.push("export_incomplete_page_limit".to_string());
    }
    if exported.malformed_lines > 0 {
        warnings.push("index_malformed_lines".to_string());
    }
    if exported.duplicates_skipped > 0 {
        warnings.push("index_duplicate_events_skipped".to_string());
    }
    if !exported.restarted_sessions.is_empty() {
        warnings.push("index_sessions_restarted".to_string());
    }
    if totals
        .get("excluded_untimestamped_events")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0
    {
        warnings.push("untimestamped_events_excluded_by_window".to_string());
    }
    if totals
        .get("duplicate_events_skipped")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0
    {
        warnings.push("duplicate_events_skipped".to_string());
    }
    if totals
        .get("events_without_event_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0
    {
        warnings.push("events_without_event_id".to_string());
    }

    let sessions_total = aggregate
        .get("coverage")
        .and_then(|c| c.get("sessions_total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let events_total = totals.get("events").and_then(|v| v.as_u64()).unwrap_or(0);

    let window_value = if window.from_ms.is_none() && window.to_ms.is_none() {
        Value::Null
    } else {
        json!({
            "from": options.from.as_deref(),
            "to": options.to.as_deref(),
        })
    };

    let mut payload = json!({
        "contract": CONTRACT,
        "command": "usage",
        "window": window_value,
        "filters": {
            "pane_id": options.pane.as_deref(),
            "session_id": options.session_id.as_deref(),
            "provider": options.provider.as_deref(),
        },
        "totals": totals,
        "breakdowns": aggregate.get("breakdowns").cloned().unwrap_or_else(|| json!({})),
        "cache_tokens": aggregate.get("cache_tokens").cloned().unwrap_or(Value::Null),
        "coverage": aggregate.get("coverage").cloned().unwrap_or_else(|| json!({})),
        "sessions": aggregate.get("sessions").cloned().unwrap_or_else(|| json!([])),
        "source": {
            "plugin": "rebuildup.conversations",
            "contract": exported.contract.clone().unwrap_or_else(|| CONVERSATIONS_CONTRACT.to_string()),
            "plugin_root": source.plugin_root.to_string_lossy(),
            "resolved_via": source.resolved_via,
            "pages": exported.pages,
            "complete": exported.complete,
        },
        "warnings": warnings,
        "generated_at": now_iso(),
    });
    if events_total == 0 && sessions_total == 0 {
        payload
            .as_object_mut()
            .unwrap()
            .insert(
                "hint".to_string(),
                json!("no indexed sessions; run `rebuildup.conversations index` to build the transcript index"),
            );
    }
    Ok(payload)
}

// --- quota -------------------------------------------------------------------------

/// Subscription quota/reset report. Per-provider outcomes are data,
/// not errors: the command exits 0 whenever it can produce the
/// envelope, and each provider carries an explicit status (`ok` /
/// `unsupported` / `no_credential` / `credential_expired` /
/// `request_failed`). Credential material never appears in output
/// (ADR-0014).
fn quota_env_endpoints(config: &PluginConfig) -> QuotaEndpoints {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    QuotaEndpoints {
        claude: env("HERDR_USAGE_QUOTA_CLAUDE_URL")
            .or_else(|| config.quota_endpoints.as_ref().and_then(|e| e.claude.clone())),
        codex_usage: env("HERDR_USAGE_QUOTA_CODEX_USAGE_URL")
            .or_else(|| config.quota_endpoints.as_ref().and_then(|e| e.codex_usage.clone())),
        codex_credits: env("HERDR_USAGE_QUOTA_CODEX_CREDITS_URL")
            .or_else(|| config.quota_endpoints.as_ref().and_then(|e| e.codex_credits.clone())),
    }
}

fn quota_ttl(config: &PluginConfig) -> i64 {
    config
        .quota_cache_ttl_seconds
        .filter(|t| *t > 0)
        .unwrap_or(QUOTA_CACHE_TTL_SECONDS)
}

fn cmd_quota(options: &CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let call = call_options(options, &config);
    let providers = match &options.provider {
        Some(p) if !p.is_empty() => Some(vec![p.clone()]),
        _ => None,
    };
    let ttl = quota_ttl(&config);

    let results = collect_quota(&QuotaOptions {
        providers,
        refresh: options.refresh,
        timeout_ms: Some(call.timeout_ms.unwrap_or(10_000)),
        state_dir: None,
        endpoints: Some(quota_env_endpoints(&config)),
        cache_ttl_seconds: Some(ttl),
    });
    let hubs = collect_quota_hubs(&HubOptions {
        hubs: Some(config.quota_hubs.clone()),
        refresh: options.refresh,
        timeout_ms: Some(call.timeout_ms.unwrap_or(10_000)),
        state_dir: None,
        cache_ttl_seconds: Some(ttl),
    });

    Ok(json!({
        "contract": CONTRACT,
        "command": "quota",
        "cache": { "ttl_seconds": ttl },
        "providers": results,
        "hubs": hubs,
        "generated_at": now_iso(),
    }))
}

// --- cost -------------------------------------------------------------------------

fn cmd_cost(options: &CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let window = parse_window(options)?;
    let report = build_usage_report(options, &config, &window)?;
    let call = call_options(options, &config);

    let pricing_ttl = config
        .pricing_cache_ttl_seconds
        .filter(|t| *t > 0)
        .unwrap_or(PRICING_CACHE_TTL_SECONDS);
    let pricing = collect_pricing(&PricingOptions {
        refresh: options.refresh,
        timeout_ms: Some(call.timeout_ms.unwrap_or(10_000)),
        state_dir: None,
        source_url: std::env::var("HERDR_USAGE_PRICING_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or(config.pricing_url.clone()),
        ttl_seconds: Some(pricing_ttl),
        model_prices: config.model_prices.clone(),
    });

    let by_model = report
        .get("breakdowns")
        .and_then(|b| b.get("by_model"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let totals = report.get("totals").cloned().unwrap_or_else(|| json!({}));
    let computation = compute_model_costs(&by_model, &pricing);

    let info = pricing.info.clone().unwrap_or(crate::pricing::PricingInfo {
        status: "unavailable",
        source: String::new(),
        fetched_at: None,
        cache_hit: false,
        cache_age_seconds: None,
        ttl_seconds: pricing_ttl,
        known_models: 0,
        user_overrides: 0,
        detail: Some("pricing table unavailable".to_string()),
    });
    // TS field order: estimate, disclaimer, estimated_cost_usd,
    // estimate_complete, priced_models, unpriced_models,
    // provider_reported_cost_usd, pricing, by_model.
    let mut warnings: Vec<String> = report
        .get("warnings")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if info.status == "unavailable" {
        warnings.push("pricing_unavailable".to_string());
    }
    if info.status == "stale" {
        warnings.push("pricing_table_stale".to_string());
    }
    if computation
        .get("unpriced_models")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        warnings.push("unpriced_models".to_string());
    }

    let mut payload = json!({
        "contract": CONTRACT,
        "command": "cost",
        "window": report.get("window").cloned().unwrap_or(Value::Null),
        "filters": report.get("filters").cloned().unwrap_or_else(|| json!({})),
        "cost": cost_value(&computation, &info, &totals),
        "totals": totals,
        "breakdowns": report.get("breakdowns").cloned().unwrap_or_else(|| json!({})),
        "coverage": report.get("coverage").cloned().unwrap_or_else(|| json!({})),
        "sessions": report.get("sessions").cloned().unwrap_or_else(|| json!([])),
        "source": report.get("source").cloned().unwrap_or(Value::Null),
        "warnings": warnings,
        "generated_at": now_iso(),
    });
    if let Some(hint) = report.get("hint").and_then(|v| v.as_str()) {
        payload
            .as_object_mut()
            .unwrap()
            .insert("hint".to_string(), json!(hint));
    }
    Ok(payload)
}

fn cost_value(computation: &Value, info: &crate::pricing::PricingInfo, totals: &Value) -> Value {
    json!({
        "estimate": true,
        "disclaimer": COST_DISCLAIMER,
        "estimated_cost_usd": computation.get("estimated_cost_usd").cloned().unwrap_or(Value::Null),
        "estimate_complete": computation.get("estimate_complete").cloned().unwrap_or(json!(false)),
        "priced_models": computation.get("priced_models").cloned().unwrap_or(json!(0)),
        "unpriced_models": computation.get("unpriced_models").cloned().unwrap_or_else(|| json!([])),
        "provider_reported_cost_usd": totals.get("provider_reported_cost_usd").cloned().unwrap_or(Value::Null),
        "pricing": info.to_value(),
        "by_model": computation.get("by_model").cloned().unwrap_or_else(|| json!([])),
    })
}

// --- divergence -------------------------------------------------------------------

/// Quota-vs-measured divergence report (T3 / #92). Runs the same
/// aggregation as `usage`, probes the direct provider quota sources,
/// and places both sides side by side per provider — different units,
/// different scopes, never reconciled. Hub (cliproxy) readings are
/// reported under `hubs` but excluded from `comparisons`: a hub
/// account cannot be proven to be the same subscription as the locally
/// indexed transcripts.
fn cmd_divergence(options: &CliOptions) -> Result<Value, RunError> {
    let config = load_config();
    let window = parse_window(options)?;
    let report = build_usage_report(options, &config, &window)?;
    let call = call_options(options, &config);
    let ttl = quota_ttl(&config);

    let coverage_providers = report
        .get("coverage")
        .and_then(|c| c.get("providers"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let provider_filter = options
        .provider
        .clone()
        .filter(|p| !p.is_empty());

    // TS builds `providerIds` as a Set in insertion order: coverage
    // providers first, then QUOTA_PROVIDERS ids not already present;
    // `--provider` clears and re-adds just that id.
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in &coverage_providers {
        if let Some(id) = p.get("provider").and_then(|v| v.as_str()) {
            if seen.insert(id.to_string()) {
                ordered.push(id.to_string());
            }
        }
    }
    for spec in quota_providers() {
        if seen.insert(spec.id.clone()) {
            ordered.push(spec.id);
        }
    }
    if let Some(pf) = &provider_filter {
        ordered = vec![pf.clone()];
    }

    let quota_opts = QuotaOptions {
        providers: Some(ordered),
        refresh: options.refresh,
        timeout_ms: Some(call.timeout_ms.unwrap_or(10_000)),
        state_dir: None,
        endpoints: Some(quota_env_endpoints(&config)),
        cache_ttl_seconds: Some(ttl),
    };
    let quota_results = collect_quota(&quota_opts);
    let hubs = collect_quota_hubs(&HubOptions {
        hubs: Some(config.quota_hubs.clone()),
        refresh: options.refresh,
        timeout_ms: Some(call.timeout_ms.unwrap_or(10_000)),
        state_dir: None,
        cache_ttl_seconds: Some(ttl),
    });

    let providers_for_divergence: Vec<Value> = match &provider_filter {
        Some(pf) => coverage_providers
            .iter()
            .filter(|p| p.get("provider").and_then(|v| v.as_str()) == Some(pf.as_str()))
            .cloned()
            .collect(),
        None => coverage_providers,
    };
    let measured = report
        .get("breakdowns")
        .and_then(|b| b.get("by_provider"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let comparisons = build_divergence(
        &providers_for_divergence,
        &measured,
        &quota_results,
        &window,
    );

    Ok(json!({
        "contract": CONTRACT,
        "command": "divergence",
        "window": report.get("window").cloned().unwrap_or(Value::Null),
        "filters": report.get("filters").cloned().unwrap_or_else(|| json!({})),
        "comparisons": comparisons,
        "hubs": hubs,
        "notes": [
            "comparisons cover direct provider quota sources only; hub (cliproxy) readings under `hubs` are excluded because a hub account cannot be proven to be the same subscription as the locally indexed transcripts",
            "quota used_percent and measured token sums are different units from different sources — verdict `different_units` reports both sides and never reconciles them",
        ],
        "source": report.get("source").cloned().unwrap_or(Value::Null),
        "generated_at": now_iso(),
    }))
}

// --- dispatch -----------------------------------------------------------------------

/// Dispatch a domain command in-process — the same `cmd_*` cores the
/// CLI entrypoint uses. The TUI calls this; it never respawns itself.
pub fn run_domain(args: &[String]) -> Result<Value, RunError> {
    let options = parse_args(args);
    match options.command.as_str() {
        "capability" => cmd_capability(),
        "usage" => {
            let config = load_config();
            let window = parse_window(&options)?;
            build_usage_report(&options, &config, &window)
        }
        "quota" => cmd_quota(&options),
        "cost" => cmd_cost(&options),
        "divergence" => cmd_divergence(&options),
        "help" | "--help" | "-h" => Ok(json!({
            "contract": CONTRACT,
            "plugin": PLUGIN_ID,
            "version": PLUGIN_VERSION,
            "usage": "usage <capability|usage|quota|cost|divergence> [--pane PANE] [--session-id ID] [--provider ID] [--from ISO] [--to ISO] [--refresh] [--session NAME] [--socket PATH] [--timeout MS] [--offline] [--conversations-root DIR] [--conversations-state-dir DIR]",
        })),
        other => Err(RunError::new(format!("unknown command: {}", other), 2)),
    }
}

/// CLI entrypoint: emit the JSON document or the error envelope;
/// returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    match run_domain(argv) {
        Ok(payload) => {
            println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{}", e.stderr_doc());
            e.code
        }
    }
}
