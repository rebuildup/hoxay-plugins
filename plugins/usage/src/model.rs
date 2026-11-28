//! Shared model constants/types for rebuildup.usage
//! (`herdr-plugins.usage/v1`, Issue #90 / parent #28).
//!
//! Rust port of `plugins/usage/src/model.ts` (retired; git history
//! preserves it). Payloads are built as `serde_json::Value` documents
//! mirroring the TS object shapes field-for-field; the typed structs
//! below exist only where a shape is serialized to plugin cache files
//! or threaded through pure functions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CONTRACT: &str = "herdr-plugins.usage/v1";
pub const CONVERSATIONS_CONTRACT: &str = "herdr-plugins.conversations/v1";
pub const CONVERSATIONS_PLUGIN_ID: &str = "rebuildup.conversations";
pub const PLUGIN_ID: &str = "rebuildup.usage";
pub const PLUGIN_VERSION: &str = "0.6.0";

/// Aggregation window; `None` bound = open-ended. Instants in ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageWindow {
    pub from_ms: Option<f64>,
    pub to_ms: Option<f64>,
}

impl UsageWindow {
    pub fn unbounded() -> Self {
        UsageWindow {
            from_ms: None,
            to_ms: None,
        }
    }
    pub fn is_bounded(&self) -> bool {
        self.from_ms.is_some() || self.to_ms.is_some()
    }
}

/// One provider-reported quota/reset window (serialized into the
/// quota/hub cache files and emitted verbatim in payloads).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub used_percent: f64,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub window_duration_mins: Option<f64>,
}

impl QuotaWindow {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Serializable quota cache payload (`quota-cache/<provider>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaCachePayload {
    pub checked_at: String,
    #[serde(default)]
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub reset_credits_available: Option<f64>,
}

/// One hub account row inside `quota-cache/hub-<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCacheAccount {
    pub account: String,
    pub provider: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub reset_credits_available: Option<f64>,
}

/// Serializable hub cache payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCachePayload {
    pub checked_at: String,
    #[serde(default)]
    pub accounts: Vec<HubCacheAccount>,
}

/// User-supplied price override (config.json `modelPrices`), USD per
/// million tokens — the same shape T3 Code exposes.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPriceOverride {
    #[serde(rename = "inputCostPerMillionTokens", default)]
    pub input_cost_per_million_tokens: Option<f64>,
    #[serde(rename = "outputCostPerMillionTokens", default)]
    pub output_cost_per_million_tokens: Option<f64>,
    #[serde(rename = "cacheReadCostPerMillionTokens", default)]
    pub cache_read_cost_per_million_tokens: Option<f64>,
    #[serde(rename = "cacheWriteCostPerMillionTokens", default)]
    pub cache_write_cost_per_million_tokens: Option<f64>,
}

/// A configured quota hub (CLIProxyAPI-compatible management API).
/// `url` is not secret; the management key is NEVER stored here —
/// `keyEnv` names the environment variable that holds it (ADR-0015).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotaHubConfig {
    pub id: String,
    pub url: String,
    #[serde(rename = "keyEnv", default)]
    pub key_env: Option<String>,
}

/// Optional overrides for locating the conversations plugin
/// (`SourceOptions` in the TS model).
#[derive(Debug, Clone, Default)]
pub struct SourceOptions {
    pub conversations_root: Option<String>,
    pub conversations_state_dir: Option<String>,
    pub conversations_config_dir: Option<String>,
}

/// Endpoint overrides (test hooks / future proxies); full URLs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuotaEndpoints {
    pub claude: Option<String>,
    #[serde(rename = "codexUsage", default)]
    pub codex_usage: Option<String>,
    #[serde(rename = "codexCredits", default)]
    pub codex_credits: Option<String>,
}
