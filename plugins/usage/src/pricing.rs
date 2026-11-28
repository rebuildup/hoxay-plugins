//! Estimated API-cost pricing for rebuildup.usage (Issue #93 /
//! parent #28 T4). Rust port of `plugins/usage/src/pricing.ts`
//! (retired; git history preserves it).
//!
//! The rate table mirrors T3 Code's `apps/server/src/usage/
//! usagePricing.ts` approach: a LiteLLM `model_prices_and_context_window`
//! snapshot fetched at runtime, cached under the plugin state dir, and
//! applied to aggregated token buckets. Two honesty rules from #93:
//!
//! - The result is an **estimate** (list-price math over standard
//!   rates), never a bill. Speed premiums, context-tier repricing,
//!   provider discounts, taxes, and subscription quota consumption are
//!   all out of scope — the contract labels this explicitly.
//! - An unknown model yields `null`, never a guessed price. Models the
//!   table cannot price are listed as unpriced so partial totals are
//!   never mistaken for complete ones.
//!
//! Credential boundary: the pricing fetch is unauthenticated public
//! data; no credential material is involved (ADR-0014/0015 unaffected).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use hoxay_herdr_client::dirs::plugin_dirs;
use serde_json::{json, Value};

use crate::model::ModelPriceOverride;
use crate::quota::{fetch_json, now_ms};
use crate::state::gc_plugin_state_cache;

/// LiteLLM community price list — the same upstream T3 Code prices from.
pub const PRICING_SOURCE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Fresh-cache TTL for the pricing table (seconds).
pub const PRICING_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

/// Hard retention for cached pricing data; older entries are GC'd.
pub const PRICING_CACHE_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const PLUGIN_ID: &str = "rebuildup.usage";

/// USD-per-token rates for the four canonical token buckets.
#[derive(Debug, Clone, Copy)]
pub struct TokenRates {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_cost_per_token: f64,
    pub cache_creation_cost_per_token: f64,
}

/// Rate table: normalized model key → rates.
pub type RateTable = HashMap<String, TokenRates>;

// --- parsing ------------------------------------------------------------------

fn finite_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite() && *f >= 0.0)
}

fn normalize_rate_key(model: &str) -> String {
    model.trim().to_lowercase()
}

/// Parse a LiteLLM `model_prices_and_context_window` document into a
/// rate table. Entries missing input or output pricing are dropped —
/// a partial rate would understate cost, which is worse than
/// reporting the model unpriced. Speed variants and context tiers are
/// ignored: aggregated totals carry no per-request speed/prompt-size
/// signal, so only standard rates can be applied honestly. 1-hour
/// cache writes are priced at the standard cache-creation rate.
pub fn parse_rate_table(document: &Value) -> RateTable {
    let mut table = RateTable::new();
    let Some(obj) = document.as_object() else {
        return table;
    };
    for (name, raw) in obj {
        let Some(entry) = raw.as_object() else { continue };
        let input = finite_number(entry.get("input_cost_per_token"));
        let output = finite_number(entry.get("output_cost_per_token"));
        let (Some(input), Some(output)) = (input, output) else {
            continue;
        };
        let key = normalize_rate_key(name);
        if key.is_empty() {
            continue;
        }
        table.insert(
            key,
            TokenRates {
                input_cost_per_token: input,
                output_cost_per_token: output,
                cache_read_cost_per_token: finite_number(
                    entry.get("cache_read_input_token_cost"),
                )
                .unwrap_or(input),
                cache_creation_cost_per_token: finite_number(
                    entry.get("cache_creation_input_token_cost"),
                )
                .unwrap_or(input),
            },
        );
    }
    table
}

/// Build a rate table from user-supplied `modelPrices` overrides.
/// Keys are normalized the same way as LiteLLM keys; malformed entries
/// are dropped, never guessed.
pub fn create_override_rate_table(
    overrides: Option<&HashMap<String, ModelPriceOverride>>,
) -> RateTable {
    let mut table = RateTable::new();
    let Some(overrides) = overrides else {
        return table;
    };
    for (model, prices) in overrides {
        let input = prices
            .input_cost_per_million_tokens
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|v| v / 1_000_000.0);
        let output = prices
            .output_cost_per_million_tokens
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|v| v / 1_000_000.0);
        let (Some(input), Some(output)) = (input, output) else {
            continue;
        };
        let key = normalize_rate_key(model);
        if key.is_empty() {
            continue;
        }
        table.insert(
            key,
            TokenRates {
                input_cost_per_token: input,
                output_cost_per_token: output,
                cache_read_cost_per_token: prices
                    .cache_read_cost_per_million_tokens
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .map(|v| v / 1_000_000.0)
                    .unwrap_or(input),
                cache_creation_cost_per_token: prices
                    .cache_write_cost_per_million_tokens
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .map(|v| v / 1_000_000.0)
                    .unwrap_or(input),
            },
        );
    }
    table
}

// --- model lookup ---------------------------------------------------------------

/// Strip a trailing `[...]` variant suffix (e.g. `[1m]` context tags).
fn strip_variant_suffix(key: &str) -> String {
    match key.find('[') {
        Some(i) => key[..i].to_string(),
        None => key.to_string(),
    }
}

/// Strip a trailing provider date stamp: `-20251101` or `-2025-11-01`.
/// TS: `/-20\d{2}(?:-\d{2}-\d{2}|\d{4})$/`.
fn strip_date_suffix(key: &str) -> String {
    let b = key.as_bytes();
    let n = b.len();
    let digit = |i: usize| b.get(i).map(|c| c.is_ascii_digit()).unwrap_or(false);
    // "-20YYMMDD" — 9 chars: - 2 0 d d d d d d
    if n >= 9
        && b[n - 9] == b'-'
        && b[n - 8] == b'2'
        && b[n - 7] == b'0'
        && (3..9).all(|i| digit(n - 9 + i))
    {
        return key[..n - 9].to_string();
    }
    // "-20YY-MM-DD" — 11 chars: - 2 0 d d - d d - d d
    if n >= 11
        && b[n - 11] == b'-'
        && b[n - 10] == b'2'
        && b[n - 9] == b'0'
        && digit(n - 8)
        && digit(n - 7)
        && b[n - 6] == b'-'
        && digit(n - 5)
        && digit(n - 4)
        && b[n - 3] == b'-'
        && digit(n - 2)
        && digit(n - 1)
    {
        return key[..n - 11].to_string();
    }
    key.to_string()
}

fn bare_model_name(key: &str) -> String {
    match key.rfind('/') {
        Some(i) => key[i + 1..].to_string(),
        None => key.to_string(),
    }
}

/// Resolve a transcript `model` string to a rate-table entry.
/// Candidate order: exact normalized key → bare name (provider prefix
/// dropped) → date-suffix-stripped variants of both. First hit wins;
/// no fuzzy/substring matching — a wrong price is worse than none.
/// Returns None for empty/unpriceable names; callers report the model
/// unpriced rather than guessing.
pub fn lookup_rate(table: &RateTable, model: &str) -> Option<TokenRates> {
    let key = strip_variant_suffix(&normalize_rate_key(model));
    let bare = bare_model_name(&key);
    if bare.is_empty() {
        return None;
    }
    let candidates = [
        key.clone(),
        bare.clone(),
        strip_date_suffix(&key),
        strip_date_suffix(&bare),
    ];
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        if let Some(rate) = table.get(&candidate) {
            return Some(*rate);
        }
    }
    None
}

// --- cost estimation -------------------------------------------------------------

/// Estimated USD cost of one aggregated bucket under `rates`.
/// `thinking_tokens` is intentionally absent: providers report it
/// inside `output_tokens`, so charging it again would double-count.
pub fn estimate_bucket_cost_usd(rates: &TokenRates, bucket: &Value) -> f64 {
    let n = |k: &str| bucket.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    n("input_tokens") * rates.input_cost_per_token
        + n("output_tokens") * rates.output_cost_per_token
        + n("cache_read_input_tokens") * rates.cache_read_cost_per_token
        + n("cache_creation_input_tokens") * rates.cache_creation_cost_per_token
}

// --- table loading (fetch + cache + GC) --------------------------------------------

#[derive(Debug, Clone)]
pub struct PricingInfo {
    pub status: &'static str, // "ok" | "stale" | "unavailable"
    pub source: String,
    pub fetched_at: Option<String>,
    pub cache_hit: bool,
    pub cache_age_seconds: Option<i64>,
    pub ttl_seconds: i64,
    pub known_models: u64,
    pub user_overrides: u64,
    pub detail: Option<String>,
}

impl PricingInfo {
    pub fn to_value(&self) -> Value {
        json!({
            "status": self.status,
            "source": self.source,
            "fetched_at": self.fetched_at,
            "cache": {
                "hit": self.cache_hit,
                "age_seconds": self.cache_age_seconds,
                "ttl_seconds": self.ttl_seconds,
            },
            "known_models": self.known_models,
            "user_overrides": self.user_overrides,
            "detail": self.detail,
        })
    }
}

#[derive(Debug, Default)]
pub struct PricingTable {
    pub info: Option<PricingInfo>,
    /// LiteLLM-derived rates (empty when unavailable).
    pub rates: RateTable,
    /// User override rates — consulted before `rates`.
    pub overrides: RateTable,
}

fn pricing_cache_file(state_dir: &Path) -> PathBuf {
    state_dir.join("pricing-cache").join("litellm.json")
}

fn rates_to_payload(rates: &RateTable, fetched_at: &str, source: &str) -> String {
    let mut rows = serde_json::Map::new();
    for (model, r) in rates {
        rows.insert(
            model.clone(),
            json!([
                r.input_cost_per_token,
                r.output_cost_per_token,
                r.cache_read_cost_per_token,
                r.cache_creation_cost_per_token,
            ]),
        );
    }
    let body = json!({
        "fetched_at": fetched_at,
        "source": source,
        "rates": Value::Object(rows),
    });
    format!("{}\n", serde_json::to_string(&body).unwrap_or_default())
}

struct PricingCacheRead {
    rates: RateTable,
    fetched_at: String,
    age_seconds: i64,
}

fn payload_to_rates(payload: &Value) -> RateTable {
    let mut table = RateTable::new();
    let Some(rows) = payload.get("rates").and_then(|v| v.as_object()) else {
        return table;
    };
    for (model, row) in rows {
        let Some(arr) = row.as_array() else { continue };
        if arr.len() != 4 {
            continue;
        }
        let vals: Vec<Option<f64>> = arr
            .iter()
            .map(|v| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0))
            .collect();
        let [Some(i), Some(o), Some(cr), Some(cc)] = vals.as_slice() else {
            continue;
        };
        table.insert(
            model.clone(),
            TokenRates {
                input_cost_per_token: *i,
                output_cost_per_token: *o,
                cache_read_cost_per_token: *cr,
                cache_creation_cost_per_token: *cc,
            },
        );
    }
    table
}

fn read_pricing_cache(state_dir: &Path) -> Option<PricingCacheRead> {
    let text = fs::read_to_string(pricing_cache_file(state_dir)).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let obj = parsed.as_object()?;
    let fetched_at = obj.get("fetched_at").and_then(|v| v.as_str())?;
    let rows = obj.get("rates").and_then(|v| v.as_object())?;
    let fetched_ms = crate::cli::parse_js_date(fetched_at)?;
    let age_ms = now_ms() - fetched_ms;
    if !age_ms.is_finite() || age_ms < 0.0 {
        return None;
    }
    let rates = payload_to_rates(&parsed);
    if rates.is_empty() {
        return None;
    }
    let _ = rows;
    Some(PricingCacheRead {
        rates,
        fetched_at: fetched_at.to_string(),
        age_seconds: (age_ms / 1000.0).floor() as i64,
    })
}

fn write_pricing_cache(
    state_dir: &Path,
    rates: &RateTable,
    fetched_at: &str,
    source: &str,
) {
    let _ = (|| -> std::io::Result<()> {
        let dir = state_dir.join("pricing-cache");
        fs::create_dir_all(&dir)?;
        let file = pricing_cache_file(state_dir);
        let tmp = PathBuf::from(format!("{}.tmp-{}", file.display(), std::process::id()));
        fs::write(&tmp, rates_to_payload(rates, fetched_at, source))?;
        fs::rename(&tmp, &file)?;
        Ok(())
    })();
    // cache is best-effort; a write failure must not fail the report
}

#[derive(Debug, Default, Clone)]
pub struct PricingOptions {
    /// Bypass the fresh-cache read (still writes on success).
    pub refresh: bool,
    pub timeout_ms: Option<u64>,
    /// Plugin state dir override (tests); default HERDR_PLUGIN_STATE_DIR layout.
    pub state_dir: Option<PathBuf>,
    /// Source URL override (test/proxy hook); env HERDR_USAGE_PRICING_URL also applies upstream.
    pub source_url: Option<String>,
    /// Cache TTL override in seconds (config `pricingCacheTtlSeconds`).
    pub ttl_seconds: Option<i64>,
    /// User price overrides from config.json `modelPrices`.
    pub model_prices: Option<HashMap<String, ModelPriceOverride>>,
}

fn empty_table(
    source: &str,
    status: &'static str,
    detail: Option<String>,
    ttl: i64,
    overrides: RateTable,
) -> PricingTable {
    let user_overrides = overrides.len() as u64;
    PricingTable {
        info: Some(PricingInfo {
            status,
            source: source.to_string(),
            fetched_at: None,
            cache_hit: false,
            cache_age_seconds: None,
            ttl_seconds: ttl,
            known_models: 0,
            user_overrides,
            detail,
        }),
        rates: RateTable::new(),
        overrides,
    }
}

fn env_get(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Load the model rate table: fresh cache → fetch → stale cache →
/// unavailable. Never throws for fetch/parse failures — every outcome
/// is a structured `PricingInfo.status`. Stale cache is served only
/// within the shared retention window; `gc_plugin_state_cache`
/// removes older entries.
pub fn collect_pricing(options: &PricingOptions) -> PricingTable {
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let ttl = options.ttl_seconds.unwrap_or(PRICING_CACHE_TTL_SECONDS);
    let state_dir = options
        .state_dir
        .clone()
        .unwrap_or_else(|| plugin_dirs(PLUGIN_ID, None, None).state_dir);
    let source = options
        .source_url
        .clone()
        .or_else(|| env_get("HERDR_USAGE_PRICING_URL"))
        .unwrap_or_else(|| PRICING_SOURCE_URL.to_string());
    let overrides = create_override_rate_table(options.model_prices.as_ref());
    let user_overrides = overrides.len() as u64;

    // Best-effort retention GC before reading — never fails the report.
    gc_plugin_state_cache(&state_dir, None, None);

    let cached = read_pricing_cache(&state_dir);
    let fresh = cached
        .as_ref()
        .map(|c| c.age_seconds < ttl)
        .unwrap_or(false);
    let within_retention = cached
        .as_ref()
        .map(|c| c.age_seconds < PRICING_CACHE_RETENTION_SECONDS)
        .unwrap_or(false);

    if !options.refresh && fresh {
        if let Some(cached) = cached {
            return PricingTable {
                info: Some(PricingInfo {
                    status: "ok",
                    source: source.clone(),
                    fetched_at: Some(cached.fetched_at.clone()),
                    cache_hit: true,
                    cache_age_seconds: Some(cached.age_seconds),
                    ttl_seconds: ttl,
                    known_models: cached.rates.len() as u64,
                    user_overrides,
                    detail: None,
                }),
                rates: cached.rates,
                overrides,
            };
        }
    }

    let res = fetch_json(
        &source,
        &[("accept".to_string(), "application/json".to_string())],
        "GET",
        None,
        timeout_ms,
    );
    if res.ok {
        let rates = parse_rate_table(&res.json.clone().unwrap_or(Value::Null));
        if !rates.is_empty() {
            let fetched_at = crate::quota::iso_from_ms(now_ms()).unwrap_or_default();
            write_pricing_cache(&state_dir, &rates, &fetched_at, &source);
            return PricingTable {
                info: Some(PricingInfo {
                    status: "ok",
                    source: source.clone(),
                    fetched_at: Some(fetched_at),
                    cache_hit: false,
                    cache_age_seconds: None,
                    ttl_seconds: ttl,
                    known_models: rates.len() as u64,
                    user_overrides,
                    detail: None,
                }),
                rates,
                overrides,
            };
        }
    }

    // Fetch failed or empty: serve the last good table, explicitly
    // stale, within retention — never fabricate an empty-price zero.
    let detail = if res.ok {
        "pricing source returned no usable rate entries".to_string()
    } else {
        res.detail.unwrap_or_else(|| "request failed".to_string())
    };
    if within_retention {
        if let Some(cached) = cached {
            return PricingTable {
                info: Some(PricingInfo {
                    status: "stale",
                    source: source.clone(),
                    fetched_at: Some(cached.fetched_at.clone()),
                    cache_hit: true,
                    cache_age_seconds: Some(cached.age_seconds),
                    ttl_seconds: ttl,
                    known_models: cached.rates.len() as u64,
                    user_overrides,
                    detail: Some(format!(
                        "pricing refresh failed ({}); serving cached table",
                        detail
                    )),
                }),
                rates: cached.rates,
                overrides,
            };
        }
    }
    empty_table(&source, "unavailable", Some(detail), ttl, overrides)
}

/// Lookup that consults user overrides before the fetched table.
pub fn lookup_priced_rate(table: &PricingTable, model: &str) -> Option<(TokenRates, &'static str)> {
    if let Some(rate) = lookup_rate(&table.overrides, model) {
        return Some((rate, "override"));
    }
    if let Some(rate) = lookup_rate(&table.rates, model) {
        return Some((rate, "table"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "claude-test-1": {
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000015,
                "cache_read_input_token_cost": 0.0000003,
                "cache_creation_input_token_cost": 0.00000375,
            },
            "gpt-6-test": {
                "input_cost_per_token": 0.0000025,
                "output_cost_per_token": 0.00001,
            },
            "partial-model": { "input_cost_per_token": 0.000001 },
            "anthropic.claude-opus-5-5-20251101-v1:0": {
                "input_cost_per_token": 0.000005,
                "output_cost_per_token": 0.000025,
            },
            "openai/gpt-6.1-sol-2026-09-29": {
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.000008,
            },
            "gpt-6.1-sol": {
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.000008,
            },
        })
    }

    #[test]
    fn parse_rate_table_keeps_standard_rates_drops_partial() {
        let table = parse_rate_table(&doc());
        assert!(table.contains_key("claude-test-1"));
        assert!(!table.contains_key("partial-model"));
        let gpt = table.get("gpt-6-test").unwrap();
        assert_eq!(gpt.cache_read_cost_per_token, 0.0000025);
        assert_eq!(gpt.cache_creation_cost_per_token, 0.0000025);
        assert!(parse_rate_table(&Value::Null).is_empty());
        assert!(parse_rate_table(&json!("nope")).is_empty());
    }

    #[test]
    fn lookup_rate_resolves_exact_bare_and_date_stripped_only() {
        let table = parse_rate_table(&doc());
        assert_eq!(
            lookup_rate(&table, "Claude-Test-1").unwrap().output_cost_per_token,
            0.000015
        );
        assert_eq!(
            lookup_rate(&table, "anthropic/claude-test-1")
                .unwrap()
                .input_cost_per_token,
            0.000003
        );
        assert!(lookup_rate(&table, "openai/gpt-6.1-sol-2026-10-01").is_some());
        assert!(lookup_rate(&table, "gpt-6.1-sol-2026-09-29").is_some());
        assert!(lookup_rate(&table, "never-heard-of-this-9").is_none());
        assert!(lookup_rate(&table, "").is_none());
        assert!(lookup_rate(&table, "opus").is_none());
    }

    #[test]
    fn override_rate_table_honors_per_million_and_fallbacks() {
        let overrides: HashMap<String, ModelPriceOverride> = [
            (
                "my-model".to_string(),
                ModelPriceOverride {
                    input_cost_per_million_tokens: Some(3.0),
                    output_cost_per_million_tokens: Some(15.0),
                    cache_read_cost_per_million_tokens: None,
                    cache_write_cost_per_million_tokens: Some(3.75),
                },
            ),
            (
                "bad".to_string(),
                ModelPriceOverride {
                    input_cost_per_million_tokens: Some(-1.0),
                    output_cost_per_million_tokens: Some(2.0),
                    cache_read_cost_per_million_tokens: None,
                    cache_write_cost_per_million_tokens: None,
                },
            ),
        ]
        .into_iter()
        .collect();
        let table = create_override_rate_table(Some(&overrides));
        let rate = table.get("my-model").unwrap();
        assert!((rate.input_cost_per_token - 0.000003).abs() < 1e-12);
        assert!((rate.output_cost_per_token - 0.000015).abs() < 1e-12);
        assert!((rate.cache_read_cost_per_token - 0.000003).abs() < 1e-12);
        assert!((rate.cache_creation_cost_per_token - 0.00000375).abs() < 1e-12);
        assert!(!table.contains_key("bad"));
        assert!(create_override_rate_table(None).is_empty());
    }

    #[test]
    fn estimate_bucket_cost_prices_four_buckets_never_thinking() {
        let rate = *parse_rate_table(&doc()).get("claude-test-1").unwrap();
        let bucket = json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 2,
            "thinking_tokens": 99,
        });
        // 10*3e-6 + 5*15e-6 + 3*0.3e-6 + 2*3.75e-6 = 0.0001134
        let cost = estimate_bucket_cost_usd(&rate, &bucket);
        assert!((cost - 0.0001134).abs() < 1e-10);
    }
}
