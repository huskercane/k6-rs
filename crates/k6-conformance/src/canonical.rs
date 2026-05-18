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
    /// CG-6 — per-sample event stream parsed from `--out json` artifacts on
    /// both sides. `None` when the script doesn't exercise sink parity (or
    /// when the adapter doesn't produce one yet). The diff layer treats
    /// `None` on either side as "skip stream diff", so existing scripts
    /// continue to work unchanged.
    pub event_stream: Option<CanonicalEventStream>,
    /// CG-6 — reliability tracking. Populated from the `.diagnostics.json`
    /// sidecar each binary emits. A non-zero `drops_total` on EITHER side
    /// flags the script as UNRELIABLE in the reporter — drops mean the
    /// stream is incomplete and parity findings can't be trusted as
    /// evidence. UNRELIABLE dominates PASS but does NOT hide other
    /// findings; diff still runs and findings are listed.
    pub reliability: Option<Reliability>,
}

/// CG-6 — canonical event-stream model. Both adapters (upstream + k6-rs)
/// parse identical wire formats into this shape, so the diff layer is
/// symmetric and adapter-agnostic.
#[derive(Debug, Clone, Default)]
pub struct CanonicalEventStream {
    /// Metric definitions keyed by canonical name (base name with no tag
    /// braces). Each metric must be defined exactly once before its first
    /// sample. Upstream auto-creates the definition lazily on first sight;
    /// k6-rs emits via the `DefState` queue in `event_stream.rs`.
    pub metric_defs: BTreeMap<String, CanonicalMetricDef>,
    /// Per-metric sample counts. We keep counts (not full sample lists)
    /// because tag-set asymmetries between upstream and k6-rs (k6-rs is
    /// missing `name`/`url`/`proto`/`group`/`scenario` system tags today)
    /// would make any per-sample identity diff fail at every boundary.
    /// First-cut diff strategy is metric-level counts only.
    pub sample_counts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct CanonicalMetricDef {
    pub name: String,
    pub kind: CanonicalMetricKindTag,
    /// Upstream's `contains` field: `"default"` or `"time"`. We treat it
    /// as opaque-but-comparable — the diff flags mismatches without
    /// interpreting the semantic.
    pub contains: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalMetricKindTag {
    Counter,
    Gauge,
    Rate,
    Trend,
}

impl CanonicalMetricKindTag {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "counter" => Self::Counter,
            "gauge" => Self::Gauge,
            "rate" => Self::Rate,
            "trend" => Self::Trend,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Rate => "rate",
            Self::Trend => "trend",
        }
    }
}

/// CG-6 — per-side reliability snapshot. Read from the
/// `<out-json>.diagnostics.json` sidecar that k6-rs writes (and that
/// upstream may or may not emit — see adapter notes). When upstream
/// doesn't emit one, we synthesize a zero-drop entry: upstream's writer
/// is unbounded and back-pressures producers, so by construction it
/// never drops samples.
#[derive(Debug, Clone)]
pub struct Reliability {
    pub upstream: SideReliability,
    pub k6rs: SideReliability,
}

#[derive(Debug, Clone, Default)]
pub struct SideReliability {
    pub capacity: u64,
    pub peak_occupancy: u64,
    pub drops_total: u64,
    pub drops_per_metric: BTreeMap<String, u64>,
    /// CG-6 follow-up: set when the sidecar evidence is missing or
    /// unparseable. Tolerant for upstream (its writer is unbounded —
    /// no sidecar by design); REQUIRED for k6-rs (the writer task
    /// always emits one). When set, the run is UNRELIABLE because we
    /// have no evidence about drops — analogous to a non-zero drop
    /// count: we cannot trust this side's stream as parity evidence.
    pub error: Option<String>,
}

impl Reliability {
    /// A run is UNRELIABLE iff either side has drops OR either side's
    /// reliability evidence is missing/unparseable (k6-rs sidecar
    /// absence). Per the CG-6 design: UNRELIABLE dominates PASS, but
    /// does NOT hide actual parity findings.
    pub fn is_unreliable(&self) -> bool {
        self.upstream.is_unreliable() || self.k6rs.is_unreliable()
    }
}

impl SideReliability {
    pub fn is_unreliable(&self) -> bool {
        self.drops_total > 0 || self.error.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalMetric {
    pub name: String,
    pub tags: NormalizedTagSet,
    pub kind: CanonicalMetricKind,
}

#[derive(Debug, Clone)]
pub enum CanonicalMetricKind {
    Counter {
        count: f64,
        rate: f64,
    },
    Gauge {
        value: f64,
        min: f64,
        max: f64,
    },
    Rate {
        rate: f64,
        passes: u64,
        fails: u64,
    },
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
