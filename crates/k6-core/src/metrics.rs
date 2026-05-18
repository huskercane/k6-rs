use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use parking_lot::RwLock;

use crate::output::event_stream::{EventSink, MetricKind, SinkEvent};
use crate::selector::MetricSelector;

/// A thread-safe metrics registry that collects all built-in and custom metrics.
///
/// VU threads send metric samples through lock-free atomics (Counter, Gauge, Rate)
/// or Mutex-guarded histograms (Trend). The Mutex is held only for the duration of
/// a single `record()` call (~nanoseconds), so contention is minimal.
///
/// `group_tree` is the first-class home of per-group and per-check identity
/// (CG-2). It's a tree rooted at `""` (the empty-path root group). Each node
/// holds its child group nodes, its directly-attached check records, and a
/// per-group `TrendMetric` for `group_duration` on that group. Both
/// `group_register` and `check_add` navigate the tree, creating nodes as
/// needed.
///
/// The aggregate `checks` rate (in `rates`) and aggregate `group_duration`
/// trend (in `trends`) are maintained in parallel — they're the source for
/// threshold expressions like `checks: ['rate>0.99']` and
/// `group_duration: ['avg<200']`. CG-1 / CG-2 keep the two views independent
/// so neither depends on the other; CG-3 will later derive the per-tag views
/// from a tag stream.
#[derive(Default)]
pub struct MetricsRegistry {
    counters: Mutex<HashMap<String, CounterMetric>>,
    gauges: Mutex<HashMap<String, GaugeMetric>>,
    rates: Mutex<HashMap<String, RateMetric>>,
    trends: Mutex<HashMap<String, TrendMetric>>,
    group_tree: Mutex<GroupNode>,
    /// CG-6 — set when `--out json` is in use. Each metric write also
    /// emits a `SinkEvent` for the upstream-compatible event stream.
    /// `RwLock<Option<...>>` because the sink must be droppable at stop
    /// (so the writer task's bounded channel closes and final flush +
    /// sidecar emit run); `OnceLock` was wrong since it can't be cleared,
    /// which would leak a sender clone and hang the writer forever.
    /// Read overhead on the hot path is one uncontended rwlock read,
    /// which parking_lot makes effectively free in steady state.
    event_sink: RwLock<Option<EventSink>>,
}

/// One node in the run's group tree (CG-2). `children` keys are child group
/// names (the segment after the final `::`), `checks` keys are check names
/// recorded directly under this group. `group_duration` accumulates the
/// `group()` call duration for THIS group only; the aggregate
/// `group_duration` trend (in `MetricsRegistry.trends`) keeps the run-wide
/// view for back-compat with existing thresholds.
///
/// Identity in storage is `(parent_path, child_name)` via tree navigation;
/// `path` and `id` (md5 of path) are denormalized into the node for cheap
/// emission at snapshot time. `name == ""` and `path == ""` for the root.
#[derive(Debug)]
pub struct GroupNode {
    pub name: String,
    pub path: String,
    pub children: BTreeMap<String, GroupNode>,
    pub checks: BTreeMap<String, CheckRecord>,
    pub group_duration: TrendMetric,
}

impl Default for GroupNode {
    fn default() -> Self {
        Self::root()
    }
}

impl GroupNode {
    fn root() -> Self {
        Self {
            name: String::new(),
            path: String::new(),
            children: BTreeMap::new(),
            checks: BTreeMap::new(),
            group_duration: TrendMetric::new(),
        }
    }

    fn child(name: &str, path: &str) -> Self {
        Self {
            name: name.to_string(),
            path: path.to_string(),
            children: BTreeMap::new(),
            checks: BTreeMap::new(),
            group_duration: TrendMetric::new(),
        }
    }

    /// Navigate (or create) the node at `segments` from this node. Idempotent
    /// — calling twice with the same segments returns the same node without
    /// allocating. Returns `&mut self` when `segments` is empty.
    fn navigate_or_create<'a>(&'a mut self, segments: &[&str]) -> &'a mut GroupNode {
        let mut cursor = self;
        let mut acc_path = cursor.path.clone();
        for seg in segments {
            acc_path.push_str("::");
            acc_path.push_str(seg);
            let key = (*seg).to_string();
            let acc_for_child = acc_path.clone();
            cursor = cursor
                .children
                .entry(key)
                .or_insert_with(|| GroupNode::child(seg, &acc_for_child));
        }
        cursor
    }

    /// Snapshot this node and its descendants. DFS, parent before children.
    fn snapshot(&self) -> GroupSnapshot {
        let duration = if self.group_duration.count == 0 {
            None
        } else {
            Some(self.group_duration.stats())
        };
        GroupSnapshot {
            name: self.name.clone(),
            path: self.path.clone(),
            children: self
                .children
                .iter()
                .map(|(k, v)| (k.clone(), v.snapshot()))
                .collect(),
            checks: self
                .checks
                .iter()
                .map(|(k, r)| {
                    (
                        k.clone(),
                        (
                            r.passes.load(Ordering::Relaxed),
                            r.fails.load(Ordering::Relaxed),
                        ),
                    )
                })
                .collect(),
            duration,
        }
    }
}

/// Snapshot of one group tree node, parallel to `GroupNode`. Lives in
/// `MetricsSnapshot.group_tree`. `duration` is `None` when no `group(...)`
/// call has completed for this group yet (group-only entries that haven't
/// finished, or pre-materialized intermediate ancestors).
#[derive(Debug, Clone, Default)]
pub struct GroupSnapshot {
    pub name: String,
    pub path: String,
    pub children: BTreeMap<String, GroupSnapshot>,
    pub checks: BTreeMap<String, (u64, u64)>, // name -> (passes, fails)
    pub duration: Option<TrendStats>,
}

/// Split a group_path like `"::A::B"` into ordered segments `["A", "B"]`.
/// Upstream's root path is `""`, which yields an empty segment list. The
/// parser tolerates malformed leading/embedded empties by skipping them.
fn group_path_segments(group_path: &str) -> Vec<&str> {
    if group_path.is_empty() {
        return Vec::new();
    }
    group_path.split("::").filter(|s| !s.is_empty()).collect()
}

/// CG-3 subset matcher. The query is what the caller asked for; the
/// stored key is what the registry has. They "match" when the stored
/// entry's name equals the query's name AND the stored entry's tags
/// contain every (k, v) the query specified. If either side fails to
/// parse as a selector, fall back to byte-equality against the original
/// `raw_query` so legacy / hand-constructed keys still work.
fn parse_query(raw: &str) -> Option<MetricSelector> {
    MetricSelector::parse(raw).ok()
}

fn stored_matches_query(stored: &str, query: &Option<MetricSelector>, raw_query: &str) -> bool {
    let Some(q) = query else {
        return stored == raw_query;
    };
    let Ok(s) = MetricSelector::parse(stored) else {
        return stored == raw_query;
    };
    if s.name != q.name {
        return false;
    }
    q.tags.iter().all(|(k, v)| s.tags.get(k) == Some(v))
}

/// CG-6 hot-path helper: split a canonical metric key into `(base_name,
/// tag_map)` for sink emission. Fast path: untagged names skip parsing
/// entirely. Tagged path mirrors `MetricSelector::canonical()`'s output
/// shape (alphabetical key order, single `:` separator) — does NOT
/// validate; the writer trusts that storage keys are well-formed.
fn split_canonical_for_sink(canonical: &str) -> (String, BTreeMap<String, String>) {
    let Some(brace_idx) = canonical.find('{') else {
        return (canonical.to_string(), BTreeMap::new());
    };
    let name = canonical[..brace_idx].to_string();
    let mut tags = BTreeMap::new();
    let end = canonical.len().saturating_sub(1); // drop trailing '}'
    if end <= brace_idx + 1 {
        return (name, tags);
    }
    let body = &canonical[brace_idx + 1..end];
    for pair in body.split(',') {
        if let Some(colon) = pair.find(':') {
            tags.insert(pair[..colon].to_string(), pair[colon + 1..].to_string());
        }
    }
    (name, tags)
}

/// A monotonically increasing counter (e.g., http_reqs, iterations, data_sent).
#[derive(Debug)]
pub struct CounterMetric {
    pub value: AtomicU64,
}

impl Clone for CounterMetric {
    fn clone(&self) -> Self {
        Self {
            value: AtomicU64::new(self.value.load(Ordering::Relaxed)),
        }
    }
}

impl CounterMetric {
    /// CG-3: merge another counter's value into this one — used when
    /// deriving aggregate / single-tag views from full-tag stored entries
    /// at snapshot time (and from the live `counter_get` subset query).
    fn merge(&mut self, other: &CounterMetric) {
        let v = other.value.load(Ordering::Relaxed);
        self.value.fetch_add(v, Ordering::Relaxed);
    }
}

/// A gauge that holds the latest value (e.g., vus, vus_max).
#[derive(Debug)]
pub struct GaugeMetric {
    pub value: AtomicU64,
    pub min: AtomicU64,
    pub max: AtomicU64,
}

/// A rate metric — tracks percentage of non-zero values (e.g., http_req_failed, checks).
#[derive(Debug)]
pub struct RateMetric {
    pub passes: AtomicU64,
    pub total: AtomicU64,
}

impl Clone for RateMetric {
    fn clone(&self) -> Self {
        Self {
            passes: AtomicU64::new(self.passes.load(Ordering::Relaxed)),
            total: AtomicU64::new(self.total.load(Ordering::Relaxed)),
        }
    }
}

impl RateMetric {
    /// CG-3: merge — sum passes and totals so the derived rate equals the
    /// across-all-samples rate (passes/total, recomputed by the caller).
    fn merge(&mut self, other: &RateMetric) {
        self.passes
            .fetch_add(other.passes.load(Ordering::Relaxed), Ordering::Relaxed);
        self.total
            .fetch_add(other.total.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// Per-check pass/fail counts. Identity is `(group_path, name)` in the registry
/// map; this struct only holds the counters. ID hashing for upstream parity
/// happens at summary build time, not here.
#[derive(Debug, Default)]
pub struct CheckRecord {
    pub passes: AtomicU64,
    pub fails: AtomicU64,
}

/// A trend metric backed by HdrHistogram for percentile calculations.
/// (e.g., http_req_duration, iteration_duration).
#[derive(Debug, Clone)]
pub struct TrendMetric {
    histogram: hdrhistogram::Histogram<u64>,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// The type of data a metric contains (affects display formatting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricContains {
    Time,
    Data,
    Default,
}

/// Snapshot of a trend metric's statistics.
///
/// `p99` was added when the underlying HDR histogram became the source of
/// truth for percentiles. Before that, threshold evaluations against `p(99)`
/// silently snapped to `p95` — see [`MetricsSnapshot::trend_histograms`] for
/// how arbitrary `p(x)` is now resolved.
#[derive(Debug, Clone, Default)]
pub struct TrendStats {
    pub avg: f64,
    pub min: f64,
    pub med: f64,
    pub max: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub count: u64,
}

/// Snapshot of all metrics for summary output.
///
/// `trend_histograms` carries cloned HDR histograms keyed by metric (or
/// submetric) name. Threshold evaluation uses it to answer arbitrary `p(x)`
/// queries — without it, the threshold engine would have to round to whatever
/// fixed percentiles `TrendStats` exposes. Cloning is O(buckets) per trend, a
/// one-time cost at snapshot time.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub counters: Vec<(String, u64, f64)>, // name, value, rate_per_sec
    pub gauges: Vec<(String, f64, f64, f64)>, // name, value, min, max
    pub rates: Vec<(String, f64, u64, u64)>, // name, rate, passes, total
    pub trends: Vec<(String, TrendStats)>, // name, stats
    pub trend_histograms: HashMap<String, hdrhistogram::Histogram<u64>>,
    /// First-class snapshot of the run's group tree (CG-2). Replaces the
    /// flat `checks_per` and `groups` fields that used to derive the tree
    /// at summary-build time. Root has `name == ""` and `path == ""`.
    pub group_tree: GroupSnapshot,
}

impl CounterMetric {
    fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }
}

impl GaugeMetric {
    fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }
}

impl RateMetric {
    fn new() -> Self {
        Self {
            passes: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }
}

impl TrendMetric {
    fn new() -> Self {
        Self {
            // Record values in microseconds — covers 1us to ~1 hour
            histogram: hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000, 3)
                .expect("valid histogram params"),
            count: 0,
            sum: 0.0,
            min: f64::MAX,
            max: f64::MIN,
        }
    }

    fn record(&mut self, value_ms: f64) {
        self.count += 1;
        self.sum += value_ms;
        if value_ms < self.min {
            self.min = value_ms;
        }
        if value_ms > self.max {
            self.max = value_ms;
        }
        // The HDR histogram has a minimum recordable value of 1µs. Skip the
        // histogram for sub-µs samples instead of flooring them at 1µs as the
        // previous code did — a phase that legitimately has zero samples
        // (e.g. http_req_blocked when reqwest can't observe it) used to
        // report a 1µs p95, which the conformance harness then flagged as
        // drift against upstream's real numbers. Aggregates count/sum/avg/
        // min/max keep using the true value, so the only fidelity loss is
        // that sub-µs samples are excluded from percentile calculation.
        let micros = (value_ms * 1000.0) as u64;
        if micros >= 1 {
            let _ = self.histogram.record(micros);
        }
    }

    fn stats(&self) -> TrendStats {
        if self.count == 0 {
            return TrendStats::default();
        }
        let quantile = |q: f64| -> f64 {
            if self.histogram.len() == 0 {
                // All recorded samples were sub-µs (e.g. a phase that was
                // never measured and recorded a stream of zeros). The
                // honest report is 0, not the histogram's bottom bucket.
                0.0
            } else {
                self.histogram.value_at_quantile(q) as f64 / 1000.0
            }
        };
        TrendStats {
            avg: self.sum / self.count as f64,
            min: self.min,
            med: quantile(0.5),
            max: self.max,
            p90: quantile(0.9),
            p95: quantile(0.95),
            p99: quantile(0.99),
            count: self.count,
        }
    }

    /// Clone the underlying histogram so callers (e.g. threshold evaluation)
    /// can answer arbitrary `p(x)` queries against a snapshot. Cost is
    /// O(buckets) per call.
    fn clone_histogram(&self) -> hdrhistogram::Histogram<u64> {
        self.histogram.clone()
    }

    /// CG-3: merge another trend's samples into this one, as if the samples
    /// had been recorded into one histogram originally. count/sum simply
    /// add; min/max take element-wise extrema; the HDR histogram does a
    /// bucket-wise add. Used by snapshot derivation and live subset reads.
    fn merge(&mut self, other: &TrendMetric) {
        if other.count == 0 {
            return;
        }
        self.count += other.count;
        self.sum += other.sum;
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
        let _ = self.histogram.add(&other.histogram);
    }
}

/// Convert a percentile (as a 0-100 quantile) into a millisecond value by
/// querying the cloned histogram in a snapshot. Returns 0.0 if the histogram
/// is empty (all sub-µs samples) or the percentile is out of range.
///
/// Used by threshold evaluation for arbitrary `p(x)` lookups.
pub fn snapshot_percentile_ms(
    snapshot: &MetricsSnapshot,
    metric: &str,
    percentile: f64,
) -> Option<f64> {
    let hist = snapshot.trend_histograms.get(metric)?;
    if hist.len() == 0 {
        return Some(0.0);
    }
    let q = (percentile / 100.0).clamp(0.0, 1.0);
    Some(hist.value_at_quantile(q) as f64 / 1000.0)
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// CG-6 — install the event-stream sink. Called once at CLI parse when
    /// `--out json=...` is configured. Returns the sink in Err if already
    /// set (shouldn't happen in normal flows; surfaces a wiring bug).
    pub fn set_event_sink(&self, sink: EventSink) -> Result<(), EventSink> {
        let mut slot = self.event_sink.write();
        if slot.is_some() {
            return Err(sink);
        }
        *slot = Some(sink);
        Ok(())
    }

    /// CG-6 — drop the registry's sender clone. Must be called at stop
    /// BEFORE awaiting the writer task handle, otherwise the channel
    /// stays open (writer's `rx.recv()` blocks forever) and the run
    /// hangs at exit.
    pub fn clear_event_sink(&self) {
        *self.event_sink.write() = None;
    }

    /// CG-6 — emit a sample event if a sink is installed. Hot path: one
    /// uncontended rwlock read returns None in the common case (no JSON
    /// output configured). When a sink is present, the canonical metric
    /// key is split into (base name, tag map) cheaply (single linear
    /// scan, no full MetricSelector::parse on the hot path) before being
    /// handed to the sink.
    fn emit_to_sink(&self, kind: MetricKind, canonical_name: &str, value: f64) {
        let guard = self.event_sink.read();
        let Some(sink) = guard.as_ref() else {
            return;
        };
        let (metric_name, tags) = split_canonical_for_sink(canonical_name);
        sink.try_record(SinkEvent {
            metric_name,
            metric_kind: kind,
            time: time::OffsetDateTime::now_utc(),
            value,
            tags,
        });
    }

    // --- Tagged metric helpers (CG-3) ---
    //
    // Storage keys by the FULL normalized tag set, encoded via
    // `MetricSelector::canonical()`. A tagged write is exactly one store;
    // the untagged aggregate and single-tag projections are derived at
    // read/snapshot time by merging the underlying metric structs. This
    // preserves the combination — a sample tagged `{status:200,method:GET}`
    // is stored under `name{method:GET,status:200}` and can be retrieved
    // at that combination later. Before CG-3, the same sample was
    // decomposed into N+1 single-tag entries and the combination was lost.

    /// Record a trend value with a full tag set (CG-3). Single write to
    /// the canonical-form key.
    pub fn trend_add_with_tags(&self, name: &str, value_ms: f64, tags: &BTreeMap<String, String>) {
        let key = MetricSelector {
            name: name.to_string(),
            tags: tags.clone(),
        }
        .canonical();
        self.trend_add(&key, value_ms);
    }

    /// Record a rate value with a full tag set (CG-3).
    pub fn rate_add_with_tags(&self, name: &str, passed: bool, tags: &BTreeMap<String, String>) {
        let key = MetricSelector {
            name: name.to_string(),
            tags: tags.clone(),
        }
        .canonical();
        self.rate_add(&key, passed);
    }

    /// Record a counter value with a full tag set (CG-3).
    pub fn counter_add_with_tags(&self, name: &str, value: u64, tags: &BTreeMap<String, String>) {
        let key = MetricSelector {
            name: name.to_string(),
            tags: tags.clone(),
        }
        .canonical();
        self.counter_add(&key, value);
    }

    // Slice-form shims kept for back-compat with existing call sites in
    // BuiltinMetrics. They convert the `(k, v)` slice into the canonical
    // full-tag map and route through the new APIs.

    pub fn trend_add_tagged(&self, name: &str, value_ms: f64, tags: &[(String, String)]) {
        let map: BTreeMap<String, String> =
            tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.trend_add_with_tags(name, value_ms, &map);
    }

    pub fn rate_add_tagged(&self, name: &str, passed: bool, tags: &[(String, String)]) {
        let map: BTreeMap<String, String> =
            tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.rate_add_with_tags(name, passed, &map);
    }

    pub fn counter_add_tagged(&self, name: &str, value: u64, tags: &[(String, String)]) {
        let map: BTreeMap<String, String> =
            tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.counter_add_with_tags(name, value, &map);
    }

    // --- Counter operations ---

    pub fn counter_add(&self, name: &str, value: u64) {
        {
            let mut counters = self.counters.lock().unwrap();
            counters
                .entry(name.to_string())
                .or_insert_with(CounterMetric::new)
                .value
                .fetch_add(value, Ordering::Relaxed);
        }
        self.emit_to_sink(MetricKind::Counter, name, value as f64);
    }

    /// CG-3: aggregate all stored counter entries whose name matches and
    /// whose tags are a superset of the query's. `counter_get("name")`
    /// returns the across-all-samples count regardless of how each sample
    /// was tagged; `counter_get("name{k:v}")` filters to samples that had
    /// at least that tag. Live reads and snapshot views agree on
    /// semantics. Falls back to literal byte-equality if the query
    /// doesn't parse as a selector.
    pub fn counter_get(&self, name: &str) -> u64 {
        let counters = self.counters.lock().unwrap();
        let query = parse_query(name);
        counters
            .iter()
            .filter(|(stored, _)| stored_matches_query(stored, &query, name))
            .map(|(_, c)| c.value.load(Ordering::Relaxed))
            .sum()
    }

    // --- Gauge operations ---

    pub fn gauge_set(&self, name: &str, value: f64) {
        {
            let mut gauges = self.gauges.lock().unwrap();
            let gauge = gauges
                .entry(name.to_string())
                .or_insert_with(GaugeMetric::new);

            let bits = value.to_bits();
            gauge.value.store(bits, Ordering::Relaxed);
            gauge.min.fetch_min(bits, Ordering::Relaxed);
            gauge.max.fetch_max(bits, Ordering::Relaxed);
        }
        self.emit_to_sink(MetricKind::Gauge, name, value);
    }

    pub fn gauge_get(&self, name: &str) -> f64 {
        let gauges = self.gauges.lock().unwrap();
        gauges
            .get(name)
            .map(|g| f64::from_bits(g.value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    // --- Rate operations ---

    pub fn rate_add(&self, name: &str, passed: bool) {
        {
            let mut rates = self.rates.lock().unwrap();
            let rate = rates
                .entry(name.to_string())
                .or_insert_with(RateMetric::new);

            rate.total.fetch_add(1, Ordering::Relaxed);
            if passed {
                rate.passes.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Wire-format value: 1.0 if the tracked event occurred, 0.0 otherwise.
        // For http_req_failed (where `passed` is actually `failed`), this
        // matches upstream's per-sample emission semantic — each sample
        // contributes one bool to the rate denominator.
        self.emit_to_sink(MetricKind::Rate, name, if passed { 1.0 } else { 0.0 });
    }

    /// CG-3: aggregate matching rate entries. Same subset semantics as
    /// `counter_get`. Rate is recomputed from the summed passes/total
    /// (you cannot average ratios sensibly across buckets).
    pub fn rate_get(&self, name: &str) -> (f64, u64, u64) {
        let rates = self.rates.lock().unwrap();
        let query = parse_query(name);
        let (mut passes, mut total) = (0u64, 0u64);
        for (stored, r) in rates.iter() {
            if stored_matches_query(stored, &query, name) {
                passes += r.passes.load(Ordering::Relaxed);
                total += r.total.load(Ordering::Relaxed);
            }
        }
        let rate = if total > 0 {
            passes as f64 / total as f64
        } else {
            0.0
        };
        (rate, passes, total)
    }

    // --- Trend operations ---

    pub fn trend_add(&self, name: &str, value_ms: f64) {
        {
            let mut trends = self.trends.lock().unwrap();
            trends
                .entry(name.to_string())
                .or_insert_with(TrendMetric::new)
                .record(value_ms);
        }
        self.emit_to_sink(MetricKind::Trend, name, value_ms);
    }

    /// CG-3: aggregate matching trend entries via histogram merge. Same
    /// subset semantics as `counter_get`. Returns `None` only when no
    /// stored entry matches at all; if at least one matches but had zero
    /// samples, returns `Some(default-ish stats)` from the merged
    /// `TrendMetric.stats()`.
    pub fn trend_stats(&self, name: &str) -> Option<TrendStats> {
        let trends = self.trends.lock().unwrap();
        let query = parse_query(name);
        let mut merged: Option<TrendMetric> = None;
        for (stored, t) in trends.iter() {
            if !stored_matches_query(stored, &query, name) {
                continue;
            }
            match merged.as_mut() {
                Some(m) => m.merge(t),
                None => merged = Some(t.clone()),
            }
        }
        merged.map(|m| m.stats())
    }

    // --- Group tree (CG-2) ---

    /// Record a single check evaluation under `(group_path, name)`. Navigates
    /// or creates the group node and increments the check's pass/fail
    /// counter. The aggregate `checks` rate metric is intentionally NOT
    /// touched here — callers (`BuiltinMetrics::record_check`) update both
    /// in parallel so the per-check view and the aggregate rate stay
    /// independent across CG-1 / CG-2 / CG-3 evolutions.
    pub fn check_add(&self, group_path: &str, name: &str, passed: bool) {
        let mut tree = self.group_tree.lock().unwrap();
        let segs = group_path_segments(group_path);
        let node = tree.navigate_or_create(&segs);
        let record = node
            .checks
            .entry(name.to_string())
            .or_insert_with(CheckRecord::default);
        if passed {
            record.passes.fetch_add(1, Ordering::Relaxed);
        } else {
            record.fails.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read the counts for a single check, for tests / inspection.
    pub fn check_get(&self, group_path: &str, name: &str) -> (u64, u64) {
        let tree = self.group_tree.lock().unwrap();
        let segs = group_path_segments(group_path);
        let mut cursor = &*tree;
        for seg in &segs {
            match cursor.children.get(*seg) {
                Some(child) => cursor = child,
                None => return (0, 0),
            }
        }
        cursor
            .checks
            .get(name)
            .map(|r| {
                (
                    r.passes.load(Ordering::Relaxed),
                    r.fails.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0))
    }

    /// Materialize a group node for `group_path` (and all its ancestors).
    /// Idempotent — calling for an existing path is a cheap navigation
    /// without allocation. Callable independently from `group_duration_add`
    /// so the JS bridge can register on group ENTRY, guaranteeing the node
    /// exists even if the inner function panics before exit or records
    /// nothing.
    pub fn group_register(&self, group_path: &str) {
        if group_path.is_empty() {
            return;
        }
        let mut tree = self.group_tree.lock().unwrap();
        let segs = group_path_segments(group_path);
        let _ = tree.navigate_or_create(&segs);
    }

    /// Record one `group(...)` invocation's duration on the group's tree
    /// node (per-group duration stats). The aggregate `group_duration`
    /// trend is maintained in parallel by `BuiltinMetrics::record_group_duration`.
    pub fn group_duration_add(&self, group_path: &str, duration_ms: f64) {
        let mut tree = self.group_tree.lock().unwrap();
        let segs = group_path_segments(group_path);
        let node = tree.navigate_or_create(&segs);
        node.group_duration.record(duration_ms);
    }

    // --- Snapshot for summary output ---

    pub fn snapshot(&self, duration_secs: f64) -> MetricsSnapshot {
        let counters = self.counters.lock().unwrap();
        let gauges = self.gauges.lock().unwrap();
        let rates = self.rates.lock().unwrap();
        let trends = self.trends.lock().unwrap();

        // CG-3: each tagged write produces ONE storage entry under its
        // canonical full-tag key. Snapshot derives the upstream-compatible
        // view by, for each stored entry, emitting:
        //   (a) the entry itself (pass-through),
        //   (b) the untagged aggregate (only if the source had any tags —
        //       otherwise the pass-through IS the untagged view and emitting
        //       again would double-count),
        //   (c) one single-tag projection per tag (only if the source had
        //       2+ tags — for a 1-tag source the projection IS the pass-
        //       through, again to avoid double-count).
        // Merges use the per-kind `merge` functions so the derived view is
        // semantically equivalent to "all those samples recorded into one
        // bucket originally."
        let counter_derived = derive_counters(&counters);
        let trend_derived = derive_trends(&trends);
        let rate_derived = derive_rates(&rates);

        let mut counter_snap: Vec<_> = counter_derived
            .iter()
            .map(|(name, c)| {
                let val = c.value.load(Ordering::Relaxed);
                let rate = if duration_secs > 0.0 {
                    val as f64 / duration_secs
                } else {
                    0.0
                };
                (name.clone(), val, rate)
            })
            .collect();
        counter_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut gauge_snap: Vec<_> = gauges
            .iter()
            .map(|(name, g)| {
                let val = f64::from_bits(g.value.load(Ordering::Relaxed));
                let min = f64::from_bits(g.min.load(Ordering::Relaxed));
                let max = f64::from_bits(g.max.load(Ordering::Relaxed));
                let min = if min == f64::from_bits(u64::MAX) {
                    val
                } else {
                    min
                };
                (name.clone(), val, min, max)
            })
            .collect();
        gauge_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rate_snap: Vec<_> = rate_derived
            .iter()
            .map(|(name, r)| {
                let passes = r.passes.load(Ordering::Relaxed);
                let total = r.total.load(Ordering::Relaxed);
                let rate = if total > 0 {
                    passes as f64 / total as f64
                } else {
                    0.0
                };
                (name.clone(), rate, passes, total)
            })
            .collect();
        rate_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut trend_snap: Vec<_> = trend_derived
            .iter()
            .map(|(name, t)| (name.clone(), t.stats()))
            .collect();
        trend_snap.sort_by(|a, b| a.0.cmp(&b.0));

        // CG-3: histograms come from the derived map so threshold `p(x)`
        // lookup against single-tag and untagged views works the same as
        // against pass-through full-tag entries.
        let trend_histograms: HashMap<String, hdrhistogram::Histogram<u64>> = trend_derived
            .iter()
            .map(|(name, t)| (name.clone(), t.clone_histogram()))
            .collect();

        let group_tree = self.group_tree.lock().unwrap().snapshot();

        MetricsSnapshot {
            counters: counter_snap,
            gauges: gauge_snap,
            rates: rate_snap,
            trends: trend_snap,
            trend_histograms,
            group_tree,
        }
    }
}

/// CG-3 derivation helpers. Snapshot keeps things lean:
///   (a) pass-through canonical(stored) — always;
///   (b) untagged aggregate of `name` — only if the source had tags
///       (otherwise the pass-through IS the untagged view).
/// Arbitrary subset projections (single-tag, 2-tag, ...) are NOT eagerly
/// emitted here. The user-facing summary builder synthesizes the
/// specific projections each threshold targets via subset-aware merge
/// at output time. This keeps the snapshot O(stored entries) instead of
/// O(stored × 2^tags).
fn derive_targets(stored_key: &str) -> Vec<String> {
    let Ok(sel) = MetricSelector::parse(stored_key) else {
        // Unparseable storage keys pass through as-is (legacy or
        // future-encoded names). No derivation.
        return vec![stored_key.to_string()];
    };
    let mut out = vec![sel.canonical()];
    if !sel.tags.is_empty() {
        out.push(sel.name.clone());
    }
    out
}

fn derive_counters(stored: &HashMap<String, CounterMetric>) -> HashMap<String, CounterMetric> {
    let mut out: HashMap<String, CounterMetric> = HashMap::new();
    for (key, c) in stored {
        for target in derive_targets(key) {
            out.entry(target)
                .and_modify(|m| m.merge(c))
                .or_insert_with(|| c.clone());
        }
    }
    out
}

fn derive_trends(stored: &HashMap<String, TrendMetric>) -> HashMap<String, TrendMetric> {
    let mut out: HashMap<String, TrendMetric> = HashMap::new();
    for (key, t) in stored {
        for target in derive_targets(key) {
            out.entry(target)
                .and_modify(|m| m.merge(t))
                .or_insert_with(|| t.clone());
        }
    }
    out
}

fn derive_rates(stored: &HashMap<String, RateMetric>) -> HashMap<String, RateMetric> {
    let mut out: HashMap<String, RateMetric> = HashMap::new();
    for (key, r) in stored {
        for target in derive_targets(key) {
            out.entry(target)
                .and_modify(|m| m.merge(r))
                .or_insert_with(|| r.clone());
        }
    }
    out
}

/// Convenience wrapper that holds an `Arc<MetricsRegistry>` and provides
/// methods matching the k6 built-in metric names.
#[derive(Clone)]
pub struct BuiltinMetrics {
    pub registry: Arc<MetricsRegistry>,
}

impl BuiltinMetrics {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(MetricsRegistry::new()),
        }
    }

    // --- Execution metrics ---

    pub fn set_vus(&self, count: u32) {
        self.registry.gauge_set("vus", count as f64);
    }

    pub fn set_vus_max(&self, count: u32) {
        self.registry.gauge_set("vus_max", count as f64);
    }

    pub fn record_iteration(&self, duration_ms: f64) {
        self.registry.counter_add("iterations", 1);
        self.registry.trend_add("iteration_duration", duration_ms);
    }

    pub fn record_dropped_iteration(&self) {
        self.registry.counter_add("dropped_iterations", 1);
    }

    // --- Check metrics ---

    /// Record a single check evaluation. Updates both the per-check registry
    /// (CG-1) for `root_group.checks` parity and the aggregate `checks` rate
    /// metric used by threshold expressions like `checks: ['rate>0.99']`. The
    /// two storages are independent on purpose — CG-3 can later derive the
    /// aggregate from tagged samples without disturbing per-check identity.
    pub fn record_check(&self, name: &str, group_path: &str, passed: bool) {
        self.registry.check_add(group_path, name, passed);
        self.registry.rate_add("checks", passed);
    }

    /// Register a group node at this path. Called on `group()` ENTRY by the
    /// JS bridge so the node exists in the tree even if the inner function
    /// records no metrics. Idempotent.
    pub fn register_group(&self, group_path: &str) {
        self.registry.group_register(group_path);
    }

    /// Record one `group(name, fn)` invocation's duration. CG-2: writes to
    /// the per-group node's `group_duration` trend AND the aggregate
    /// `group_duration` trend. The aggregate is kept for back-compat with
    /// existing thresholds like `group_duration: ['avg<200']`; the per-group
    /// stats are surfaced in the snapshot's group tree for future tagged
    /// submetric work (CG-3) and for end-of-run reporting.
    pub fn record_group_duration(&self, group_path: &str, duration_ms: f64) {
        self.registry.group_duration_add(group_path, duration_ms);
        self.registry.trend_add("group_duration", duration_ms);
    }

    // --- HTTP metrics ---

    pub fn record_http_request(&self, timings: &crate::traits::Timings, failed: bool) {
        self.record_http_request_tagged(timings, failed, &[]);
    }

    pub fn record_http_request_tagged(
        &self,
        timings: &crate::traits::Timings,
        failed: bool,
        tags: &[(String, String)],
    ) {
        self.registry.counter_add_tagged("http_reqs", 1, tags);
        // Upstream k6 semantic for http_req_failed: record the failure bool
        // directly. `passes` in the summary then = count of failed requests,
        // `fails` = count of non-failed, `rate` = failed/total (the failure
        // rate). Reversing this (recording !failed) inverts the rate and swaps
        // the passes/fails fields in --summary-export, breaking parity.
        self.registry
            .rate_add_tagged("http_req_failed", failed, tags);
        self.registry
            .trend_add_tagged("http_req_duration", timings.duration, tags);
        self.registry
            .trend_add_tagged("http_req_blocked", timings.blocked, tags);
        self.registry
            .trend_add_tagged("http_req_connecting", timings.connecting, tags);
        self.registry
            .trend_add_tagged("http_req_tls_handshaking", timings.tls_handshaking, tags);
        self.registry
            .trend_add_tagged("http_req_sending", timings.sending, tags);
        self.registry
            .trend_add_tagged("http_req_waiting", timings.waiting, tags);
        self.registry
            .trend_add_tagged("http_req_receiving", timings.receiving, tags);
    }

    // --- Network metrics ---

    pub fn record_data_sent(&self, bytes: u64) {
        self.registry.counter_add("data_sent", bytes);
    }

    pub fn record_data_received(&self, bytes: u64) {
        self.registry.counter_add("data_received", bytes);
    }

    // --- WebSocket metrics ---

    pub fn record_ws_session(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("ws_sessions", 1, tags);
        self.registry
            .trend_add_tagged("ws_session_duration", duration_ms, tags);
    }

    pub fn record_ws_connecting(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry
            .trend_add_tagged("ws_connecting", duration_ms, tags);
    }

    pub fn record_ws_msg_sent(&self, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("ws_msgs_sent", 1, tags);
    }

    pub fn record_ws_msg_received(&self, tags: &[(String, String)]) {
        self.registry
            .counter_add_tagged("ws_msgs_received", 1, tags);
    }

    pub fn record_ws_ping(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry.trend_add_tagged("ws_ping", duration_ms, tags);
    }

    // --- gRPC metrics ---

    pub fn record_grpc_request(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("grpc_reqs", 1, tags);
        self.registry
            .trend_add_tagged("grpc_req_duration", duration_ms, tags);
    }
}

impl Default for BuiltinMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_basic() {
        let reg = MetricsRegistry::new();
        reg.counter_add("http_reqs", 1);
        reg.counter_add("http_reqs", 1);
        reg.counter_add("http_reqs", 5);
        assert_eq!(reg.counter_get("http_reqs"), 7);
    }

    #[test]
    fn gauge_basic() {
        let reg = MetricsRegistry::new();
        reg.gauge_set("vus", 10.0);
        assert!((reg.gauge_get("vus") - 10.0).abs() < 0.01);
        reg.gauge_set("vus", 5.0);
        assert!((reg.gauge_get("vus") - 5.0).abs() < 0.01);
    }

    #[test]
    fn rate_basic() {
        let reg = MetricsRegistry::new();
        reg.rate_add("checks", true);
        reg.rate_add("checks", true);
        reg.rate_add("checks", false);

        let (rate, passes, total) = reg.rate_get("checks");
        assert_eq!(passes, 2);
        assert_eq!(total, 3);
        assert!((rate - 0.6667).abs() < 0.01);
    }

    #[test]
    fn trend_basic() {
        let reg = MetricsRegistry::new();
        reg.trend_add("http_req_duration", 100.0);
        reg.trend_add("http_req_duration", 200.0);
        reg.trend_add("http_req_duration", 300.0);

        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.avg - 200.0).abs() < 0.01);
        assert!((stats.min - 100.0).abs() < 0.01);
        assert!((stats.max - 300.0).abs() < 0.01);
    }

    #[test]
    fn trend_percentiles() {
        let reg = MetricsRegistry::new();
        // Add 100 values from 1 to 100
        for i in 1..=100 {
            reg.trend_add("latency", i as f64);
        }

        let stats = reg.trend_stats("latency").unwrap();
        assert_eq!(stats.count, 100);
        assert!((stats.avg - 50.5).abs() < 0.1);
        assert!((stats.min - 1.0).abs() < 0.01);
        assert!((stats.max - 100.0).abs() < 0.01);
        // p50 ≈ 50, p90 ≈ 90, p95 ≈ 95, p99 ≈ 99
        assert!((stats.med - 50.0).abs() < 2.0);
        assert!((stats.p90 - 90.0).abs() < 2.0);
        assert!((stats.p95 - 95.0).abs() < 2.0);
        assert!((stats.p99 - 99.0).abs() < 2.0);
    }

    #[test]
    fn snapshot_resolves_arbitrary_percentile_via_histogram() {
        // P0.1 regression: before this change, only p90/p95 were precomputed;
        // any other percentile snapped to p95 in threshold evaluation. The
        // snapshot now carries the cloned HDR histogram so arbitrary p(x)
        // queries return the real value.
        let reg = MetricsRegistry::new();
        for i in 1..=1000 {
            reg.trend_add("latency", i as f64);
        }
        let snap = reg.snapshot(1.0);

        // Direct query through the new snapshot_percentile_ms helper.
        let p33 = super::snapshot_percentile_ms(&snap, "latency", 33.0).unwrap();
        let p99 = super::snapshot_percentile_ms(&snap, "latency", 99.0).unwrap();
        let p999 = super::snapshot_percentile_ms(&snap, "latency", 99.9).unwrap();

        assert!((p33 - 330.0).abs() < 5.0, "p(33) ≈ 330, got {p33}");
        assert!((p99 - 990.0).abs() < 5.0, "p(99) ≈ 990, got {p99}");
        assert!((p999 - 999.0).abs() < 5.0, "p(99.9) ≈ 999, got {p999}");

        // Sanity: querying an unknown metric returns None (caller can decide
        // how to handle missing trends — current threshold code falls back
        // to 0.0 for any unknown metric).
        assert!(super::snapshot_percentile_ms(&snap, "missing", 50.0).is_none());
    }

    #[test]
    fn snapshot_percentile_on_empty_histogram_is_zero() {
        // All-zero recording leaves the histogram empty (after the (c) fix
        // — sub-µs samples are excluded). The snapshot helper must report
        // 0.0 rather than panicking or returning a phantom 1µs.
        let reg = MetricsRegistry::new();
        for _ in 0..10 {
            reg.trend_add("phase_zero", 0.0);
        }
        let snap = reg.snapshot(1.0);
        let p99 = super::snapshot_percentile_ms(&snap, "phase_zero", 99.0).unwrap();
        assert_eq!(p99, 0.0);
    }

    #[test]
    fn snapshot_sorted() {
        let reg = MetricsRegistry::new();
        reg.counter_add("z_counter", 1);
        reg.counter_add("a_counter", 2);
        reg.trend_add("z_trend", 10.0);
        reg.trend_add("a_trend", 20.0);

        let snap = reg.snapshot(1.0);
        assert_eq!(snap.counters[0].0, "a_counter");
        assert_eq!(snap.counters[1].0, "z_counter");
        assert_eq!(snap.trends[0].0, "a_trend");
        assert_eq!(snap.trends[1].0, "z_trend");
    }

    #[test]
    fn builtin_metrics_http() {
        let m = BuiltinMetrics::new();
        let timings = crate::traits::Timings {
            duration: 150.0,
            waiting: 120.0,
            receiving: 25.0,
            sending: 5.0,
            ..Default::default()
        };

        m.record_http_request(&timings, false);
        m.record_http_request(&timings, false);
        m.record_http_request(&timings, true); // failed

        assert_eq!(m.registry.counter_get("http_reqs"), 3);

        // Upstream-compatible: 1 failed out of 3 → failure rate = 1/3.
        let (fail_rate, passes, total) = m.registry.rate_get("http_req_failed");
        assert!(
            (fail_rate - 0.3333).abs() < 0.01,
            "fail rate should be 1/3, got {fail_rate}"
        );
        assert_eq!(passes, 1, "passes = number of failed requests");
        assert_eq!(total, 3);

        let stats = m.registry.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.avg - 150.0).abs() < 0.01);
    }

    #[test]
    fn trend_record_zero_does_not_pollute_percentiles() {
        // Regression for bug (c): TrendMetric.record() used to do
        //     let micros = (value_ms * 1000.0).max(1.0) as u64;
        // which mapped legitimate zero samples to 1µs in the histogram. A
        // phase that was never measured (e.g. http_req_blocked when reqwest
        // can't see it) recorded a stream of zeros that came out as
        // p50=p90=p95 = 0.001ms, flagged as drift against upstream's real
        // numbers. After the fix, zero samples are excluded from the
        // histogram entirely; aggregates remain honest.
        let mut t = TrendMetric::new();
        for _ in 0..100 {
            t.record(0.0);
        }
        let s = t.stats();
        assert_eq!(s.count, 100);
        assert_eq!(s.avg, 0.0);
        assert_eq!(s.min, 0.0);
        assert_eq!(s.max, 0.0);
        assert_eq!(s.med, 0.0, "previously was 0.001 (1µs floor)");
        assert_eq!(s.p90, 0.0, "previously was 0.001 (1µs floor)");
        assert_eq!(s.p95, 0.0, "previously was 0.001 (1µs floor)");
    }

    #[test]
    fn trend_records_real_values_in_percentiles() {
        // Companion check: non-zero recording still produces correct percentiles.
        let mut t = TrendMetric::new();
        for ms in 1..=100 {
            t.record(ms as f64);
        }
        let s = t.stats();
        assert_eq!(s.count, 100);
        assert!((s.avg - 50.5).abs() < 0.5);
        assert!((s.med - 50.0).abs() < 2.0);
        assert!((s.p90 - 90.0).abs() < 2.0);
        assert!((s.p95 - 95.0).abs() < 2.0);
    }

    #[test]
    fn trend_mixed_zero_and_nonzero_percentiles_reflect_nonzero_only() {
        // 90 zero samples + 10 samples of 100ms. avg should reflect the mix,
        // but p50/p90/p95 should reflect only the non-zero population since
        // sub-µs samples are excluded from the histogram.
        let mut t = TrendMetric::new();
        for _ in 0..90 {
            t.record(0.0);
        }
        for _ in 0..10 {
            t.record(100.0);
        }
        let s = t.stats();
        assert_eq!(s.count, 100);
        assert!((s.avg - 10.0).abs() < 0.1, "avg = 1000/100 = 10ms");
        assert!(
            (s.med - 100.0).abs() < 2.0,
            "p50 over the 10 non-zero samples"
        );
        assert!((s.p95 - 100.0).abs() < 2.0);
    }

    #[test]
    fn check_registry_keys_by_group_and_name() {
        // CG-1/CG-2 regression: per-check identity is `(group_path, name)`.
        // Two checks with the same name in different groups must remain
        // separate. CG-2 moves storage from a flat map onto the group tree
        // — the test now reads through the tree snapshot directly.
        let reg = MetricsRegistry::new();
        reg.check_add("", "status is 200", true);
        reg.check_add("::api", "status is 200", false);
        reg.check_add("::api", "status is 200", false);

        assert_eq!(reg.check_get("", "status is 200"), (1, 0));
        assert_eq!(reg.check_get("::api", "status is 200"), (0, 2));

        let snap = reg.snapshot(1.0);
        // Root holds the first check.
        assert_eq!(snap.group_tree.checks["status is 200"], (1, 0));
        // The api child holds the second one with the same name.
        let api = snap
            .group_tree
            .children
            .get("api")
            .expect("api group materialized by check_add navigation");
        assert_eq!(api.path, "::api");
        assert_eq!(api.checks["status is 200"], (0, 2));
    }

    #[test]
    fn check_registry_counts_passes_and_fails() {
        // CG-1 regression: passes and fails accumulate correctly under a
        // single key, and the aggregate `checks` rate metric (touched only
        // by BuiltinMetrics::record_check, not check_add) stays independent.
        let reg = MetricsRegistry::new();
        reg.check_add("", "ok", true);
        reg.check_add("", "ok", true);
        reg.check_add("", "ok", false);
        assert_eq!(reg.check_get("", "ok"), (2, 1));

        // check_add intentionally does NOT update the aggregate `checks` rate;
        // BuiltinMetrics::record_check owns parallel updates.
        let (_rate, _passes, total) = reg.rate_get("checks");
        assert_eq!(total, 0, "check_add must not touch the aggregate rate");
    }

    #[test]
    fn group_tree_navigates_to_nested_paths_on_check_add() {
        // CG-2 regression: check_add navigates the tree, creating
        // intermediate group nodes as it descends. The result is a real
        // tree, not a flat (path, name) → record map.
        let reg = MetricsRegistry::new();
        reg.check_add("::api::v2", "post ok", true);
        let snap = reg.snapshot(1.0);
        let api = snap
            .group_tree
            .children
            .get("api")
            .expect("api node created");
        assert_eq!(api.path, "::api");
        assert!(api.checks.is_empty(), "api group has no direct check");
        let v2 = api.children.get("v2").expect("v2 child created");
        assert_eq!(v2.path, "::api::v2");
        assert_eq!(v2.checks["post ok"], (1, 0));
    }

    #[test]
    fn group_register_is_idempotent_and_materializes_without_checks() {
        // CG-2 invariant: group_register can be called on every group()
        // entry — repeatedly, even for paths that already exist — and the
        // tree converges to the same shape. A group-only call (no inner
        // check) must still produce a snapshot node.
        let reg = MetricsRegistry::new();
        reg.group_register("::audit");
        reg.group_register("::audit"); // idempotent
        reg.group_register("::audit::sub");
        reg.group_register(""); // empty path → no-op, must not panic

        let snap = reg.snapshot(1.0);
        let audit = snap
            .group_tree
            .children
            .get("audit")
            .expect("audit present");
        assert_eq!(audit.path, "::audit");
        assert!(audit.checks.is_empty());
        let sub = audit.children.get("sub").expect("nested sub present");
        assert_eq!(sub.path, "::audit::sub");
        // Root holds no checks and no children other than what we registered.
        assert!(snap.group_tree.checks.is_empty());
        assert_eq!(snap.group_tree.children.len(), 1);
    }

    #[test]
    fn group_duration_writes_per_node_and_aggregate() {
        // CG-2 regression: record_group_duration writes the per-group
        // TrendMetric on the tree node AND keeps updating the aggregate
        // `group_duration` trend in `trends` so existing thresholds keep
        // working. The two views are kept parallel.
        let m = BuiltinMetrics::new();
        m.record_group_duration("::api", 100.0);
        m.record_group_duration("::api", 200.0);
        m.record_group_duration("::audit", 50.0);

        let snap = m.registry.snapshot(1.0);

        // Per-node duration on api: avg 150ms, two samples.
        let api = snap.group_tree.children.get("api").unwrap();
        let api_dur = api.duration.as_ref().expect("api group_duration populated");
        assert_eq!(api_dur.count, 2);
        assert!((api_dur.avg - 150.0).abs() < 0.01);

        // Per-node duration on audit: 50ms, one sample.
        let audit = snap.group_tree.children.get("audit").unwrap();
        let audit_dur = audit.duration.as_ref().unwrap();
        assert_eq!(audit_dur.count, 1);
        assert!((audit_dur.avg - 50.0).abs() < 0.01);

        // Aggregate group_duration trend: 3 samples (api×2 + audit×1).
        let agg = snap
            .trends
            .iter()
            .find(|(n, _)| n == "group_duration")
            .map(|(_, s)| s.clone())
            .expect("aggregate group_duration present");
        assert_eq!(agg.count, 3);
        assert!((agg.avg - (100.0 + 200.0 + 50.0) / 3.0).abs() < 0.01);
    }

    #[test]
    fn group_with_no_duration_has_none_duration_field() {
        // CG-2: a group that was register_group'd but never had a duration
        // recorded (e.g. group entered but inner fn panicked before exit)
        // must surface as `duration: None`, not a phantom zero-trend stats.
        let reg = MetricsRegistry::new();
        reg.group_register("::pending");
        let snap = reg.snapshot(1.0);
        let pending = snap.group_tree.children.get("pending").unwrap();
        assert!(pending.duration.is_none());
    }

    #[test]
    fn builtin_record_check_updates_per_check_and_aggregate() {
        // CG-1: BuiltinMetrics::record_check is the single hot-path entry —
        // it must update both per-check storage and the aggregate `checks`
        // rate so threshold expressions on `checks: ['rate>X']` keep working
        // alongside the new per-check tree.
        let m = BuiltinMetrics::new();
        m.record_check("a", "", true);
        m.record_check("a", "", false);
        m.record_check("b", "", true);

        assert_eq!(m.registry.check_get("", "a"), (1, 1));
        assert_eq!(m.registry.check_get("", "b"), (1, 0));
        let (rate, passes, total) = m.registry.rate_get("checks");
        assert_eq!(total, 3);
        assert_eq!(passes, 2);
        assert!((rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn http_req_failed_matches_upstream_semantics() {
        // Regression for bug (d): record_http_request_tagged previously
        // recorded `!failed`, inverting the rate and swapping the
        // passes/fails fields in --summary-export. After fix, passes
        // counts FAILED requests (per upstream's convention), fails counts
        // non-failed, rate is the failure rate.
        let m = BuiltinMetrics::new();
        let t = crate::traits::Timings::default();

        // All 10 succeed → 0% failure rate, 0 passes (= 0 failures), 10 fails.
        for _ in 0..10 {
            m.record_http_request(&t, false);
        }
        let (rate, passes, total) = m.registry.rate_get("http_req_failed");
        assert_eq!(rate, 0.0);
        assert_eq!(passes, 0);
        assert_eq!(total, 10);

        // 5 more, all fail → 5/15 = 33.3% failure rate.
        for _ in 0..5 {
            m.record_http_request(&t, true);
        }
        let (rate, passes, total) = m.registry.rate_get("http_req_failed");
        assert!((rate - 5.0 / 15.0).abs() < 1e-9);
        assert_eq!(passes, 5);
        assert_eq!(total, 15);
    }

    #[test]
    fn builtin_metrics_iterations() {
        let m = BuiltinMetrics::new();
        m.record_iteration(100.0);
        m.record_iteration(200.0);
        m.record_dropped_iteration();

        assert_eq!(m.registry.counter_get("iterations"), 2);
        assert_eq!(m.registry.counter_get("dropped_iterations"), 1);

        let stats = m.registry.trend_stats("iteration_duration").unwrap();
        assert_eq!(stats.count, 2);
        assert!((stats.avg - 150.0).abs() < 0.01);
    }

    #[test]
    fn concurrent_metric_access() {
        let reg = Arc::new(MetricsRegistry::new());
        let mut handles = vec![];

        for _ in 0..20 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    reg.counter_add("http_reqs", 1);
                    reg.trend_add("http_req_duration", i as f64);
                    reg.rate_add("checks", i % 2 == 0);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(reg.counter_get("http_reqs"), 2000);

        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 2000);

        let (_, passes, total) = reg.rate_get("checks");
        assert_eq!(total, 2000);
        assert_eq!(passes, 1000);
    }

    #[test]
    fn snapshot_with_rate() {
        let reg = MetricsRegistry::new();
        reg.counter_add("http_reqs", 100);

        let snap = reg.snapshot(10.0); // 10 seconds
        assert_eq!(snap.counters[0].1, 100); // value
        assert!((snap.counters[0].2 - 10.0).abs() < 0.01); // rate = 100/10
    }

    #[test]
    fn tagged_trend_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![
            ("scenario".to_string(), "light".to_string()),
            ("method".to_string(), "GET".to_string()),
        ];

        reg.trend_add_tagged("http_req_duration", 100.0, &tags);
        reg.trend_add_tagged("http_req_duration", 200.0, &tags);

        // Base metric has both
        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 2);

        // Tagged sub-metrics also have both
        let tagged = reg
            .trend_stats("http_req_duration{scenario:light}")
            .unwrap();
        assert_eq!(tagged.count, 2);

        let method_tagged = reg.trend_stats("http_req_duration{method:GET}").unwrap();
        assert_eq!(method_tagged.count, 2);
    }

    #[test]
    fn tagged_counter_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![("scenario".to_string(), "heavy".to_string())];

        reg.counter_add_tagged("http_reqs", 1, &tags);
        reg.counter_add_tagged("http_reqs", 1, &tags);

        assert_eq!(reg.counter_get("http_reqs"), 2);
        assert_eq!(reg.counter_get("http_reqs{scenario:heavy}"), 2);
    }

    #[test]
    fn tagged_rate_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![("scenario".to_string(), "api".to_string())];

        reg.rate_add_tagged("http_req_failed", true, &tags);
        reg.rate_add_tagged("http_req_failed", false, &tags);

        let (_, passes, total) = reg.rate_get("http_req_failed");
        assert_eq!(total, 2);

        let (_, tagged_passes, tagged_total) = reg.rate_get("http_req_failed{scenario:api}");
        assert_eq!(tagged_total, 2);
        assert_eq!(tagged_passes, 1);
    }

    // --- CG-3: full tag-set preservation ---

    #[test]
    fn trend_add_with_tags_preserves_full_combination() {
        // CG-3 bug fix: before this change, `trend_add_tagged` decomposed
        // a multi-tag sample into independent single-tag submetrics, so
        // the COMBINATION `{status:200,method:GET}` was lost. After CG-3
        // there's a real storage entry under the canonical full-tag key,
        // queryable directly AND via every subset.
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("status".to_string(), "200".to_string());
        tags.insert("method".to_string(), "GET".to_string());
        reg.trend_add_with_tags("http_req_duration", 100.0, &tags);
        reg.trend_add_with_tags("http_req_duration", 200.0, &tags);

        // Full-tag query — finds the stored entry directly.
        let full = reg
            .trend_stats("http_req_duration{method:GET,status:200}")
            .expect("full-tag entry exists");
        assert_eq!(full.count, 2);
        assert!((full.avg - 150.0).abs() < 0.01);

        // Subset queries — same merged stats, because both samples'
        // tag sets contain the query's tag.
        let by_status = reg.trend_stats("http_req_duration{status:200}").unwrap();
        assert_eq!(by_status.count, 2);
        let by_method = reg.trend_stats("http_req_duration{method:GET}").unwrap();
        assert_eq!(by_method.count, 2);

        // Untagged query — across all samples.
        let all = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(all.count, 2);
    }

    #[test]
    fn full_tag_buckets_separate_distinct_combinations() {
        // Two different full-tag combinations stay separated; subset
        // queries aggregate across them; single-tag queries that match
        // only one combo return just that combo's samples.
        let reg = MetricsRegistry::new();
        let mut t1 = std::collections::BTreeMap::new();
        t1.insert("status".to_string(), "200".to_string());
        t1.insert("method".to_string(), "GET".to_string());
        let mut t2 = std::collections::BTreeMap::new();
        t2.insert("status".to_string(), "500".to_string());
        t2.insert("method".to_string(), "GET".to_string());

        for _ in 0..3 {
            reg.counter_add_with_tags("http_reqs", 1, &t1);
        }
        for _ in 0..7 {
            reg.counter_add_with_tags("http_reqs", 1, &t2);
        }

        // Distinct full-tag combinations.
        assert_eq!(
            reg.counter_get("http_reqs{method:GET,status:200}"),
            3,
            "200 combo isolated"
        );
        assert_eq!(
            reg.counter_get("http_reqs{method:GET,status:500}"),
            7,
            "500 combo isolated"
        );
        // Subset that matches both combos.
        assert_eq!(reg.counter_get("http_reqs{method:GET}"), 10);
        // Single-tag that matches only one combo.
        assert_eq!(reg.counter_get("http_reqs{status:200}"), 3);
        // Untagged aggregate.
        assert_eq!(reg.counter_get("http_reqs"), 10);
    }

    #[test]
    fn tag_order_canonicalization_on_write_and_read() {
        // Write with tags in {b, a} order; lookup with {a, b} order.
        // Storage uses MetricSelector canonical (alphabetical) — both
        // round-trip to the same key.
        let reg = MetricsRegistry::new();
        let mut tags_ba = std::collections::BTreeMap::new();
        tags_ba.insert("b".to_string(), "2".to_string());
        tags_ba.insert("a".to_string(), "1".to_string());
        reg.counter_add_with_tags("name", 5, &tags_ba);

        // Read with the "alphabetical" form.
        assert_eq!(reg.counter_get("name{a:1,b:2}"), 5);
        // And with the original write order — canonical normalization
        // routes both to the same storage entry.
        assert_eq!(reg.counter_get("name{b:2,a:1}"), 5);
    }

    #[test]
    fn snapshot_emits_full_tag_passthrough_plus_untagged_aggregate() {
        // CG-3: snapshot is intentionally lean — it emits the stored
        // full-tag entry (pass-through) and the untagged aggregate, but
        // does NOT eagerly emit arbitrary subset projections (single-tag,
        // 2-tag, ...). Subset projections that thresholds need are
        // synthesized at summary-build time. This keeps the snapshot
        // O(stored entries) instead of O(stored × 2^tags).
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("a".to_string(), "1".to_string());
        tags.insert("b".to_string(), "2".to_string());
        for _ in 0..4 {
            reg.counter_add_with_tags("name", 1, &tags);
        }

        let snap = reg.snapshot(1.0);
        let by_key: std::collections::HashMap<&str, u64> = snap
            .counters
            .iter()
            .map(|(k, v, _)| (k.as_str(), *v))
            .collect();
        assert_eq!(by_key["name{a:1,b:2}"], 4, "full-tag pass-through");
        assert_eq!(by_key["name"], 4, "untagged aggregate");
        // No eager single-tag projection. summary::build_summary_data
        // synthesizes these on demand for threshold targets.
        assert!(!by_key.contains_key("name{a:1}"));
        assert!(!by_key.contains_key("name{b:2}"));
    }

    #[test]
    fn snapshot_does_not_double_count_untagged_sources() {
        // Per the CG-3 design review note: a stored UNTAGGED entry must
        // contribute exactly once to the untagged view, not also via the
        // "(b) emit untagged aggregate" rule. Before this guard, a
        // stored `name` (no tags) would have been emitted twice.
        let reg = MetricsRegistry::new();
        reg.counter_add("name", 5);
        let snap = reg.snapshot(1.0);
        let entries: Vec<_> = snap
            .counters
            .iter()
            .filter(|(k, _, _)| k == "name")
            .collect();
        assert_eq!(entries.len(), 1, "exactly one `name` entry");
        assert_eq!(entries[0].1, 5, "untagged value, NOT doubled to 10");
    }

    #[test]
    fn snapshot_single_tag_source_emits_passthrough_and_untagged_once() {
        // Same property the deleted `_does_not_double_project` test
        // checked, expressed against the new contract: a 1-tag stored
        // entry emits exactly one pass-through and one untagged
        // aggregate. No collision, no inflation.
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("k".to_string(), "v".to_string());
        for _ in 0..3 {
            reg.counter_add_with_tags("name", 1, &tags);
        }
        let snap = reg.snapshot(1.0);
        let by_key: std::collections::HashMap<&str, u64> = snap
            .counters
            .iter()
            .map(|(k, v, _)| (k.as_str(), *v))
            .collect();
        assert_eq!(by_key["name{k:v}"], 3);
        assert_eq!(by_key["name"], 3);
    }

    #[test]
    fn rate_subset_match_recomputes_rate_from_summed_passes_and_total() {
        // Rate aggregation must sum passes AND total, then recompute
        // — averaging ratios would be wrong if the buckets had unequal
        // sample counts.
        let reg = MetricsRegistry::new();
        let mut t1 = std::collections::BTreeMap::new();
        t1.insert("kind".to_string(), "a".to_string());
        let mut t2 = std::collections::BTreeMap::new();
        t2.insert("kind".to_string(), "b".to_string());

        // Bucket a: 10 samples, 5 passes → rate 0.5.
        for i in 0..10 {
            reg.rate_add_with_tags("ev", i % 2 == 0, &t1);
        }
        // Bucket b: 100 samples, 75 passes → rate 0.75.
        for i in 0..100 {
            reg.rate_add_with_tags("ev", i % 4 != 0, &t2);
        }

        // Untagged aggregate: (5 + 75) / (10 + 100) = 80/110 ≈ 0.727.
        let (rate, passes, total) = reg.rate_get("ev");
        assert_eq!(total, 110);
        assert_eq!(passes, 80);
        assert!((rate - 80.0 / 110.0).abs() < 1e-9, "got {rate}");
    }
}
