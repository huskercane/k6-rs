//! Upstream k6 adapter.
//!
//! `--summary-export` emits a JSON object: `{root_group, metrics}`. CRUCIAL:
//! it has **no `type` field per metric** — the kind is inferred from which
//! `values` keys are present. Upstream trends do NOT include `count`; gauges
//! (vus, vus_max) are NOT emitted in summary-export at all (terminal-only).
//!
//! Inference rules (from observing real k6 v2.0 output):
//!   - has `passes` AND `fails` → Rate
//!   - has `count` AND `rate` AND no `passes` → Counter
//!   - has `avg` AND `p(90)` → Trend (no count)
//!   - has `value` AND `min` AND `max` AND no `avg` → Gauge
//!
//! Spike scope: counters + trends only. The `--out json` event stream is wired
//! in later when sink parity work begins (week 5).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::adapters::{Adapter, RunArtifacts, collect_root_group_tree, read_to_string};
use crate::canonical::{
    CanonicalCheck, CanonicalGroup, CanonicalMetric, CanonicalMetricKind, CanonicalRun,
    CanonicalSummary, CanonicalTrend, NormalizedTagSet, selector_string,
};

pub struct UpstreamAdapter;

impl Adapter for UpstreamAdapter {
    fn adapt(&self, artifacts: &RunArtifacts) -> Result<CanonicalRun> {
        let raw = read_to_string(&artifacts.summary_export_path)?;
        let parsed: UpstreamSummary =
            serde_json::from_str(&raw).context("decoding upstream summary JSON")?;

        let mut metrics = BTreeMap::new();
        for (raw_name, entry) in parsed.metrics {
            let (name, tags) = split_selector(&raw_name);
            // Upstream embeds a `thresholds: { "expr": ok_bool }` object inside
            // metrics that have thresholds attached. Filter to just the
            // numeric fields so kind inference can proceed.
            let numeric: BTreeMap<String, f64> = entry
                .iter()
                .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
                .collect();
            let Some(kind) = infer_kind(&numeric) else {
                continue;
            };
            let selector = selector_string(&name, &tags);
            metrics.insert(selector, CanonicalMetric { name, tags, kind });
        }

        // CG-1 + CG-2: shared walker fills both per-check and per-group maps
        // in one DFS. Symmetric across adapters.
        let mut checks: BTreeMap<String, CanonicalCheck> = BTreeMap::new();
        let mut groups: BTreeMap<String, CanonicalGroup> = BTreeMap::new();
        if let Some(root_group) = parsed.root_group {
            collect_root_group_tree(&root_group, &mut groups, &mut checks);
        }

        Ok(CanonicalRun {
            metrics,
            thresholds: Vec::new(),
            summary: CanonicalSummary {
                duration_ms: 0.0, // upstream summary-export omits duration; engine work later.
            },
            exit_code: artifacts.exit_code,
            checks,
            groups,
            // CG-6: the runner reads the event stream + sidecar
            // separately and populates these post-adapt. Keeps the
            // summary-export adapter focused on its single concern.
            event_stream: None,
            reliability: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamSummary {
    #[serde(default)]
    metrics: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    root_group: Option<Value>,
}

fn infer_kind(values: &BTreeMap<String, f64>) -> Option<CanonicalMetricKind> {
    let has = |k: &str| values.contains_key(k);
    let get = |k: &str| values.get(k).copied().unwrap_or(0.0);

    if has("passes") && has("fails") {
        return Some(CanonicalMetricKind::Rate {
            rate: get("value"),
            passes: get("passes") as u64,
            fails: get("fails") as u64,
        });
    }
    if has("count") && has("rate") && !has("passes") {
        return Some(CanonicalMetricKind::Counter {
            count: get("count"),
            rate: get("rate"),
        });
    }
    if has("avg") && has("p(90)") {
        return Some(CanonicalMetricKind::Trend(CanonicalTrend {
            count: None, // upstream does not expose count in summary-export
            avg: get("avg"),
            min: get("min"),
            med: get("med"),
            max: get("max"),
            p90: get("p(90)"),
            p95: get("p(95)"),
            p99: values.get("p(99)").copied(),
        }));
    }
    if has("value") && has("min") && has("max") && !has("avg") {
        return Some(CanonicalMetricKind::Gauge {
            value: get("value"),
            min: get("min"),
            max: get("max"),
        });
    }
    None
}

/// CG-5: delegate to the shared `k6_core::selector` parser. On parse error
/// (malformed input from a future runner version, etc.) fall back to
/// treating the entire raw string as a tag-less name; the canonical layer
/// stays consistent with the engine-side fallback semantics in
/// `thresholds::resolve_stat`.
fn split_selector(raw: &str) -> (String, NormalizedTagSet) {
    match k6_core::selector::MetricSelector::parse(raw) {
        Ok(s) => (s.name, s.tags),
        Err(_) => (raw.to_string(), NormalizedTagSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn infers_counter() {
        let v = vals(&[("count", 10.0), ("rate", 5.0)]);
        match infer_kind(&v) {
            Some(CanonicalMetricKind::Counter { count, rate }) => {
                assert_eq!(count, 10.0);
                assert_eq!(rate, 5.0);
            }
            _ => panic!("expected counter"),
        }
    }

    #[test]
    fn infers_trend_without_count() {
        let v = vals(&[
            ("avg", 1.5),
            ("min", 0.1),
            ("med", 0.5),
            ("max", 13.0),
            ("p(90)", 2.0),
            ("p(95)", 7.5),
        ]);
        match infer_kind(&v) {
            Some(CanonicalMetricKind::Trend(t)) => {
                assert_eq!(t.count, None);
                assert_eq!(t.p95, 7.5);
                assert_eq!(t.p99, None);
            }
            _ => panic!("expected trend"),
        }
    }

    #[test]
    fn infers_rate() {
        let v = vals(&[("passes", 10.0), ("fails", 0.0), ("value", 1.0)]);
        match infer_kind(&v) {
            Some(CanonicalMetricKind::Rate {
                rate,
                passes,
                fails,
            }) => {
                assert_eq!(rate, 1.0);
                assert_eq!(passes, 10);
                assert_eq!(fails, 0);
            }
            _ => panic!("expected rate"),
        }
    }

    #[test]
    fn parses_real_upstream_shape() {
        let s = r#"{
            "root_group": {"name":"","path":""},
            "metrics": {
                "http_reqs": {"count": 10, "rate": 519.97},
                "iterations": {"count": 10, "rate": 519.97},
                "http_req_duration": {
                    "avg": 1.66, "min": 0.30, "med": 0.34,
                    "max": 13.18, "p(90)": 1.90, "p(95)": 7.54
                },
                "http_req_failed": {"passes": 10, "fails": 0, "value": 1}
            }
        }"#;
        let parsed: UpstreamSummary = serde_json::from_str(s).unwrap();
        assert_eq!(parsed.metrics.len(), 4);
    }

    #[test]
    fn splits_submetric_selector() {
        let (name, tags) = split_selector("http_req_duration{status:200}");
        assert_eq!(name, "http_req_duration");
        assert_eq!(tags.get("status").map(String::as_str), Some("200"));
    }
}
