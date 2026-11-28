//! Pure aggregation core for rebuildup.usage.
//!
//! Rust port of `plugins/usage/src/aggregate.ts` (retired; git history
//! preserves it). Consumes the raw usage-carrying events exported by
//! `rebuildup.conversations usage` plus the session registry entries the
//! same command reports, and produces totals + dimension breakdowns +
//! honest coverage. No I/O lives here — every rule that can fabricate a
//! number is decided in this file and unit-tested directly.
//!
//! Honesty rules (contract `herdr-plugins.usage/v1`):
//! - Only provider-reported `usage` objects are summed; a session with
//!   no usage events contributes zero events, not zeroed "measurement".
//! - Duplicate `(session_key, event_id)` pairs are skipped and counted —
//!   index rewrites (opencode rescan) can replay events mid-scan.
//! - Events without a usable timestamp are excluded when a `--from`/`--to`
//!   window is set (counted), included otherwise (counted separately).
//! - `thinking_tokens` / `provider_reported_cost_usd` stay `null` unless
//!   at least one contributing event reported them — missing is never
//!   folded into a fabricated zero.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::model::UsageWindow;

/// One usage-carrying event as exported by `conversations usage`.
#[derive(Debug, Clone)]
pub struct UsageEventRecord {
    pub session_key: String,
    pub event: Value,
}

#[derive(Debug, Default, Clone)]
struct MutableBucket {
    key: Option<String>,
    events: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    thinking_tokens: u64,
    thinking_seen: bool,
    total_tokens: u64,
    cost_usd: f64,
    cost_seen: bool,
}

/// JS `Number.isFinite(v) && v > 0 → Math.floor(v)` token rule.
fn token_count(value: Option<&Value>) -> u64 {
    match value.and_then(|v| v.as_f64()) {
        Some(f) if f.is_finite() && f > 0.0 => f.floor() as u64,
        _ => 0,
    }
}

struct UsageFields {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
    thinking: Option<u64>,
}

/// Parse the provider `usage` object; None when absent/malformed.
fn usage_fields(raw: Option<&Value>) -> Option<UsageFields> {
    let u = raw?.as_object()?;
    let input = token_count(u.get("input_tokens"));
    let output = token_count(u.get("output_tokens"));
    let cache_read = token_count(u.get("cache_read_input_tokens"));
    let cache_create = token_count(u.get("cache_creation_input_tokens"));
    let thinking = match u.get("thinking_tokens").and_then(|v| v.as_f64()) {
        Some(f) if f.is_finite() && f > 0.0 => Some(f.floor() as u64),
        _ => None,
    };
    if input + output + cache_read + cache_create + thinking.unwrap_or(0) == 0 {
        return None;
    }
    Some(UsageFields {
        input,
        output,
        cache_read,
        cache_create,
        thinking,
    })
}

/// JS `Date.parse` on an ISO timestamp → ms, or None.
pub fn parse_timestamp_ms(raw: Option<&Value>) -> Option<f64> {
    let s = raw?.as_str()?;
    if s.is_empty() {
        return None;
    }
    crate::cli::parse_js_date(s)
}

fn in_window(ts_ms: f64, window: &UsageWindow) -> bool {
    if let Some(from) = window.from_ms {
        if ts_ms < from {
            return false;
        }
    }
    if let Some(to) = window.to_ms {
        if ts_ms > to {
            return false;
        }
    }
    true
}

fn non_empty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn add_to_bucket(bucket: &mut MutableBucket, fields: &UsageFields, cost: Option<f64>) {
    bucket.events += 1;
    bucket.input_tokens += fields.input;
    bucket.output_tokens += fields.output;
    bucket.cache_read_input_tokens += fields.cache_read;
    bucket.cache_creation_input_tokens += fields.cache_create;
    bucket.total_tokens += fields.input + fields.output + fields.cache_read + fields.cache_create;
    if let Some(t) = fields.thinking {
        bucket.thinking_tokens += t;
        bucket.thinking_seen = true;
    }
    if let Some(c) = cost {
        bucket.cost_usd += c;
        bucket.cost_seen = true;
    }
}

fn freeze_bucket(bucket: &MutableBucket) -> Value {
    json!({
        "key": bucket.key,
        "events": bucket.events,
        "input_tokens": bucket.input_tokens,
        "output_tokens": bucket.output_tokens,
        "cache_read_input_tokens": bucket.cache_read_input_tokens,
        "cache_creation_input_tokens": bucket.cache_creation_input_tokens,
        "thinking_tokens": if bucket.thinking_seen { json!(bucket.thinking_tokens) } else { Value::Null },
        "total_tokens": bucket.total_tokens,
        "provider_reported_cost_usd": if bucket.cost_seen { json!(bucket.cost_usd) } else { Value::Null },
    })
}

/// Insertion-ordered bucket map mirroring TS `Map<string, MutableBucket>`
/// + stable sort by (total_tokens desc, events desc).
#[derive(Default)]
struct BucketMap {
    order: Vec<String>, // "" sentinel encodes the null key
    map: HashMap<String, MutableBucket>,
}

impl BucketMap {
    fn bucket(&mut self, key: Option<&str>) -> &mut MutableBucket {
        let map_key = key.unwrap_or("").to_string();
        if !self.map.contains_key(&map_key) {
            self.order.push(map_key.clone());
            self.map.insert(
                map_key.clone(),
                MutableBucket {
                    key: key.map(|s| s.to_string()),
                    ..Default::default()
                },
            );
        }
        self.map.get_mut(&map_key).unwrap()
    }
    fn sorted_entries(&self) -> Vec<Value> {
        let mut entries: Vec<&MutableBucket> =
            self.order.iter().filter_map(|k| self.map.get(k)).collect();
        entries.sort_by(|a, b| {
            b.total_tokens
                .cmp(&a.total_tokens)
                .then_with(|| b.events.cmp(&a.events))
        });
        entries.iter().map(|b| freeze_bucket(b)).collect()
    }
}

pub struct AggregateInput<'a> {
    pub events: &'a [UsageEventRecord],
    /// Session registry entries (conversations `sessions` payload rows).
    pub sessions: &'a [Value],
    pub window: UsageWindow,
    /// Provider support status per provider id (from conversations capability).
    pub provider_status: &'a HashMap<String, String>,
}

fn session_str<'a>(s: &'a Value, key: &str) -> Option<&'a str> {
    s.get(key).and_then(|v| v.as_str())
}

/// Aggregate exported usage events into totals, breakdowns, and coverage.
/// Pure: same input → same output; safe to unit test without I/O.
pub fn aggregate_usage(input: &AggregateInput) -> Value {
    let window = input.window;
    let windowed = window.is_bounded();

    let mut seen: HashSet<String> = HashSet::new();
    let mut totals = Map::new();
    totals.insert("events".to_string(), json!(0u64));
    totals.insert("sessions_with_usage".to_string(), json!(0u64));
    for k in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "total_tokens",
        "thinking_token_events",
        "provider_reported_cost_events",
        "excluded_untimestamped_events",
        "duplicate_events_skipped",
        "events_without_event_id",
    ] {
        totals.insert(k.to_string(), json!(0u64));
    }
    totals.insert("thinking_tokens".to_string(), Value::Null);
    totals.insert("provider_reported_cost_usd".to_string(), Value::Null);

    let bump = |totals: &mut Map<String, Value>, key: &str, by: u64| {
        let cur = totals.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        totals.insert(key.to_string(), json!(cur + by));
    };

    let mut by_provider = BucketMap::default();
    let mut by_model = BucketMap::default();
    let mut by_host = BucketMap::default();
    let mut by_project = BucketMap::default();
    let mut by_session = BucketMap::default();
    let mut by_pane = BucketMap::default();
    let mut sessions_with_usage: HashSet<String> = HashSet::new();
    let session_by_key: HashMap<String, &Value> = input
        .sessions
        .iter()
        .filter_map(|s| session_str(s, "key").map(|k| (k.to_string(), s)))
        .collect();

    for record in input.events {
        let Some(fields) = usage_fields(record.event.get("usage")) else {
            continue; // defensive: source already filters
        };

        // Dedup on (session_key, event_id); missing ids cannot be deduped.
        let event_id = non_empty_str(record.event.get("event_id"));
        if let Some(eid) = &event_id {
            let dedup_key = format!("{} {}", record.session_key, eid);
            if !seen.insert(dedup_key) {
                bump(&mut totals, "duplicate_events_skipped", 1);
                continue;
            }
        } else {
            bump(&mut totals, "events_without_event_id", 1);
        }

        // Time window: untimestamped events cannot be placed — excluded
        // (counted) when a window is set, included otherwise.
        let ts = parse_timestamp_ms(record.event.get("timestamp"));
        match ts {
            None => {
                if windowed {
                    bump(&mut totals, "excluded_untimestamped_events", 1);
                    continue;
                }
            }
            Some(ms) => {
                if windowed && !in_window(ms, &window) {
                    continue;
                }
            }
        }

        let identity = record.event.get("identity");
        let host = identity.and_then(|i| non_empty_str(i.get("host")));
        let pane_id = identity.and_then(|i| non_empty_str(i.get("pane_id")));
        let model = non_empty_str(record.event.get("model"));
        let provider = if record.session_key.contains(':') {
            record.session_key.split(':').next().map(|s| s.to_string())
        } else {
            None
        };
        let project = session_by_key
            .get(&record.session_key)
            .and_then(|s| non_empty_str(s.get("cwd")));
        let cost = match record.event.get("cost_usd").and_then(|v| v.as_f64()) {
            Some(f) if f.is_finite() && f >= 0.0 => Some(f),
            _ => None,
        };

        bump(&mut totals, "events", 1);
        bump(&mut totals, "input_tokens", fields.input);
        bump(&mut totals, "output_tokens", fields.output);
        bump(&mut totals, "cache_read_input_tokens", fields.cache_read);
        bump(&mut totals, "cache_creation_input_tokens", fields.cache_create);
        bump(
            &mut totals,
            "total_tokens",
            fields.input + fields.output + fields.cache_read + fields.cache_create,
        );
        if let Some(t) = fields.thinking {
            bump(&mut totals, "thinking_token_events", 1);
            let cur = totals
                .get("thinking_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            totals.insert("thinking_tokens".to_string(), json!(cur + t));
        }
        if let Some(c) = cost {
            bump(&mut totals, "provider_reported_cost_events", 1);
            let cur = totals
                .get("provider_reported_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            totals.insert("provider_reported_cost_usd".to_string(), json!(cur + c));
        }
        sessions_with_usage.insert(record.session_key.clone());

        add_to_bucket(by_provider.bucket(provider.as_deref()), &fields, cost);
        add_to_bucket(by_model.bucket(model.as_deref()), &fields, cost);
        add_to_bucket(by_host.bucket(host.as_deref()), &fields, cost);
        add_to_bucket(by_project.bucket(project.as_deref()), &fields, cost);
        add_to_bucket(
            by_session.bucket(Some(&record.session_key)),
            &fields,
            cost,
        );
        add_to_bucket(by_pane.bucket(pane_id.as_deref()), &fields, cost);
    }

    let sessions_with_usage_count = sessions_with_usage.len() as u64;
    totals.insert(
        "sessions_with_usage".to_string(),
        json!(sessions_with_usage_count),
    );

    // Coverage: registry truth, not measured truth. A session with no
    // usage events is still listed — "no measured usage" is not "zero".
    struct ProviderCoverage {
        provider: String,
        provider_status: String,
        sessions: u64,
        sessions_ok: u64,
        sessions_with_issues: u64,
        usage_events: u64,
    }
    let mut provider_map: HashMap<String, ProviderCoverage> = HashMap::new();
    let mut provider_order: Vec<String> = Vec::new();
    let (mut ok, mut issues, mut stale, mut events_missing) = (0u64, 0u64, 0u64, 0u64);

    let contributing: Vec<&Value> = input
        .sessions
        .iter()
        .filter(|s| {
            session_str(s, "key")
                .map(|k| sessions_with_usage.contains(k))
                .unwrap_or(false)
        })
        .collect();

    for session in input.sessions {
        let status = session_str(session, "status").unwrap_or("unknown");
        let events_present = session.get("events_present").and_then(|v| v.as_bool());
        let has_error = session.get("error").map(|e| !e.is_null()).unwrap_or(false);
        let has_issue = status != "ok" || events_present == Some(false) || has_error;
        if has_issue {
            issues += 1;
        } else {
            ok += 1;
        }
        if session.get("stale").and_then(|v| v.as_bool()) == Some(true) {
            stale += 1;
        }
        if events_present == Some(false) {
            events_missing += 1;
        }

        let provider = session_str(session, "provider").unwrap_or("unknown").to_string();
        if !provider_map.contains_key(&provider) {
            provider_order.push(provider.clone());
            let provider_status = input
                .provider_status
                .get(&provider)
                .cloned()
                .or_else(|| session_str(session, "provider_status").map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown".to_string());
            provider_map.insert(
                provider.clone(),
                ProviderCoverage {
                    provider: provider.clone(),
                    provider_status,
                    sessions: 0,
                    sessions_ok: 0,
                    sessions_with_issues: 0,
                    usage_events: 0,
                },
            );
        }
        let entry = provider_map.get_mut(&provider).unwrap();
        entry.sessions += 1;
        if has_issue {
            entry.sessions_with_issues += 1;
        } else {
            entry.sessions_ok += 1;
        }
    }
    for key in &sessions_with_usage {
        let provider = if key.contains(':') {
            key.split(':').next().unwrap_or("unknown").to_string()
        } else {
            "unknown".to_string()
        };
        if let Some(entry) = provider_map.get_mut(&provider) {
            entry.usage_events += 1;
        }
    }

    let mut providers: Vec<Value> = provider_order
        .iter()
        .filter_map(|p| provider_map.get(p))
        .map(|p| {
            json!({
                "provider": p.provider,
                "provider_status": p.provider_status,
                "sessions": p.sessions,
                "sessions_ok": p.sessions_ok,
                "sessions_with_issues": p.sessions_with_issues,
                "usage_events": p.usage_events,
            })
        })
        .collect();
    providers.sort_by(|a, b| {
        let au = a.get("usage_events").and_then(|v| v.as_u64()).unwrap_or(0);
        let bu = b.get("usage_events").and_then(|v| v.as_u64()).unwrap_or(0);
        bu.cmp(&au).then_with(|| {
            let ap = a.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let bp = b.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            ap.cmp(bp)
        })
    });

    json!({
        "totals": Value::Object(totals),
        "breakdowns": {
            "by_provider": by_provider.sorted_entries(),
            "by_model": by_model.sorted_entries(),
            "by_host": by_host.sorted_entries(),
            "by_project": by_project.sorted_entries(),
            "by_session": by_session.sorted_entries(),
            "by_pane": by_pane.sorted_entries(),
        },
        "coverage": {
            "sessions_total": input.sessions.len() as u64,
            "sessions_ok": ok,
            "sessions_with_issues": issues,
            "sessions_stale": stale,
            "sessions_events_missing": events_missing,
            "providers": providers,
        },
        "sessions": contributing,
    })
}

// --- cache token breakdown (T3 / #92) -------------------------------------------

/// Whether a provider's cache-token accounting can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTokenStatus {
    Reported,
    Unsupported,
    Unknown,
}

impl CacheTokenStatus {
    fn as_str(&self) -> &'static str {
        match self {
            CacheTokenStatus::Reported => "reported",
            CacheTokenStatus::Unsupported => "unsupported",
            CacheTokenStatus::Unknown => "unknown",
        }
    }
}

fn cache_status_for(
    provider: &str,
    provider_status: Option<&str>,
    capability: Option<&str>,
) -> (CacheTokenStatus, Option<String>) {
    if provider_status == Some("unsupported") {
        return (
            CacheTokenStatus::Unsupported,
            Some("provider adapter is unsupported; no usage events can be indexed".to_string()),
        );
    }
    if capability == Some("unsupported") {
        return (
            CacheTokenStatus::Unsupported,
            Some("provider adapter declares no cache-token accounting".to_string()),
        );
    }
    if provider_status == Some("supported") && capability == Some("reported") {
        return (CacheTokenStatus::Reported, None);
    }
    (
        CacheTokenStatus::Unknown,
        Some(format!(
            "cache-token capability not verified for provider \"{}\"",
            provider
        )),
    )
}

fn num_field(v: Option<&Value>) -> u64 {
    v.and_then(|n| n.as_f64())
        .filter(|f| f.is_finite())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// Per-provider cache-token view. `providers` are the coverage provider
/// rows; `measured` is `breakdowns.by_provider`; `cache_capability` is
/// the adapter-declared `usage.cache_tokens` map.
pub fn build_cache_tokens(
    providers: &[Value],
    measured: &[Value],
    cache_capability: &HashMap<String, String>,
) -> Value {
    let measured_by_provider: HashMap<String, &Value> = measured
        .iter()
        .filter_map(|b| {
            b.get("key")
                .and_then(|k| k.as_str())
                .map(|k| (k.to_string(), b))
        })
        .collect();

    let mut out: Vec<Value> = Vec::new();
    for p in providers {
        let provider = p
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let measured_entry = measured_by_provider.get(&provider).copied();
        let (status, detail) = cache_status_for(
            &provider,
            p.get("provider_status").and_then(|v| v.as_str()),
            cache_capability.get(&provider).map(|s| s.as_str()),
        );
        let get = |k: &str| measured_entry.and_then(|m| m.get(k));
        out.push(json!({
            "provider": provider,
            "status": status.as_str(),
            "events": num_field(get("events")),
            "cache_read_input_tokens": num_field(get("cache_read_input_tokens")),
            "cache_creation_input_tokens": num_field(get("cache_creation_input_tokens")),
            "detail": detail,
        }));
    }
    out.sort_by(|a, b| {
        let ae = a.get("events").and_then(|v| v.as_u64()).unwrap_or(0);
        let be = b.get("events").and_then(|v| v.as_u64()).unwrap_or(0);
        be.cmp(&ae).then_with(|| {
            let ap = a.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            let bp = b.get("provider").and_then(|v| v.as_str()).unwrap_or("");
            ap.cmp(bp)
        })
    });
    json!({ "providers": out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn sessions() -> Vec<Value> {
        vec![
            json!({"key": "claude:s1", "provider": "claude", "cwd": "/repo/a", "status": "ok", "events_present": true}),
            json!({"key": "codex:s2", "provider": "codex", "cwd": "/repo/b", "status": "ok", "events_present": true}),
            json!({"key": "claude:s3", "provider": "claude", "cwd": null, "status": "missing", "events_present": false}),
        ]
    }

    fn ev(session_key: &str, event_id: &str, usage: Value, extra: Value) -> UsageEventRecord {
        let mut event = json!({
            "event_id": event_id,
            "type": "assistant_message",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "model": "m1",
            "usage": usage,
            "identity": {"host": "h1", "pane_id": "p1"},
        });
        if let (Some(e), Some(x)) = (event.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                e.insert(k.clone(), v.clone());
            }
        }
        UsageEventRecord {
            session_key: session_key.to_string(),
            event,
        }
    }

    fn run(events: &[UsageEventRecord], window: UsageWindow) -> Value {
        aggregate_usage(&AggregateInput {
            events,
            sessions: &sessions(),
            window,
            provider_status: &HashMap::new(),
        })
    }

    #[test]
    fn sums_tokens_dedups_and_breaks_down_dimensions() {
        let result = run(
            &[
                ev("claude:s1", "e1", json!({"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 3}), json!({})),
                ev("claude:s1", "e1", json!({"input_tokens": 10, "output_tokens": 5}), json!({})),
                ev("claude:s1", "e2", json!({"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 2, "thinking_tokens": 4}), json!({})),
                ev("codex:s2", "e9", json!({"input_tokens": 100, "output_tokens": 10}), json!({"model": "gpt-x"})),
            ],
            UsageWindow::unbounded(),
        );
        let totals = result.get("totals").unwrap();
        assert_eq!(totals.get("events").unwrap(), &json!(3));
        assert_eq!(totals.get("duplicate_events_skipped").unwrap(), &json!(1));
        assert_eq!(totals.get("input_tokens").unwrap(), &json!(111));
        assert_eq!(totals.get("output_tokens").unwrap(), &json!(16));
        assert_eq!(totals.get("cache_read_input_tokens").unwrap(), &json!(3));
        assert_eq!(totals.get("cache_creation_input_tokens").unwrap(), &json!(2));
        assert_eq!(totals.get("thinking_tokens").unwrap(), &json!(4));
        assert!(totals.get("provider_reported_cost_usd").unwrap().is_null());

        let breakdowns = result.get("breakdowns").unwrap();
        let models: Vec<&str> = breakdowns
            .get("by_model")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
            .collect();
        let mut models = models;
        models.sort();
        assert_eq!(models, vec!["gpt-x", "m1"]);

        let projects: Vec<&str> = breakdowns
            .get("by_project")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
            .collect();
        let mut projects = projects;
        projects.sort();
        assert_eq!(projects, vec!["/repo/a", "/repo/b"]);

        let coverage = result.get("coverage").unwrap();
        assert_eq!(coverage.get("sessions_total").unwrap(), &json!(3));
        assert_eq!(coverage.get("sessions_with_issues").unwrap(), &json!(1));
        let claude = coverage
            .get("providers")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p.get("provider").and_then(|v| v.as_str()) == Some("claude"))
            .unwrap();
        assert_eq!(claude.get("sessions").unwrap(), &json!(2));
    }

    #[test]
    fn window_excludes_out_of_range_and_untimestamped_events_honestly() {
        let window = UsageWindow {
            from_ms: Some(1767225600000.0), // 2026-01-01T00:00:00Z
            to_ms: Some(1767229200000.0),   // 2026-01-01T01:00:00Z
        };
        let result = run(
            &[
                ev("claude:s1", "e1", json!({"input_tokens": 10, "output_tokens": 5}), json!({})),
                ev("claude:s1", "e2", json!({"input_tokens": 99, "output_tokens": 99}), json!({"timestamp": "2026-06-01T00:00:00.000Z"})),
                ev("claude:s1", "e3", json!({"input_tokens": 50, "output_tokens": 50}), json!({"timestamp": null})),
            ],
            window,
        );
        let totals = result.get("totals").unwrap();
        assert_eq!(totals.get("events").unwrap(), &json!(1));
        assert_eq!(totals.get("input_tokens").unwrap(), &json!(10));
        assert_eq!(totals.get("excluded_untimestamped_events").unwrap(), &json!(1));

        let open = run(
            &[ev("claude:s1", "e3", json!({"input_tokens": 50, "output_tokens": 50}), json!({"timestamp": null}))],
            UsageWindow::unbounded(),
        );
        let ot = open.get("totals").unwrap();
        assert_eq!(ot.get("events").unwrap(), &json!(1));
        assert_eq!(ot.get("excluded_untimestamped_events").unwrap(), &json!(0));
    }

    #[test]
    fn missing_usage_fields_stay_null_never_zero_fabricated() {
        let result = run(
            &[
                ev("claude:s1", "e1", json!({"input_tokens": 5, "output_tokens": 5}), json!({})),
                UsageEventRecord {
                    session_key: "claude:s1".to_string(),
                    event: json!({"event_id": "e2", "usage": null}),
                },
                ev("claude:s1", "e3", json!({"input_tokens": 0, "output_tokens": 0}), json!({})),
            ],
            UsageWindow::unbounded(),
        );
        let totals = result.get("totals").unwrap();
        assert_eq!(totals.get("events").unwrap(), &json!(1));
        assert!(totals.get("thinking_tokens").unwrap().is_null());
        assert!(totals.get("provider_reported_cost_usd").unwrap().is_null());
        assert_eq!(totals.get("events_without_event_id").unwrap(), &json!(0));
    }

    #[test]
    fn cache_token_statuses_come_from_capability_not_zeros() {
        let providers = vec![
            json!({"provider": "claude", "provider_status": "supported"}),
            json!({"provider": "codex", "provider_status": "supported"}),
            json!({"provider": "future-prop", "provider_status": "supported"}),
            json!({"provider": "dead-prop", "provider_status": "unsupported"}),
        ];
        let measured = vec![
            json!({"key": "claude", "events": 2, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2}),
            json!({"key": "codex", "events": 1, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 6}),
        ];
        let capability: HashMap<String, String> = [
            ("claude".to_string(), "reported".to_string()),
            ("codex".to_string(), "reported".to_string()),
            ("dead-prop".to_string(), "reported".to_string()),
        ]
        .into_iter()
        .collect();
        let result = build_cache_tokens(&providers, &measured, &capability);
        let rows = result.get("providers").unwrap().as_array().unwrap();
        let get = |p: &str| rows.iter().find(|r| r.get("provider").and_then(|v| v.as_str()) == Some(p)).unwrap().clone();
        assert_eq!(get("claude").get("status").unwrap(), &json!("reported"));
        assert_eq!(get("claude").get("cache_read_input_tokens").unwrap(), &json!(3));
        assert_eq!(get("codex").get("cache_creation_input_tokens").unwrap(), &json!(6));
        assert_eq!(get("future-prop").get("status").unwrap(), &json!("unknown"));
        assert_eq!(get("future-prop").get("events").unwrap(), &json!(0));
        assert_eq!(get("dead-prop").get("status").unwrap(), &json!("unsupported"));
        assert!(get("dead-prop").get("detail").unwrap().as_str().unwrap().contains("unsupported"));
    }

    #[test]
    fn adapter_declared_unsupported_is_explicit() {
        let providers = vec![json!({"provider": "nocache", "provider_status": "supported"})];
        let capability: HashMap<String, String> =
            [("nocache".to_string(), "unsupported".to_string())].into_iter().collect();
        let result = build_cache_tokens(&providers, &[], &capability);
        let row = &result.get("providers").unwrap().as_array().unwrap()[0];
        assert_eq!(row.get("status").unwrap(), &json!("unsupported"));
        assert!(row
            .get("detail")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("no cache-token accounting"));
    }
}
