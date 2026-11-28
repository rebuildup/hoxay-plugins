//! Quota-vs-measured divergence report for rebuildup.usage
//! (Issue #92 / parent #28 T3). Rust port of
//! `plugins/usage/src/divergence.ts` (retired; git history preserves
//! it).
//!
//! Compares subscription-quota windows (provider-reported
//! `used_percent` + reset instants) against transcript-measured token
//! sums. These are different units from different sources covering
//! different scopes (a quota window counts every client on the
//! subscription; the transcript index covers only locally indexed
//! sessions). The report places both sides side by side and labels the
//! comparison `different_units` — it never reconciles, never picks a
//! winner, and never converts percent into tokens or vice versa.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::model::UsageWindow;

/// Fixed honest note attached to every `different_units` comparison.
pub const DIVERGENCE_NOTE: &str = "quota windows report provider-side subscription utilization (used_percent of a resetting window, covering every client on the account); measured totals are transcript token sums over the requested window (local index only). Different units, scopes, and sources — the two are reported side by side and never reconciled into each other.";

fn num(v: Option<&Value>) -> f64 {
    v.and_then(|n| n.as_f64()).unwrap_or(0.0)
}

fn str_or_null(v: Option<&Value>) -> Value {
    v.and_then(|s| s.as_str())
        .map(|s| json!(s))
        .unwrap_or(Value::Null)
}

/// Build one comparison per provider seen on either side. Pure: same
/// input → same output; safe to unit test without I/O.
///
/// `providers` are the coverage provider rows; `measured` is
/// `breakdowns.by_provider`; `quota` is the `collect_quota` output.
pub fn build_divergence(
    providers: &[Value],
    measured: &[Value],
    quota: &[Value],
    window: &UsageWindow,
) -> Vec<Value> {
    // measuredByProvider: key must be a string.
    let mut measured_by_provider: BTreeMap<String, &Value> = BTreeMap::new();
    for b in measured {
        if let Some(key) = b.get("key").and_then(|v| v.as_str()) {
            measured_by_provider.insert(key.to_string(), b);
        }
    }
    let quota_by_provider: BTreeMap<String, &Value> = quota
        .iter()
        .filter_map(|q| {
            q.get("provider")
                .and_then(|v| v.as_str())
                .map(|p| (p.to_string(), q))
        })
        .collect();

    let mut provider_ids: BTreeSet<String> = BTreeSet::new();
    for p in providers {
        if let Some(id) = p.get("provider").and_then(|v| v.as_str()) {
            provider_ids.insert(id.to_string());
        }
    }
    for q in quota {
        if let Some(id) = q.get("provider").and_then(|v| v.as_str()) {
            provider_ids.insert(id.to_string());
        }
    }

    let mut comparisons = Vec::new();
    for provider in provider_ids {
        let measured_entry = measured_by_provider.get(&provider).copied();
        let measured_value = measured_entry.map(|e| {
            json!({
                "events": num(e.get("events")),
                "input_tokens": num(e.get("input_tokens")),
                "output_tokens": num(e.get("output_tokens")),
                "cache_read_input_tokens": num(e.get("cache_read_input_tokens")),
                "cache_creation_input_tokens": num(e.get("cache_creation_input_tokens")),
                "total_tokens": num(e.get("total_tokens")),
            })
        });
        let quota_result = quota_by_provider.get(&provider).copied();
        let quota_ok = quota_result
            .and_then(|q| q.get("status"))
            .and_then(|v| v.as_str())
            == Some("ok");

        let quota_value = quota_result.map(|q| {
            json!({
                "status": str_or_null(q.get("status")),
                "detail": str_or_null(q.get("detail")),
                "plan": str_or_null(q.get("plan")),
                "checked_at": str_or_null(q.get("checked_at")),
                "stale": q.get("stale").and_then(|v| v.as_bool()).unwrap_or(false),
                "windows": q.get("windows").cloned().unwrap_or_else(|| json!([])),
            })
        });

        // verdictFor — detail is always a concrete string (TS falls
        // back to `quota status "x"` / the no-source message).
        let quota_detail = match quota_result {
            Some(q) => q
                .get("detail")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let status = q
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    format!("quota status \"{}\"", status)
                }),
            None => "no quota source registered for this provider".to_string(),
        };
        let (verdict, detail) = if !quota_ok {
            if measured_value.is_none() {
                ("no_data", quota_detail)
            } else {
                ("quota_unavailable", quota_detail)
            }
        } else if measured_value.is_none() {
            (
                "no_measured_usage",
                "quota answered but the transcript index has no usage events for this provider"
                    .to_string(),
            )
        } else {
            ("different_units", DIVERGENCE_NOTE.to_string())
        };
        let detail_value = json!(detail);

        // windowAlignment
        let alignment = window_alignment(
            window,
            if quota_ok {
                quota_result
                    .and_then(|q| q.get("windows"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
        );

        comparisons.push(json!({
            "provider": provider,
            "verdict": verdict,
            "detail": detail_value,
            "quota": quota_value,
            "measured": measured_value,
            "window_alignment": alignment,
        }));
    }
    comparisons
}

fn window_alignment(window: &UsageWindow, quota_windows: Vec<Value>) -> &'static str {
    if window.from_ms.is_none() && window.to_ms.is_none() {
        return "unbounded";
    }
    let (Some(from), Some(to)) = (window.from_ms, window.to_ms) else {
        return "unknown";
    };
    let span_mins = (to - from) / 60_000.0;
    if !span_mins.is_finite() || span_mins <= 0.0 {
        return "unknown";
    }
    let usable: Vec<f64> = quota_windows
        .iter()
        .filter_map(|w| {
            w.get("window_duration_mins")
                .and_then(|v| v.as_f64())
                .filter(|d| d.is_finite() && *d > 0.0)
        })
        .collect();
    if usable.is_empty() {
        return "unknown";
    }
    let tolerance = (2.0f64).max(span_mins * 0.05);
    if usable
        .iter()
        .any(|d| (d - span_mins).abs() <= tolerance)
    {
        "matches_quota_window"
    } else {
        "custom_window"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn measured_entry() -> Value {
        json!({
            "key": "claude", "events": 2, "input_tokens": 30, "output_tokens": 12,
            "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2,
            "total_tokens": 47,
        })
    }

    fn quota_result(partial: Value) -> Value {
        let mut base = json!({
            "provider": "claude", "status": "ok", "detail": null, "plan": "max",
            "windows": [], "checked_at": "2026-01-01T00:30:00.000Z",
            "stale": false,
        });
        if let (Some(b), Some(p)) = (base.as_object_mut(), partial.as_object()) {
            for (k, v) in p {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn window(id: &str, mins: Option<f64>, used: f64) -> Value {
        json!({
            "id": id, "label": id, "kind": "session", "used_percent": used,
            "resets_at": null, "window_duration_mins": mins,
        })
    }

    fn find<'a>(comparisons: &'a [Value], provider: &str) -> &'a Value {
        comparisons
            .iter()
            .find(|c| c.get("provider").and_then(|v| v.as_str()) == Some(provider))
            .unwrap()
    }

    #[test]
    fn both_sides_present_is_different_units_never_reconciled() {
        let comparisons = build_divergence(
            &[json!({"provider": "claude", "provider_status": "supported"})],
            &[measured_entry()],
            &[quota_result(json!({"windows": [window("five_hour", Some(300.0), 40.0)]}))],
            &UsageWindow::unbounded(),
        );
        let claude = find(&comparisons, "claude");
        assert_eq!(claude.get("verdict").unwrap(), &json!("different_units"));
        assert!(claude
            .get("detail")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("never reconciled"));
        assert_eq!(
            claude.pointer("/measured/input_tokens").unwrap(),
            &json!(30.0)
        );
        assert_eq!(
            claude.pointer("/quota/windows/0/used_percent").unwrap(),
            &json!(40.0)
        );
        assert_eq!(claude.get("window_alignment").unwrap(), &json!("unbounded"));
    }

    #[test]
    fn quota_failure_and_empty_index_verdicts() {
        let comparisons = build_divergence(
            &[
                json!({"provider": "claude", "provider_status": "supported"}),
                json!({"provider": "codex", "provider_status": "supported"}),
            ],
            &[measured_entry()],
            &[
                quota_result(json!({"status": "request_failed", "detail": "HTTP 500", "windows": []})),
                quota_result(json!({"provider": "codex", "status": "ok", "windows": [window("primary", Some(300.0), 40.0)]})),
            ],
            &UsageWindow::unbounded(),
        );
        assert_eq!(find(&comparisons, "claude").get("verdict").unwrap(), &json!("quota_unavailable"));
        assert_eq!(find(&comparisons, "claude").get("detail").unwrap(), &json!("HTTP 500"));
        assert_eq!(find(&comparisons, "codex").get("verdict").unwrap(), &json!("no_measured_usage"));
    }

    #[test]
    fn neither_side_no_data_and_window_alignment() {
        let comparisons = build_divergence(
            &[json!({"provider": "opencode", "provider_status": "supported"})],
            &[],
            &[quota_result(json!({
                "provider": "opencode", "status": "unsupported",
                "detail": "no quota artifact", "windows": [],
            }))],
            &UsageWindow {
                from_ms: Some(1767225600000.0),
                to_ms: Some(1767240000000.0), // 4h = 240min… wait 1767240000000-1767225600000=14400000ms=240min
            },
        );
        assert_eq!(find(&comparisons, "opencode").get("verdict").unwrap(), &json!("no_data"));

        // Exactly 300-minute span matches a 300-minute quota window.
        let aligned = build_divergence(
            &[json!({"provider": "claude", "provider_status": "supported"})],
            &[measured_entry()],
            &[quota_result(json!({"windows": [window("five_hour", Some(300.0), 40.0)]}))],
            &UsageWindow {
                from_ms: Some(1767225600000.0),
                to_ms: Some(1767243600000.0), // +18_000_000ms = 300min
            },
        );
        assert_eq!(aligned[0].get("window_alignment").unwrap(), &json!("matches_quota_window"));

        let custom = build_divergence(
            &[json!({"provider": "claude", "provider_status": "supported"})],
            &[measured_entry()],
            &[quota_result(json!({"windows": [window("five_hour", Some(300.0), 40.0)]}))],
            &UsageWindow {
                from_ms: Some(1767225600000.0),
                to_ms: Some(1767312000000.0), // +86400000ms = 1440min
            },
        );
        assert_eq!(custom[0].get("window_alignment").unwrap(), &json!("custom_window"));
    }
}
