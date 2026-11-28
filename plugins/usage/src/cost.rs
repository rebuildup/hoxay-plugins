//! Estimated API-cost computation for rebuildup.usage (Issue #93 /
//! parent #28 T4). Rust port of `plugins/usage/src/cost.ts` (retired;
//! git history preserves it).
//!
//! Pure: same `(by_model breakdown, pricing table)` input → same
//! output; unit-testable without I/O. The estimate applies standard
//! per-token list rates to the four canonical token buckets. It is
//! **not** a bill: subscription quota consumption, speed premiums,
//! context-tier repricing, provider discounts, and taxes are all
//! excluded (documented in the contract disclaimer).
//!
//! Honesty rules:
//! - `provider_reported_cost_usd` stays a separate field — provider-
//!   reported cost is never blended into the estimate.
//! - A model with no rate-table entry gets `estimated_cost_usd: null`
//!   and `cost_source: "unpriced"` — never a guessed price.
//! - `estimated_cost_usd` totals cover priced models only;
//!   `estimate_complete: false` discloses partial coverage.

use serde_json::{json, Value};

use crate::pricing::{estimate_bucket_cost_usd, lookup_priced_rate, PricingTable};

/// Price every `by_model` breakdown row. Rows are re-sorted: priced
/// models by estimated cost desc, then unpriced models by token volume
/// desc — the actionable rows surface first.
pub fn compute_model_costs(by_model: &[Value], pricing: &PricingTable) -> Value {
    let mut entries: Vec<Value> = Vec::new();
    let mut unpriced: Vec<Value> = Vec::new();
    let mut total = 0.0f64;
    let mut priced = 0u64;
    let mut any_priced = false;

    for bucket in by_model {
        let key = bucket
            .get("key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let estimated = key
            .as_deref()
            .and_then(|k| lookup_priced_rate(pricing, k))
            .map(|(rates, _via)| estimate_bucket_cost_usd(&rates, bucket));
        let cost_source = if estimated.is_some() {
            "model_priced"
        } else {
            "unpriced"
        };
        entries.push(json!({
            "key": key,
            "events": bucket.get("events").cloned().unwrap_or(Value::Null),
            "input_tokens": bucket.get("input_tokens").cloned().unwrap_or(Value::Null),
            "output_tokens": bucket.get("output_tokens").cloned().unwrap_or(Value::Null),
            "cache_read_input_tokens": bucket.get("cache_read_input_tokens").cloned().unwrap_or(Value::Null),
            "cache_creation_input_tokens": bucket.get("cache_creation_input_tokens").cloned().unwrap_or(Value::Null),
            "total_tokens": bucket.get("total_tokens").cloned().unwrap_or(Value::Null),
            "thinking_tokens": bucket.get("thinking_tokens").cloned().unwrap_or(Value::Null),
            "provider_reported_cost_usd": bucket.get("provider_reported_cost_usd").cloned().unwrap_or(Value::Null),
            "estimated_cost_usd": estimated,
            "cost_source": cost_source,
        }));
        if let Some(e) = estimated {
            priced += 1;
            any_priced = true;
            total += e;
        } else {
            unpriced.push(json!({
                "key": key,
                "events": bucket.get("events").cloned().unwrap_or(Value::Null),
                "total_tokens": bucket.get("total_tokens").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    // TS sort: priced by cost desc; priced before unpriced; ties and
    // unpriced rows by total_tokens desc then events desc.
    entries.sort_by(|a, b| {
        let ac = a.get("estimated_cost_usd").and_then(|v| v.as_f64());
        let bc = b.get("estimated_cost_usd").and_then(|v| v.as_f64());
        match (ac, bc) {
            (Some(x), Some(y)) if x != y => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => {
                let at = b
                    .get("total_tokens")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    - a.get("total_tokens")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                if at != 0.0 {
                    return at.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal);
                }
                let ae = b
                    .get("events")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    - a.get("events").and_then(|v| v.as_f64()).unwrap_or(0.0);
                ae.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
            }
        }
    });
    unpriced.sort_by(|a, b| {
        let bt = b
            .get("total_tokens")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            - a.get("total_tokens").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if bt != 0.0 {
            return bt.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal);
        }
        let be = b
            .get("events")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            - a.get("events").and_then(|v| v.as_f64()).unwrap_or(0.0);
        be.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
    });

    json!({
        "by_model": entries,
        "estimated_cost_usd": if any_priced { json!(total) } else { Value::Null },
        "estimate_complete": unpriced.is_empty(),
        "priced_models": priced,
        "unpriced_models": unpriced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{parse_rate_table, PricingTable, RateTable};
    use serde_json::json;

    fn bucket(key: Option<&str>) -> Value {
        json!({
            "key": key,
            "events": 1,
            "input_tokens": 100,
            "output_tokens": 10,
            "cache_read_input_tokens": 40,
            "cache_creation_input_tokens": 6,
            "thinking_tokens": null,
            "total_tokens": 156,
            "provider_reported_cost_usd": null,
        })
    }

    fn pricing_table() -> PricingTable {
        let doc = json!({
            "priced-model": {
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002,
            },
        });
        PricingTable {
            info: None,
            rates: parse_rate_table(&doc),
            overrides: RateTable::new(),
        }
    }

    #[test]
    fn priced_models_get_estimates_unpriced_stay_null() {
        let pricing = pricing_table();
        let result = compute_model_costs(
            &[bucket(Some("priced-model")), bucket(Some("mystery-model")), bucket(None)],
            &pricing,
        );
        let by_model = result.get("by_model").unwrap().as_array().unwrap();
        let priced = by_model
            .iter()
            .find(|m| m.get("key").and_then(|v| v.as_str()) == Some("priced-model"))
            .unwrap();
        // 100*1e-6 + 10*2e-6 + 40*1e-6 + 6*1e-6 = 0.000166
        let est = priced.get("estimated_cost_usd").unwrap().as_f64().unwrap();
        assert!((est - 0.000166).abs() < 1e-10);
        assert_eq!(priced.get("cost_source").unwrap(), &json!("model_priced"));
        let unpriced = by_model
            .iter()
            .find(|m| m.get("key").and_then(|v| v.as_str()) == Some("mystery-model"))
            .unwrap();
        assert!(unpriced.get("estimated_cost_usd").unwrap().is_null());
        assert_eq!(unpriced.get("cost_source").unwrap(), &json!("unpriced"));
        let nullkey = by_model
            .iter()
            .find(|m| m.get("key").unwrap().is_null())
            .unwrap();
        assert_eq!(nullkey.get("cost_source").unwrap(), &json!("unpriced"));
        assert_eq!(result.get("estimate_complete").unwrap(), &json!(false));
        assert_eq!(result.get("priced_models").unwrap(), &json!(1));
        assert_eq!(
            result.get("unpriced_models").unwrap().as_array().unwrap().len(),
            2
        );
        let total = result.get("estimated_cost_usd").unwrap().as_f64().unwrap();
        assert!((total - 0.000166).abs() < 1e-10);
    }

    #[test]
    fn provider_reported_cost_stays_separate() {
        let pricing = pricing_table();
        let mut b = bucket(Some("priced-model"));
        b.as_object_mut()
            .unwrap()
            .insert("provider_reported_cost_usd".to_string(), json!(9.99));
        let result = compute_model_costs(&[b], &pricing);
        let row = &result.get("by_model").unwrap().as_array().unwrap()[0];
        assert_eq!(row.get("provider_reported_cost_usd").unwrap(), &json!(9.99));
        let est = row.get("estimated_cost_usd").unwrap().as_f64().unwrap();
        assert!((est - 0.000166).abs() < 1e-10);
    }

    #[test]
    fn unavailable_table_prices_nothing_never_zero() {
        let pricing = PricingTable {
            info: None,
            rates: RateTable::new(),
            overrides: RateTable::new(),
        };
        let result = compute_model_costs(&[bucket(Some("priced-model"))], &pricing);
        assert!(result.get("estimated_cost_usd").unwrap().is_null());
        let row = &result.get("by_model").unwrap().as_array().unwrap()[0];
        assert!(row.get("estimated_cost_usd").unwrap().is_null());
        assert_eq!(row.get("cost_source").unwrap(), &json!("unpriced"));
        assert_eq!(
            result.get("unpriced_models").unwrap().as_array().unwrap().len(),
            1
        );
    }
}
