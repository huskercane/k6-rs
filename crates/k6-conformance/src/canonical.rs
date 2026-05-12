//! Canonical intermediate model. Both adapters target this shape.
//!
//! Spike scope: only `Counter` and `Trend` metric kinds are exercised. Gauge/Rate
//! variants exist but are not diffed yet.

use std::collections::BTreeMap;

use k6_core::selector::MetricSelector;

/// Normalized full tag set (BTreeMap for deterministic ordering — CG-3).
/// Identity for a metric series is `(name, NormalizedTagSet)`; hashing is an
/// acceleration concern handled elsewhere, never identity.
pub type NormalizedTagSet = BTreeMap<String, String>;

#[derive(Debug, Clone, Default)]
pub struct CanonicalRun {
    /// Keyed by canonical selector string (e.g. `http_reqs` or
    /// `http_req_duration{status:200}`). The diff layer only reads this map.
    pub metrics: BTreeMap<String, CanonicalMetric>,
    pub thresholds: Vec<CanonicalThreshold>,
    pub summary: CanonicalSummary,
    pub exit_code: i32,
    /// Per-check identity (CG-1). Key is the canonical check path
    /// `"<group_path>::<name>"` matching upstream k6's `lib.Check.Path`. The
    /// diff layer iterates the union and reports any pass/fail count mismatch
    /// by check identity.
    pub checks: BTreeMap<String, CanonicalCheck>,
    /// Per-group identity (CG-2). Key is the canonical group path (matches
    /// upstream's `lib.Group.Path`). Root group (path `""`) is intentionally
    /// excluded from this map — it always exists on both sides by
    /// construction, so diffing it adds no signal. Per-group duration stats
    /// are intentionally NOT captured here in CG-2: upstream's
    /// `--summary-export` doesn't expose per-group `group_duration`
    /// submetrics, so a duration-drift comparison would have nothing to
    /// diff against until sink-stream parity work lands. Identity (name,
    /// path, id) is what CG-2's conformance surface gates on.
    pub groups: BTreeMap<String, CanonicalGroup>,
}

#[derive(Debug, Clone)]
pub struct CanonicalMetric {
    pub name: String,
    pub tags: NormalizedTagSet,
    pub kind: CanonicalMetricKind,
}

#[derive(Debug, Clone)]
pub enum CanonicalMetricKind {
    Counter { count: f64, rate: f64 },
    Gauge { value: f64, min: f64, max: f64 },
    Rate { rate: f64, passes: u64, fails: u64 },
    /// Distribution stats. `p99` is `None` until CG-pre P0 (real percentiles) lands.
    Trend(CanonicalTrend),
}

#[derive(Debug, Clone, Default)]
pub struct CanonicalTrend {
    /// Upstream's `--summary-export` does not include `count` for trends;
    /// k6-rs's does. `None` means "this side did not expose it" — the diff
    /// layer must skip the count comparison when either side is None.
    pub count: Option<u64>,
    pub avg: f64,
    pub min: f64,
    pub med: f64,
    pub max: f64,
    pub p90: f64,
    pub p95: f64,
    /// `None` until CG-pre P0 (real percentiles) lands.
    pub p99: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CanonicalThreshold {
    pub metric: String,
    pub expression: String,
    pub passed: bool,
    pub actual_value: f64,
}

/// Per-check identity row (CG-1). Identity in storage is `(group_path, name)`;
/// the map key in `CanonicalRun.checks` is the canonical path so both
/// adapters can flatten the upstream-shape `root_group.{groups,checks}` tree
/// into one comparable map without re-walking the tree at diff time.
///
/// `id` is the md5 hex of `path` as serialized by each runner — it's not
/// load-bearing identity (the tuple key is), but capturing it lets the diff
/// layer detect when one side serializes the wrong hash bytes or computes
/// the hash over a slightly different path encoding. If both sides have the
/// canonical implementation, the ids match by construction.
#[derive(Debug, Clone)]
pub struct CanonicalCheck {
    pub name: String,
    pub group_path: String,
    pub id: String,
    pub passes: u64,
    pub fails: u64,
}

/// Per-group identity row (CG-2). Map key in `CanonicalRun.groups` is the
/// canonical path so the diff layer can iterate the union without re-walking
/// the tree. The `id` is the serialized md5 each runner emits; comparing it
/// catches hash / path-encoding bugs the same way `CanonicalCheck.id` does
/// for checks. Per-group `group_duration` is intentionally absent — see the
/// note on `CanonicalRun.groups` for the reasoning.
#[derive(Debug, Clone)]
pub struct CanonicalGroup {
    pub name: String,
    pub path: String,
    pub id: String,
}

/// Placeholder for spike: groups/checks/threshold-state expand in weeks 2-4
/// once CG-1/CG-2/CG-4 land in the engine.
#[derive(Debug, Clone, Default)]
pub struct CanonicalSummary {
    pub duration_ms: f64,
}

/// Produce the canonical selector string for a metric identity. CG-5: this
/// is a thin wrapper over `MetricSelector::canonical` — the encoding lives
/// in `k6-core` so the engine (threshold lookup) and the conformance harness
/// produce byte-identical canonical forms.
pub fn selector_string(name: &str, tags: &NormalizedTagSet) -> String {
    MetricSelector {
        name: name.to_string(),
        tags: tags.clone(),
    }
    .canonical()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_no_tags() {
        let tags = NormalizedTagSet::new();
        assert_eq!(selector_string("http_reqs", &tags), "http_reqs");
    }

    #[test]
    fn selector_ordered_tags() {
        let mut tags = NormalizedTagSet::new();
        tags.insert("status".into(), "200".into());
        tags.insert("scenario".into(), "light".into());
        // BTreeMap orders keys alphabetically: scenario before status
        assert_eq!(
            selector_string("http_req_duration", &tags),
            "http_req_duration{scenario:light,status:200}"
        );
    }
}
