//! k6-rs adapter.
//!
//! k6-rs `--summary-export` writes `k6_core::summary::SummaryData`, which today
//! exposes `{metrics, root_group, state}` with `metrics[name].values` shaped
//! similarly to upstream (count/avg/min/med/max/p(90)/p(95) for trends — no
//! p(99) yet; this is the CG-pre P0 gap).
//!
//! Spike scope: counters + trends only, summary-export only. The periodic
//! `--out json` stream is wired in later when sink parity work begins.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::adapters::{collect_root_group_tree, read_to_string, Adapter, RunArtifacts};
use crate::canonical::{
    selector_string, CanonicalCheck, CanonicalGroup, CanonicalMetric, CanonicalMetricKind,
    CanonicalRun, CanonicalSummary, CanonicalTrend, NormalizedTagSet,
};

pub struct K6rsAdapter;

impl Adapter for K6rsAdapter {
    fn adapt(&self, artifacts: &RunArtifacts) -> Result<CanonicalRun> {
        let raw = read_to_string(&artifacts.summary_export_path)?;
        let (summary, metrics, checks, groups) =
            parse(&raw).context("parsing k6-rs summary export")?;

        Ok(CanonicalRun {
            metrics,
            thresholds: Vec::new(),
            summary,
            exit_code: artifacts.exit_code,
            checks,
            groups,
            // CG-6: event_stream + reliability are populated by the
            // runner from the per-side json_stream + sidecar files; the
            // summary-export adapter doesn't own that responsibility.
            event_stream: None,
            reliability: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct K6rsSummary {
    metrics: BTreeMap<String, K6rsMetricEntry>,
    #[serde(default)]
    state: K6rsState,
    /// CG-1: k6-rs's summary now mirrors upstream's `root_group` shape (the
    /// schema bump in `k6_core::summary::SummaryGroup`). Both adapters feed
    /// the same flattening walker for per-check identity.
    #[serde(default)]
    root_group: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct K6rsState {
    #[serde(default)]
    test_run_duration_ms: f64,
}

#[derive(Debug, Deserialize)]
struct K6rsMetricEntry {
    #[serde(rename = "type")]
    metric_type: String,
    #[serde(default)]
    values: BTreeMap<String, f64>,
}

fn parse(
    s: &str,
) -> Result<(
    CanonicalSummary,
    BTreeMap<String, CanonicalMetric>,
    BTreeMap<String, CanonicalCheck>,
    BTreeMap<String, CanonicalGroup>,
)> {
    let parsed: K6rsSummary = serde_json::from_str(s).context("decoding summary JSON")?;
    let summary = CanonicalSummary {
        duration_ms: parsed.state.test_run_duration_ms,
    };

    let mut checks: BTreeMap<String, CanonicalCheck> = BTreeMap::new();
    let mut groups: BTreeMap<String, CanonicalGroup> = BTreeMap::new();
    if let Some(root_group) = &parsed.root_group {
        collect_root_group_tree(root_group, &mut groups, &mut checks);
    }

    let mut out = BTreeMap::new();
    for (raw_name, entry) in parsed.metrics {
        let (name, tags) = split_selector(&raw_name);
        let kind = match entry.metric_type.as_str() {
            "counter" => CanonicalMetricKind::Counter {
                count: *entry.values.get("count").unwrap_or(&0.0),
                rate: *entry.values.get("rate").unwrap_or(&0.0),
            },
            "trend" => CanonicalMetricKind::Trend(CanonicalTrend {
                count: entry.values.get("count").copied().map(|c| c as u64),
                avg: *entry.values.get("avg").unwrap_or(&0.0),
                min: *entry.values.get("min").unwrap_or(&0.0),
                med: *entry.values.get("med").unwrap_or(&0.0),
                max: *entry.values.get("max").unwrap_or(&0.0),
                p90: *entry.values.get("p(90)").unwrap_or(&0.0),
                p95: *entry.values.get("p(95)").unwrap_or(&0.0),
                // P0 gap — k6-rs trends don't expose p(99) yet
                p99: entry.values.get("p(99)").copied(),
            }),
            "rate" => CanonicalMetricKind::Rate {
                rate: *entry.values.get("rate").unwrap_or(&0.0),
                passes: entry.values.get("passes").copied().unwrap_or(0.0) as u64,
                fails: entry.values.get("fails").copied().unwrap_or(0.0) as u64,
            },
            "gauge" => CanonicalMetricKind::Gauge {
                value: *entry.values.get("value").unwrap_or(&0.0),
                min: *entry.values.get("min").unwrap_or(&0.0),
                max: *entry.values.get("max").unwrap_or(&0.0),
            },
            _ => continue,
        };
        let selector = selector_string(&name, &tags);
        out.insert(
            selector,
            CanonicalMetric {
                name,
                tags,
                kind,
            },
        );
    }
    Ok((summary, out, checks, groups))
}

/// CG-5: delegate to the shared `k6_core::selector` parser. The local impl
/// is gone — both adapters use the same code path so canonical forms can't
/// drift between sides. On parse error we fall back to the raw string as
/// a tag-less name, matching engine-side fallback in `thresholds::resolve_stat`.
fn split_selector(raw: &str) -> (String, NormalizedTagSet) {
    match k6_core::selector::MetricSelector::parse(raw) {
        Ok(s) => (s.name, s.tags),
        Err(_) => (raw.to_string(), NormalizedTagSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_k6rs_summary() {
        let s = r#"{
            "metrics": {
                "http_reqs": { "type": "counter", "contains": "default",
                               "values": { "count": 10, "rate": 5.0 } },
                "http_req_duration": {
                    "type": "trend", "contains": "time",
                    "values": {
                        "count": 10, "avg": 12.5, "min": 1.0, "med": 11.0,
                        "max": 30.0, "p(90)": 25.0, "p(95)": 28.0
                    }
                }
            },
            "root_group": { "name": "", "path": "" },
            "state": { "is_std_out_tty": false, "test_run_duration_ms": 1234.5 }
        }"#;
        let (sum, m, checks, groups) = parse(s).unwrap();
        assert_eq!(sum.duration_ms, 1234.5);
        assert!(matches!(
            m["http_reqs"].kind,
            CanonicalMetricKind::Counter { count: 10.0, .. }
        ));
        let CanonicalMetricKind::Trend(t) = &m["http_req_duration"].kind else {
            panic!();
        };
        assert_eq!(t.p95, 28.0);
        assert_eq!(t.count, Some(10));
        assert!(t.p99.is_none()); // CG-pre P0 gap
        // Legacy fixture: root_group {name, path} only — no checks, no groups.
        // The walker must handle that gracefully (empty maps, not a parse error).
        assert!(checks.is_empty());
        assert!(groups.is_empty());
    }

    #[test]
    fn parses_k6rs_summary_with_per_check_root_group() {
        // CG-1 + CG-2: k6-rs's summary mirrors upstream's root_group shape
        // with nested groups/checks. The adapter walks the tree into flat
        // per-check + per-group maps keyed by canonical path.
        let s = r#"{
            "metrics": {},
            "root_group": {
                "name": "", "path": "", "id": "d4...",
                "groups": {
                    "api": {
                        "name": "api", "path": "::api", "id": "id-api",
                        "groups": {},
                        "checks": {
                            "v1 ok": {"name": "v1 ok", "path": "::api::v1 ok", "id": "x", "passes": 3, "fails": 0}
                        }
                    }
                },
                "checks": {
                    "is healthy": {"name": "is healthy", "path": "::is healthy", "id": "x", "passes": 2, "fails": 1}
                }
            },
            "state": { "is_std_out_tty": false, "test_run_duration_ms": 100.0 }
        }"#;
        let (_, _, checks, groups) = parse(s).unwrap();
        assert_eq!(checks.len(), 2);
        let h = &checks["::is healthy"];
        assert_eq!(h.group_path, "");
        assert_eq!((h.passes, h.fails), (2, 1));
        let v1 = &checks["::api::v1 ok"];
        assert_eq!(v1.group_path, "::api");
        assert_eq!((v1.passes, v1.fails), (3, 0));
        // CG-2: groups map populated (root excluded).
        assert_eq!(groups.len(), 1);
        assert_eq!(groups["::api"].id, "id-api");
    }
}
