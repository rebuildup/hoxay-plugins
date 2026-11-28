//! Plugin state retention / GC for rebuildup.usage (Issue #93 /
//! parent #28 T4). Rust port of `plugins/usage/src/state.ts` (retired;
//! git history preserves it).
//!
//! The plugin's only durable state is short-lived caches under
//! `HERDR_PLUGIN_STATE_DIR`:
//!
//! | path | contents | fresh TTL | retention |
//! | --- | --- | --- | --- |
//! | `quota-cache/<provider>.json` | provider quota windows (quota numbers only — never credentials, ADR-0014) | `quotaCacheTtlSeconds` (default 300s) | `PLUGIN_CACHE_RETENTION_SECONDS` |
//! | `quota-cache/hub-<id>.json` | hub account quota windows (ADR-0015) | same | same |
//! | `pricing-cache/litellm.json` | parsed LiteLLM rate table (public prices, no secrets) | `pricingCacheTtlSeconds` (default 24h) | same |
//!
//! `usage` itself keeps **no** aggregate index — totals are recomputed
//! from the conversations index on every call. Retention rule: a cache
//! entry is a *referenced* reading while its payload timestamp is
//! younger than the retention window; GC deletes only entries **older**
//! than retention (plus orphaned `.tmp-*` write temps and unparseable
//! files, which no reader can reference). Reads additionally refuse to
//! serve entries past retention, so a failed GC can never resurrect
//! ancient data.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Hard retention for every plugin cache entry, in seconds (30 days).
pub const PLUGIN_CACHE_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Cache subdirectories owned by this plugin.
const CACHE_DIRS: [&str; 2] = ["quota-cache", "pricing-cache"];

#[derive(Debug, Default)]
pub struct CacheGcResult {
    /// Cache files examined across all cache dirs.
    pub scanned: u64,
    /// Paths removed (absolute).
    pub removed: Vec<PathBuf>,
}

fn payload_timestamp_ms(parsed: &Value) -> Option<f64> {
    let raw = parsed
        .get("checked_at")
        .or_else(|| parsed.get("fetched_at"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    crate::cli::parse_js_date(raw)
}

/// Remove expired cache entries under `state_dir`. Best-effort and
/// non-throwing: GC must never fail a report. Entries at or under
/// `retention_seconds` old are always kept — referenced data is never
/// deleted.
pub fn gc_plugin_state_cache(
    state_dir: &Path,
    retention_seconds: Option<i64>,
    now_ms: Option<f64>,
) -> CacheGcResult {
    let retention = retention_seconds.unwrap_or(PLUGIN_CACHE_RETENTION_SECONDS) as f64;
    let now_ms = now_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    });
    let mut result = CacheGcResult::default();

    for dir_name in CACHE_DIRS {
        let dir = state_dir.join(dir_name);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue; // cache dir absent — nothing to collect
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let file = entry.path();
            // Orphaned atomic-write temps are never referenced.
            if name.contains(".tmp-") {
                result.scanned += 1;
                if fs::remove_file(&file).is_ok() {
                    result.removed.push(file);
                }
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }
            result.scanned += 1;
            let mut expired = true;
            if let Ok(text) = fs::read_to_string(&file) {
                if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                    match payload_timestamp_ms(&parsed) {
                        // Unparseable or timestamp-less files cannot be
                        // referenced by any reader — safe to collect.
                        None => expired = true,
                        Some(ts) => expired = now_ms - ts > retention * 1000.0,
                    }
                }
            }
            if !expired {
                continue;
            }
            if fs::remove_file(&file).is_ok() {
                result.removed.push(file);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write_cache(state_dir: &Path, rel: &str, payload: &Value) -> PathBuf {
        let file = state_dir.join(rel);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, serde_json::to_string(payload).unwrap()).unwrap();
        file
    }

    #[test]
    fn gc_keeps_referenced_removes_expired_tmp_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        let now = crate::quota::now_ms();
        let iso = |ms: f64| crate::quota::iso_from_ms(ms).unwrap();
        let fresh = iso(now - 3_600_000.0);
        let expired = iso(now - (PLUGIN_CACHE_RETENTION_SECONDS as f64 + 3600.0) * 1000.0);

        let keep_quota = write_cache(state_dir, "quota-cache/claude.json", &json!({"checked_at": fresh, "windows": []}));
        let keep_hub = write_cache(state_dir, "quota-cache/hub-home.json", &json!({"checked_at": fresh, "accounts": []}));
        let keep_pricing = write_cache(state_dir, "pricing-cache/litellm.json", &json!({"fetched_at": fresh, "rates": {}}));
        let drop_quota = write_cache(state_dir, "quota-cache/codex.json", &json!({"checked_at": expired, "windows": []}));
        let drop_hub = write_cache(state_dir, "quota-cache/hub-old.json", &json!({"checked_at": expired, "accounts": []}));
        let drop_pricing = write_cache(state_dir, "pricing-cache/litellm-old.json", &json!({"fetched_at": expired, "rates": {}}));
        let tmp = write_cache(state_dir, "quota-cache/claude.json.tmp-123", &json!({"checked_at": fresh}));
        let corrupt = state_dir.join("quota-cache/broken.json");
        fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        fs::write(&corrupt, "{not json").unwrap();

        let result = gc_plugin_state_cache(state_dir, None, None);
        for kept in [&keep_quota, &keep_hub, &keep_pricing] {
            assert!(kept.exists(), "kept file removed: {:?}", kept);
        }
        for dropped in [&drop_quota, &drop_hub, &drop_pricing, &tmp, &corrupt] {
            assert!(!dropped.exists(), "expired file kept: {:?}", dropped);
        }
        assert_eq!(result.removed.len(), 5);
    }

    #[test]
    fn gc_boundary_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path();
        let now = crate::quota::now_ms();
        // A few seconds inside the retention window.
        let edge = crate::quota::iso_from_ms(
            now - (PLUGIN_CACHE_RETENTION_SECONDS as f64 - 60.0) * 1000.0,
        )
        .unwrap();
        let file = write_cache(
            state_dir,
            "quota-cache/claude.json",
            &json!({"checked_at": edge, "windows": []}),
        );
        gc_plugin_state_cache(state_dir, None, None);
        assert!(file.exists());
    }
}
