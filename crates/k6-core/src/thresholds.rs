use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::metrics::MetricsSnapshot;
use crate::selector::MetricSelector;

/// CG-4: a single threshold's configuration, parsed from either the legacy
/// string form (`'p(95)<2000'`) or the object form
/// (`{ threshold: 'p(95)<2000', abortOnFail: true, delayAbortEval: '500ms' }`).
/// `abort_on_fail` + `delay_abort_eval` together gate the periodic
/// evaluator's decision to cancel the run mid-flight.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    pub expression: String,
    pub abort_on_fail: bool,
    /// Grace period — the threshold must be continuously failing for at
    /// least this long before `abort_on_fail` actually triggers cancel.
    /// Matches upstream's `delayAbortEval`. Default is zero (immediate).
    pub delay_abort_eval: Duration,
}

impl Threshold {
    /// Construct from the legacy string form. `abort_on_fail = false`,
    /// `delay_abort_eval = 0`.
    pub fn from_expression(expression: impl Into<String>) -> Self {
        Self {
            expression: expression.into(),
            abort_on_fail: false,
            delay_abort_eval: Duration::ZERO,
        }
    }
}

/// CG-4: per-threshold lifetime state, tracked across periodic
/// evaluations. `failing_since` is the wall-clock instant at which the
/// threshold first observed a failing evaluation; it clears the moment
/// the threshold passes again, so the grace period only fires after
/// CONTINUOUS failure.
///
/// NOT serialized: keeping this internal avoids creating a conformance
/// asymmetry with upstream, whose `--summary-export` only emits the
/// final pass/fail. If/when a real consumer needs the timeline we can
/// surface it via a separate API.
#[derive(Debug, Default, Clone)]
pub struct ThresholdState {
    pub last_failed: bool,
    pub failing_since: Option<Instant>,
}

/// CG-4: outcome of one periodic update_states tick. The evaluator
/// computes this for ALL thresholds before reporting it back — the
/// caller never sees a half-updated state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickDecision {
    /// No abort-on-fail threshold has crossed its grace window. Run continues.
    Continue,
    /// At least one abort-on-fail threshold has been failing for at
    /// least its `delay_abort_eval`. Caller should cancel the run.
    Abort,
}

/// Result of evaluating all thresholds.
#[derive(Debug)]
pub struct ThresholdResults {
    pub results: Vec<ThresholdResult>,
}

/// Result of a single threshold evaluation.
#[derive(Debug)]
pub struct ThresholdResult {
    pub metric: String,
    pub expression: String,
    pub passed: bool,
    pub actual_value: f64,
}

impl ThresholdResults {
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }
}

/// A parsed threshold condition.
#[derive(Debug, Clone)]
struct ThresholdCondition {
    stat: ThresholdStat,
    op: ThresholdOp,
    value: f64,
}

#[derive(Debug, Clone)]
enum ThresholdStat {
    /// Counter value
    Count,
    /// Counter rate per second
    Rate,
    /// Trend: avg, min, max, med
    Avg,
    Min,
    Max,
    Med,
    /// Trend percentile: p(90), p(95), p(99)
    Percentile(f64),
    /// Rate metric: the pass rate (0.0 to 1.0)
    Value,
}

#[derive(Debug, Clone)]
enum ThresholdOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Neq,
}

/// Evaluate all thresholds against a metrics snapshot.
///
/// `thresholds` maps metric name → list of `Threshold` configs. The
/// returned `ThresholdResults` carries one entry per (metric, expression)
/// pair, sorted by metric name for deterministic order.
pub fn evaluate(
    thresholds: &HashMap<String, Vec<Threshold>>,
    snapshot: &MetricsSnapshot,
) -> ThresholdResults {
    let mut results = Vec::new();

    for (metric, conditions) in thresholds {
        for t in conditions {
            let result = evaluate_one(metric, &t.expression, snapshot);
            results.push(result);
        }
    }

    results.sort_by(|a, b| a.metric.cmp(&b.metric));

    ThresholdResults { results }
}

/// CG-4: one tick of the periodic threshold evaluator. Updates `states`
/// in place AND returns a `TickDecision` indicating whether at least one
/// `abort_on_fail` threshold has been failing long enough to trigger
/// cancel.
///
/// Two phases, deterministic order:
///   1. Update every threshold's `(last_failed, failing_since)` from the
///      snapshot. No early-exit — every threshold sees the same snapshot.
///   2. Decide cancel by scanning the now-updated states. The decision
///      is independent of map iteration order.
///
/// Caller is responsible for keeping `states` in lockstep with
/// `thresholds`: the same (metric, expression) key shape across ticks.
/// The map key is `format!("{metric}::{expression}")` — colon-separated
/// to avoid colliding with metric names that contain other punctuation.
pub fn update_states(
    thresholds: &HashMap<String, Vec<Threshold>>,
    snapshot: &MetricsSnapshot,
    states: &mut HashMap<String, ThresholdState>,
    now: Instant,
) -> TickDecision {
    // Phase 1: update every state from the same snapshot.
    for (metric, configs) in thresholds {
        for t in configs {
            let result = evaluate_one(metric, &t.expression, snapshot);
            let key = state_key(metric, &t.expression);
            let state = states.entry(key).or_default();
            state.last_failed = !result.passed;
            if state.last_failed {
                if state.failing_since.is_none() {
                    state.failing_since = Some(now);
                }
            } else {
                state.failing_since = None;
            }
        }
    }

    // Phase 2: decide cancel. Scan AFTER all updates so the decision
    // doesn't depend on map iteration order or short-circuit on the
    // first failing threshold seen.
    let mut should_abort = false;
    for (metric, configs) in thresholds {
        for t in configs {
            if !t.abort_on_fail {
                continue;
            }
            let key = state_key(metric, &t.expression);
            let Some(state) = states.get(&key) else {
                continue;
            };
            let Some(failing_since) = state.failing_since else {
                continue;
            };
            if now.saturating_duration_since(failing_since) >= t.delay_abort_eval {
                should_abort = true;
                // No early break — keep scanning so downstream telemetry /
                // logging can observe every threshold that would have
                // triggered abort, not just the first one.
            }
        }
    }
    if should_abort {
        TickDecision::Abort
    } else {
        TickDecision::Continue
    }
}

fn state_key(metric: &str, expression: &str) -> String {
    format!("{metric}::{expression}")
}

fn evaluate_one(metric: &str, expr: &str, snapshot: &MetricsSnapshot) -> ThresholdResult {
    let parsed = match parse_condition(expr) {
        Some(c) => c,
        None => {
            return ThresholdResult {
                metric: metric.to_string(),
                expression: expr.to_string(),
                passed: false,
                actual_value: 0.0,
            };
        }
    };

    let actual = resolve_stat(metric, &parsed.stat, snapshot);
    let passed = compare(actual, &parsed.op, parsed.value);

    ThresholdResult {
        metric: metric.to_string(),
        expression: expr.to_string(),
        passed,
        actual_value: actual,
    }
}

/// Compare a threshold key against a stored metric name. CG-5: normalizes
/// both sides through `MetricSelector::canonical` so equivalent selectors
/// (different tag order, different whitespace) match. If either side fails
/// to parse — e.g. a CG-3 future encoding the parser doesn't recognize, or
/// legacy hand-constructed test names — fall back to literal string
/// equality so callers don't silently miss a stored value.
fn metric_names_match(threshold_key: &str, stored: &str) -> bool {
    if threshold_key == stored {
        return true; // fast path
    }
    match (
        MetricSelector::parse(threshold_key),
        MetricSelector::parse(stored),
    ) {
        (Ok(a), Ok(b)) => a.canonical() == b.canonical(),
        _ => false,
    }
}

/// CG-3: stored entry's tag set is a SUPERSET of the threshold's. Used as
/// a fallback when canonical equality misses — a threshold targeting
/// `name{a:1}` aggregates across all stored entries with `a:1` regardless
/// of which other tags they carry. Mirrors the live-read semantics in
/// `MetricsRegistry::counter_get` etc.
fn stored_is_subset_match(threshold_key: &str, stored: &str) -> bool {
    let Ok(q) = MetricSelector::parse(threshold_key) else {
        return false;
    };
    let Ok(s) = MetricSelector::parse(stored) else {
        return false;
    };
    if s.name != q.name {
        return false;
    }
    q.tags.iter().all(|(k, v)| s.tags.get(k) == Some(v))
}

/// Result of a subset-merge over snapshot trends for a threshold target.
/// Bundles the merged primitives (count/sum/min/max) AND either a merged
/// histogram or precomputed-stats fallback so the threshold evaluator can
/// answer arbitrary `p(x)` queries even on hand-constructed snapshots that
/// don't carry histograms (only precomputed `TrendStats`).
struct MergedTrend {
    count: u64,
    avg: f64,
    min: f64,
    max: f64,
    /// Merged HDR histogram across subset matches that supplied one.
    /// `None` (or empty) means no match exposed a histogram — fall back
    /// to the precomputed fields below for percentile answers.
    histogram: Option<hdrhistogram::Histogram<u64>>,
    /// Precomputed percentile stats from the first subset match — used
    /// when the merged histogram is unavailable/empty. Preserves the
    /// "hand-built snapshot with stats-only entries" case (legacy
    /// threshold tests + the engine's own snapshot construction before
    /// CG-pre P0 wired histograms into every TrendStats slot).
    fallback_med: f64,
    fallback_p90: f64,
    fallback_p95: f64,
    fallback_p99: f64,
}

/// CG-3: subset-merge over snapshot trends. Returns `None` if no stored
/// entry has tags that are a superset of the threshold's.
fn merged_trend_for_threshold(
    threshold_key: &str,
    snapshot: &MetricsSnapshot,
) -> Option<MergedTrend> {
    let mut count: u64 = 0;
    let mut sum: f64 = 0.0;
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    let mut hist: Option<hdrhistogram::Histogram<u64>> = None;
    let mut fallback: Option<crate::metrics::TrendStats> = None;
    for (stored, stats) in &snapshot.trends {
        if !stored_is_subset_match(threshold_key, stored) {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(stats.clone());
        }
        count += stats.count;
        sum += stats.avg * stats.count as f64;
        if stats.min < min {
            min = stats.min;
        }
        if stats.max > max {
            max = stats.max;
        }
        if let Some(h) = snapshot.trend_histograms.get(stored) {
            match hist.as_mut() {
                Some(m) => {
                    let _ = m.add(h);
                }
                None => hist = Some(h.clone()),
            }
        }
    }
    let fb = fallback?; // None if no subset-match found
    let avg = if count > 0 { sum / count as f64 } else { 0.0 };
    Some(MergedTrend {
        count,
        avg,
        min: if min == f64::MAX { 0.0 } else { min },
        max: if max == f64::MIN { 0.0 } else { max },
        histogram: hist,
        fallback_med: fb.med,
        fallback_p90: fb.p90,
        fallback_p95: fb.p95,
        fallback_p99: fb.p99,
    })
}

/// CG-3: same subset semantics for counters. Returns (value, rate) or None.
fn merged_counter_for_threshold(
    threshold_key: &str,
    snapshot: &MetricsSnapshot,
) -> Option<(u64, f64)> {
    let mut value: u64 = 0;
    let mut rate: f64 = 0.0;
    let mut had = false;
    for (stored, v, r) in &snapshot.counters {
        if !stored_is_subset_match(threshold_key, stored) {
            continue;
        }
        had = true;
        value += *v;
        rate += *r;
    }
    if had { Some((value, rate)) } else { None }
}

/// CG-3: same subset semantics for rates. Returns (rate, passes, total).
fn merged_rate_for_threshold(
    threshold_key: &str,
    snapshot: &MetricsSnapshot,
) -> Option<(f64, u64, u64)> {
    let mut passes: u64 = 0;
    let mut total: u64 = 0;
    let mut had = false;
    for (stored, _r, p, t) in &snapshot.rates {
        if !stored_is_subset_match(threshold_key, stored) {
            continue;
        }
        had = true;
        passes += *p;
        total += *t;
    }
    if !had {
        return None;
    }
    let rate = if total > 0 {
        passes as f64 / total as f64
    } else {
        0.0
    };
    Some((rate, passes, total))
}

fn resolve_stat(metric: &str, stat: &ThresholdStat, snapshot: &MetricsSnapshot) -> f64 {
    // CG-3: subset-merge across every stored entry whose tags are a
    // non-strict superset of the threshold's tags. The canonical-equal
    // entry (when present) is naturally one of the matches, so it's
    // included in the merge — earlier versions skipped subset-merge
    // when an exact match existed, which under-counted mixed-cardinality
    // storage (e.g. both `name{a:1}` and `name{a:1,b:2}` stored, threshold
    // on `name{a:1}` ignored the second series). The merge below picks
    // up both.
    let pre_synth_trend = merged_trend_for_threshold(metric, snapshot);
    let pre_synth_counter = merged_counter_for_threshold(metric, snapshot);
    let pre_synth_rate = merged_rate_for_threshold(metric, snapshot);

    // Synthesized trend (subset merge) handled first. Percentile queries
    // prefer the merged HDR histogram when one is available (gives
    // arbitrary p(x) without snapping to the four precomputed
    // percentiles); fall back to precomputed fields when no match in the
    // subset carried a histogram — preserves the engine's own legacy
    // snapshot shape and the hand-constructed threshold-test fixtures.
    if let Some(synth) = pre_synth_trend.as_ref() {
        let percentile_from = |p: f64| -> f64 {
            if let Some(h) = synth.histogram.as_ref() {
                if h.len() > 0 {
                    let q = (p / 100.0).clamp(0.0, 1.0);
                    return h.value_at_quantile(q) as f64 / 1000.0;
                }
            }
            if (p - 50.0).abs() < 0.5 {
                synth.fallback_med
            } else if (p - 90.0).abs() < 0.5 {
                synth.fallback_p90
            } else if (p - 95.0).abs() < 0.5 {
                synth.fallback_p95
            } else if (p - 99.0).abs() < 0.5 {
                synth.fallback_p99
            } else {
                synth.fallback_p95
            }
        };
        return match stat {
            ThresholdStat::Avg => synth.avg,
            ThresholdStat::Min => synth.min,
            ThresholdStat::Max => synth.max,
            ThresholdStat::Med => percentile_from(50.0),
            ThresholdStat::Percentile(p) => percentile_from(*p),
            ThresholdStat::Count => synth.count as f64,
            ThresholdStat::Value => synth.avg,
            _ => synth.avg,
        };
    }
    if let Some((value, rate)) = pre_synth_counter {
        return match stat {
            ThresholdStat::Count | ThresholdStat::Value => value as f64,
            ThresholdStat::Rate => rate,
            _ => value as f64,
        };
    }
    if let Some((rate, _passes, total)) = pre_synth_rate {
        return match stat {
            ThresholdStat::Rate | ThresholdStat::Value => rate,
            ThresholdStat::Count => total as f64,
            _ => rate,
        };
    }

    // Check if it's a trend metric. Selector-aware match: tag-order or
    // whitespace differences between threshold key and storage key no
    // longer cause silent misses.
    if let Some((stored_name, stats)) = snapshot
        .trends
        .iter()
        .find(|(n, _)| metric_names_match(metric, n))
    {
        return match stat {
            ThresholdStat::Avg => stats.avg,
            ThresholdStat::Min => stats.min,
            ThresholdStat::Max => stats.max,
            ThresholdStat::Med => stats.med,
            ThresholdStat::Percentile(p) => {
                // Real arbitrary-percentile lookup against the snapshot's
                // cloned HDR histogram. Falls back to the fixed snapshot
                // stats only when the histogram is missing (e.g. legacy
                // tests constructing TrendStats by hand without a backing
                // registry); in production every trend has a histogram.
                // CG-5: feed the histogram lookup the snapshot's actual
                // stored name, not the (possibly differently-canonicalized)
                // threshold key.
                if let Some(v) = crate::metrics::snapshot_percentile_ms(snapshot, stored_name, *p) {
                    return v;
                }
                if (*p - 90.0).abs() < 0.5 {
                    stats.p90
                } else if (*p - 95.0).abs() < 0.5 {
                    stats.p95
                } else if (*p - 99.0).abs() < 0.5 {
                    stats.p99
                } else {
                    stats.p95
                }
            }
            ThresholdStat::Count => stats.count as f64,
            ThresholdStat::Value => stats.avg, // fallback
            _ => stats.avg,
        };
    }

    // Check counter metrics
    if let Some((_, value, rate)) = snapshot
        .counters
        .iter()
        .find(|(n, _, _)| metric_names_match(metric, n))
    {
        return match stat {
            ThresholdStat::Count | ThresholdStat::Value => *value as f64,
            ThresholdStat::Rate => *rate,
            _ => *value as f64,
        };
    }

    // Check rate metrics (like http_req_failed, checks)
    if let Some((_, rate, _passes, total)) = snapshot
        .rates
        .iter()
        .find(|(n, _, _, _)| metric_names_match(metric, n))
    {
        return match stat {
            ThresholdStat::Rate | ThresholdStat::Value => *rate,
            ThresholdStat::Count => *total as f64,
            _ => *rate,
        };
    }

    // Check gauge metrics
    if let Some((_, value, min, max)) = snapshot
        .gauges
        .iter()
        .find(|(n, _, _, _)| metric_names_match(metric, n))
    {
        return match stat {
            ThresholdStat::Min => *min,
            ThresholdStat::Max => *max,
            ThresholdStat::Value => *value,
            _ => *value,
        };
    }

    0.0
}

fn compare(actual: f64, op: &ThresholdOp, expected: f64) -> bool {
    match op {
        ThresholdOp::Lt => actual < expected,
        ThresholdOp::Lte => actual <= expected,
        ThresholdOp::Gt => actual > expected,
        ThresholdOp::Gte => actual >= expected,
        ThresholdOp::Eq => (actual - expected).abs() < f64::EPSILON,
        ThresholdOp::Neq => (actual - expected).abs() >= f64::EPSILON,
    }
}

/// Parse a threshold expression like "p(95)<2000", "rate<0.01", "avg<500".
fn parse_condition(expr: &str) -> Option<ThresholdCondition> {
    let expr = expr.trim();

    // Find the operator position
    let (stat_str, op, value_str) = parse_op(expr)?;

    let stat = parse_stat(stat_str.trim())?;
    let value: f64 = value_str.trim().parse().ok()?;

    Some(ThresholdCondition { stat, op, value })
}

fn parse_op(expr: &str) -> Option<(&str, ThresholdOp, &str)> {
    // Order matters: check two-char ops first
    for (pattern, op) in [
        ("===", ThresholdOp::Eq),
        ("<=", ThresholdOp::Lte),
        (">=", ThresholdOp::Gte),
        ("!=", ThresholdOp::Neq),
        ("==", ThresholdOp::Eq),
        ("<", ThresholdOp::Lt),
        (">", ThresholdOp::Gt),
    ] {
        if let Some(pos) = expr.find(pattern) {
            let left = &expr[..pos];
            let right = &expr[pos + pattern.len()..];
            return Some((left, op, right));
        }
    }
    None
}

fn parse_stat(s: &str) -> Option<ThresholdStat> {
    let s = s.trim();
    match s {
        "avg" => Some(ThresholdStat::Avg),
        "min" => Some(ThresholdStat::Min),
        "max" => Some(ThresholdStat::Max),
        "med" => Some(ThresholdStat::Med),
        "count" => Some(ThresholdStat::Count),
        "rate" => Some(ThresholdStat::Rate),
        "value" => Some(ThresholdStat::Value),
        _ if s.starts_with("p(") && s.ends_with(')') => {
            let inner = &s[2..s.len() - 1];
            let p: f64 = inner.parse().ok()?;
            Some(ThresholdStat::Percentile(p))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::TrendStats;

    fn sample_snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![
                ("http_reqs".to_string(), 1000, 100.0),
                ("iterations".to_string(), 500, 50.0),
            ],
            gauges: vec![("vus".to_string(), 10.0, 1.0, 10.0)],
            rates: vec![
                ("checks".to_string(), 0.98, 980, 1000),
                ("http_req_failed".to_string(), 0.005, 5, 1000),
            ],
            trends: vec![
                (
                    "http_req_duration".to_string(),
                    TrendStats {
                        p99: 0.0,
                        avg: 150.0,
                        min: 10.0,
                        med: 120.0,
                        max: 2500.0,
                        p90: 350.0,
                        p95: 800.0,
                        count: 1000,
                    },
                ),
                (
                    "iteration_duration".to_string(),
                    TrendStats {
                        p99: 0.0,
                        avg: 200.0,
                        min: 50.0,
                        med: 180.0,
                        max: 3000.0,
                        p90: 500.0,
                        p95: 1200.0,
                        count: 500,
                    },
                ),
            ],
        }
    }

    #[test]
    fn parse_p95_less_than() {
        let cond = parse_condition("p(95)<2000").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Percentile(p) if (p - 95.0).abs() < 0.01));
        assert!(matches!(cond.op, ThresholdOp::Lt));
        assert!((cond.value - 2000.0).abs() < 0.01);
    }

    #[test]
    fn parse_rate_less_than() {
        let cond = parse_condition("rate<0.01").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Rate));
        assert!((cond.value - 0.01).abs() < 0.001);
    }

    #[test]
    fn parse_avg_less_equal() {
        let cond = parse_condition("avg<=500").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Avg));
        assert!(matches!(cond.op, ThresholdOp::Lte));
    }

    #[test]
    fn parse_count_greater_than() {
        let cond = parse_condition("count>100").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Count));
        assert!(matches!(cond.op, ThresholdOp::Gt));
    }

    #[test]
    fn parse_strict_equality_like_upstream() {
        // Upstream accepts both `==` and `===` threshold equality operators.
        // They are numerically equivalent here because threshold operands are
        // already parsed as f64.
        let cond = parse_condition("count===20").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Count));
        assert!(matches!(cond.op, ThresholdOp::Eq));
        assert!((cond.value - 20.0).abs() < 0.01);

        let cond = parse_condition("rate == 0.5").unwrap();
        assert!(matches!(cond.stat, ThresholdStat::Rate));
        assert!(matches!(cond.op, ThresholdOp::Eq));
        assert!((cond.value - 0.5).abs() < 0.01);
    }

    #[test]
    fn parse_invalid_threshold_expressions_fail() {
        // Port of upstream thresholds_parser invalid syntax cases.
        assert!(parse_condition("count!20").is_none());
        assert!(parse_condition("foo>20").is_none());
        assert!(parse_condition("count>abc").is_none());
        assert!(parse_condition("p() < 10").is_none());
        assert!(parse_condition("p(foo)<10").is_none());
        assert!(parse_condition("p(99<10").is_none());
    }

    #[test]
    fn threshold_p95_passes() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec![Threshold::from_expression("p(95)<2000")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
        assert_eq!(results.results.len(), 1);
        assert!((results.results[0].actual_value - 800.0).abs() < 1.0);
    }

    #[test]
    fn threshold_p95_fails() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec![Threshold::from_expression("p(95)<500")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());
    }

    #[test]
    fn threshold_rate_metric() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_failed".to_string(),
            vec![Threshold::from_expression("rate<0.01")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
        assert!((results.results[0].actual_value - 0.005).abs() < 0.001);
    }

    #[test]
    fn threshold_rate_metric_fails() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_failed".to_string(),
            vec![Threshold::from_expression("rate<0.001")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());
    }

    #[test]
    fn threshold_checks_rate() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "checks".to_string(),
            vec![Threshold::from_expression("rate>0.95")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
    }

    #[test]
    fn threshold_counter_count() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_reqs".to_string(),
            vec![Threshold::from_expression("count>500")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
    }

    #[test]
    fn threshold_avg_and_max() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec![
                Threshold::from_expression("avg<200"),
                Threshold::from_expression("max<5000"),
            ],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
        assert_eq!(results.results.len(), 2);
    }

    #[test]
    fn multiple_thresholds_partial_fail() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec![
                Threshold::from_expression("p(95)<2000"),
                Threshold::from_expression("avg<100"),
            ], // avg=150 > 100
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());

        let passed_count = results.results.iter().filter(|r| r.passed).count();
        assert_eq!(passed_count, 1);
    }

    #[test]
    fn threshold_arbitrary_percentile_against_real_histogram() {
        // P0.2 regression: a threshold like `p(99)<950` must evaluate the
        // ACTUAL p99 from the underlying histogram, not snap to p95. Before
        // this fix, p(99) silently took stats.p95 as a fallback — so a test
        // for "p(99)<X" could pass spuriously when p99 was actually above X
        // (or vice versa) just because p95 happened to be on the right side.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        for i in 1..=1000 {
            reg.trend_add("latency", i as f64);
        }
        let snapshot = reg.snapshot(1.0);

        // p95 ≈ 950, p99 ≈ 990. A threshold of "p(99)<950" must FAIL
        // (because real p99 is ~990) — under the old code it would PASS
        // because p99 fell back to p95 (~950, just under 950).
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "latency".to_string(),
            vec![Threshold::from_expression("p(99)<950")],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(
            !results.all_passed(),
            "p(99)<950 must fail when real p99 is ~990; got actual_value={}",
            results.results[0].actual_value
        );
        assert!(
            (results.results[0].actual_value - 990.0).abs() < 5.0,
            "p(99) should be ~990, got {}",
            results.results[0].actual_value
        );

        // Companion: a non-round percentile like p(33) used to silently
        // snap to p95 (~950). Now it should report ~330.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "latency".to_string(),
            vec![Threshold::from_expression("p(33)<400")],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(results.all_passed(), "p(33)<400 should pass; p33 ≈ 330");
        assert!(
            (results.results[0].actual_value - 330.0).abs() < 5.0,
            "p(33) should be ~330, got {}",
            results.results[0].actual_value
        );
    }

    #[test]
    fn threshold_matches_storage_with_different_tag_order() {
        // CG-5 bug fix: threshold key and storage key carrying the same tag
        // set in different orders (or with whitespace) used to silently
        // miss each other — `n == metric` is byte-comparison. With the
        // selector-aware lookup, both canonicalize to `name{a:1,b:2}` and
        // match. Before CG-5 this test would have returned 0.0 (unknown
        // metric falls through to default).
        use crate::metrics::TrendStats;
        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![(
                // Storage keys tags in alphabetical order (matches CG-5
                // canonical form).
                "http_req_duration{a:1,b:2}".to_string(),
                TrendStats {
                    avg: 500.0,
                    min: 10.0,
                    med: 400.0,
                    max: 5000.0,
                    p90: 1000.0,
                    p95: 2000.0,
                    p99: 0.0,
                    count: 1000,
                },
            )],
        };

        // Threshold key carries tags in (b,a) order — different from storage.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration{b:2,a:1}".to_string(),
            vec![Threshold::from_expression("p(95)<2500")],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(
            results.all_passed(),
            "selector-aware match must find the stored metric despite tag order"
        );
        assert!(
            (results.results[0].actual_value - 2000.0).abs() < 1.0,
            "actual value should come from the stored submetric, got {}",
            results.results[0].actual_value
        );

        // Companion: same threshold but with extra whitespace around
        // separators. Canonical normalization strips it; lookup still
        // succeeds.
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration{ b : 2 , a : 1 }".to_string(),
            vec![Threshold::from_expression("p(95)<2500")],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(results.all_passed(), "whitespace tolerance must work too");
    }

    #[test]
    fn threshold_targets_full_tag_combination_after_cg3() {
        // CG-3 bug fix on real data: before CG-3, a sample tagged with
        // {a:1,b:2} was decomposed into independent single-tag submetrics
        // `name{a:1}` and `name{b:2}`. A threshold targeting the full
        // combination `name{a:1,b:2}` would silently miss (lookup returns
        // 0). After CG-3, storage keys by the canonical full-tag form,
        // snapshot derivation passes it through, and the threshold finds
        // its target with correct stats.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("a".to_string(), "1".to_string());
        tags.insert("b".to_string(), "2".to_string());
        // 100 samples in [1..=100] ms — avg ≈ 50.5, count = 100.
        for ms in 1..=100 {
            reg.trend_add_with_tags("name", ms as f64, &tags);
        }
        let snapshot = reg.snapshot(1.0);

        let mut thresholds = HashMap::new();
        thresholds.insert(
            "name{a:1,b:2}".to_string(),
            vec![
                Threshold::from_expression("count>0"),
                Threshold::from_expression("avg<60"),
            ],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(
            results.all_passed(),
            "full-tag threshold must find its target after CG-3; got {results:?}"
        );
        // The avg threshold's actual_value reads from the merged
        // pass-through entry, not a stale 0.0.
        let avg_result = results
            .results
            .iter()
            .find(|r| r.expression == "avg<60")
            .unwrap();
        assert!(
            (avg_result.actual_value - 50.5).abs() < 1.0,
            "avg ≈ 50.5 for 1..=100, got {}",
            avg_result.actual_value
        );
    }

    #[test]
    fn threshold_subset_merges_across_exact_and_superset_stored_entries() {
        // CG-3 follow-up bug: when storage holds BOTH an exact-match entry
        // and a wider-cardinality superset (e.g. `name{a:1}` and
        // `name{a:1,b:2}`), a threshold on `name{a:1}` must aggregate
        // across both — the second's tags are a superset of the query's.
        // Earlier resolve_stat skipped subset-merge if any canonical-equal
        // entry existed, undercounting mixed-cardinality data.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        let mut one_tag = std::collections::BTreeMap::new();
        one_tag.insert("a".to_string(), "1".to_string());
        for _ in 0..5 {
            reg.counter_add_with_tags("name", 1, &one_tag); // → name{a:1}
        }
        let mut two_tag = std::collections::BTreeMap::new();
        two_tag.insert("a".to_string(), "1".to_string());
        two_tag.insert("b".to_string(), "2".to_string());
        for _ in 0..3 {
            reg.counter_add_with_tags("name", 1, &two_tag); // → name{a:1,b:2}
        }
        let snapshot = reg.snapshot(1.0);

        let mut thresholds = HashMap::new();
        thresholds.insert(
            "name{a:1}".to_string(),
            vec![Threshold::from_expression("count==8")], // 5 + 3
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(
            results.all_passed(),
            "threshold must subset-merge across exact + superset stored entries; got {results:?}"
        );
        assert!(
            (results.results[0].actual_value - 8.0).abs() < 0.01,
            "expected count=8 (5 exact + 3 superset), got {}",
            results.results[0].actual_value
        );
    }

    #[test]
    fn threshold_parse_failure_falls_back_to_literal_equality() {
        // CG-5 defensive: if a stored name doesn't parse as a selector
        // (e.g. some future encoding the parser doesn't recognize yet),
        // the literal-equality fast path still finds it. Without this,
        // changes to storage shape could silently regress threshold
        // lookup.
        use crate::metrics::TrendStats;
        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![(
                // A storage key with content the selector parser would
                // reject (stray `}` makes parsing fail). Both sides are
                // byte-equal, so literal equality still matches.
                "weird}name".to_string(),
                TrendStats {
                    avg: 42.0,
                    min: 1.0,
                    med: 40.0,
                    max: 100.0,
                    p90: 80.0,
                    p95: 90.0,
                    p99: 0.0,
                    count: 10,
                },
            )],
        };
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "weird}name".to_string(),
            vec![Threshold::from_expression("avg<50")],
        );
        let results = evaluate(&thresholds, &snapshot);
        assert!(results.all_passed());
        assert!((results.results[0].actual_value - 42.0).abs() < 0.01);
    }

    // --- CG-4: threshold lifecycle ---

    /// Build a one-entry thresholds map keyed by `metric` carrying one
    /// `Threshold` config — minor test ergonomic.
    fn one_threshold(
        metric: &str,
        expression: &str,
        abort_on_fail: bool,
        delay_abort_eval: Duration,
    ) -> HashMap<String, Vec<Threshold>> {
        let mut m = HashMap::new();
        m.insert(
            metric.to_string(),
            vec![Threshold {
                expression: expression.to_string(),
                abort_on_fail,
                delay_abort_eval,
            }],
        );
        m
    }

    fn snapshot_with_counter(name: &str, value: u64) -> MetricsSnapshot {
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        reg.counter_add(name, value);
        reg.snapshot(1.0)
    }

    #[test]
    fn state_marks_failing_since_on_first_failure() {
        // CG-4 regression: the first time `update_states` observes a
        // failing threshold, it records `failing_since = now`. Until the
        // grace period elapses, `update_states` returns Continue even
        // though the threshold is failing.
        let thresholds = one_threshold(
            "http_reqs",
            "count==999",
            true,                       // abort_on_fail
            Duration::from_millis(500), // grace
        );
        let snap = snapshot_with_counter("http_reqs", 10); // count==999 fails
        let mut states = HashMap::new();
        let t0 = Instant::now();

        let decision = update_states(&thresholds, &snap, &mut states, t0);
        // failing_since just set, grace not elapsed → no abort yet.
        assert_eq!(decision, TickDecision::Continue);
        let state = states
            .get("http_reqs::count==999")
            .expect("state recorded under composite key");
        assert!(state.last_failed);
        assert!(state.failing_since.is_some());
        assert_eq!(state.failing_since.unwrap(), t0);
    }

    #[test]
    fn state_clears_failing_since_when_recovers() {
        // CG-4 regression: the grace period must restart if a threshold
        // ever passes again. Continuous failure is required for abort.
        let thresholds = one_threshold("http_reqs", "count<100", true, Duration::from_secs(1));
        let mut states = HashMap::new();
        let t0 = Instant::now();

        // Tick 1: count=500 → fails (500 < 100 is false).
        let snap_fail = snapshot_with_counter("http_reqs", 500);
        update_states(&thresholds, &snap_fail, &mut states, t0);
        assert!(states["http_reqs::count<100"].last_failed);
        assert!(states["http_reqs::count<100"].failing_since.is_some());

        // Tick 2: count=10 → passes (10 < 100). failing_since clears.
        let snap_pass = snapshot_with_counter("http_reqs", 10);
        update_states(
            &thresholds,
            &snap_pass,
            &mut states,
            t0 + Duration::from_millis(200),
        );
        assert!(!states["http_reqs::count<100"].last_failed);
        assert!(states["http_reqs::count<100"].failing_since.is_none());
    }

    #[test]
    fn abort_triggers_only_after_grace_period() {
        // CG-4 bug fix regression: with `delay_abort_eval = 500ms`, the
        // first failing tick records `failing_since` but returns
        // `Continue`. A later tick AFTER the grace window has elapsed
        // returns `Abort`. Without this gating, abortOnFail would fire
        // on the very first failing eval — defeating the purpose of
        // `delayAbortEval`.
        let thresholds = one_threshold("http_reqs", "count<10", true, Duration::from_millis(500));
        let snap = snapshot_with_counter("http_reqs", 100); // 100 < 10 → fail
        let mut states = HashMap::new();
        let t0 = Instant::now();

        // First failing tick: state recorded, NO abort yet.
        let d1 = update_states(&thresholds, &snap, &mut states, t0);
        assert_eq!(d1, TickDecision::Continue);

        // 200ms later: still failing, still inside grace.
        let d2 = update_states(
            &thresholds,
            &snap,
            &mut states,
            t0 + Duration::from_millis(200),
        );
        assert_eq!(d2, TickDecision::Continue);

        // 600ms later: outside grace window — abort fires.
        let d3 = update_states(
            &thresholds,
            &snap,
            &mut states,
            t0 + Duration::from_millis(600),
        );
        assert_eq!(d3, TickDecision::Abort);
    }

    #[test]
    fn abort_only_triggers_for_thresholds_with_abort_on_fail() {
        // CG-4: a failing threshold WITHOUT abort_on_fail must NEVER
        // cause cancel — it just records its state for end-of-run eval.
        let thresholds = one_threshold(
            "http_reqs",
            "count<10",
            false, // abort_on_fail OFF
            Duration::from_millis(0),
        );
        let snap = snapshot_with_counter("http_reqs", 100);
        let mut states = HashMap::new();
        let t0 = Instant::now();

        let d = update_states(
            &thresholds,
            &snap,
            &mut states,
            t0 + Duration::from_secs(60), // even way after grace window
        );
        assert_eq!(d, TickDecision::Continue);
        assert!(states["http_reqs::count<10"].last_failed);
    }

    #[test]
    fn periodic_eval_requires_real_elapsed_for_rate_thresholds() {
        // CG-4 follow-up regression: counter-rate thresholds depend on the
        // snapshot's `duration_secs` (rate = value / duration_secs). When
        // a caller takes a periodic snapshot with `duration_secs == 0.0`,
        // every counter's rate field collapses to 0.0 and a `rate<X` /
        // `rate>X` threshold sees the wrong value mid-run. The fix in
        // main.rs is to pass elapsed wall-clock time; this test guards
        // the underlying contract by demonstrating both behaviors.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        for _ in 0..100 {
            reg.counter_add("http_reqs", 1);
        }

        // Threshold: aborts if rate is below 10/s. With 100 events over
        // 5s the real rate is 20/s — should pass. With duration=0 the
        // snapshot reports rate=0 → threshold fails → abort would fire.
        let thresholds = one_threshold("http_reqs", "rate>10", true, Duration::ZERO);

        // Wrong: snapshot with zero duration (the bug).
        let snap_zero = reg.snapshot(0.0);
        let mut states_zero = HashMap::new();
        let d_zero = update_states(&thresholds, &snap_zero, &mut states_zero, Instant::now());
        assert_eq!(
            d_zero,
            TickDecision::Abort,
            "with duration=0 the rate collapses to 0, threshold rate>10 fails, abort fires — this is the bug the fix avoids"
        );

        // Correct: snapshot with real elapsed time. 100 reqs / 5s = 20/s.
        let snap_real = reg.snapshot(5.0);
        let mut states_real = HashMap::new();
        let d_real = update_states(&thresholds, &snap_real, &mut states_real, Instant::now());
        assert_eq!(
            d_real,
            TickDecision::Continue,
            "with elapsed duration the rate is 20/s, threshold rate>10 passes, no abort"
        );
    }

    #[test]
    fn update_states_evaluates_all_before_deciding_cancel() {
        // CG-4 design invariant: phase 1 (state updates) completes for
        // every threshold before phase 2 (cancel decision) runs. This
        // ensures the decision doesn't depend on map iteration order
        // — if any abort_on_fail threshold has crossed its grace
        // window, abort fires regardless of where it sits in the map.
        let mut thresholds: HashMap<String, Vec<Threshold>> = HashMap::new();
        // First (in insertion order): a NON-aborting threshold that's failing.
        thresholds.insert(
            "iterations".to_string(),
            vec![Threshold {
                expression: "count>9999".to_string(),
                abort_on_fail: false,
                delay_abort_eval: Duration::ZERO,
            }],
        );
        // Second: an aborting threshold that's failing (zero grace).
        thresholds.insert(
            "http_reqs".to_string(),
            vec![Threshold {
                expression: "count<5".to_string(),
                abort_on_fail: true,
                delay_abort_eval: Duration::ZERO,
            }],
        );

        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        reg.counter_add("http_reqs", 100); // 100 < 5 → fail
        reg.counter_add("iterations", 50); // 50 > 9999 → fail
        let snap = reg.snapshot(1.0);

        let mut states = HashMap::new();
        let d = update_states(&thresholds, &snap, &mut states, Instant::now());
        // The aborting threshold is failing AND past zero grace → abort.
        assert_eq!(d, TickDecision::Abort);
        // BOTH states updated, regardless of which one drove the abort decision.
        assert!(states["http_reqs::count<5"].last_failed);
        assert!(states["iterations::count>9999"].last_failed);
    }

    #[test]
    fn invalid_expression_fails() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec![Threshold::from_expression("invalid_expr")],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());
    }

    #[test]
    fn unknown_metric_returns_zero() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "nonexistent_metric".to_string(),
            vec![Threshold::from_expression("avg<100")],
        );

        let results = evaluate(&thresholds, &snap);
        // 0.0 < 100 → passes
        assert!(results.all_passed());
        assert!((results.results[0].actual_value - 0.0).abs() < 0.01);
    }

    #[test]
    fn threshold_with_tag_filter() {
        // Simulate tagged metrics in snapshot
        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![
                (
                    "http_req_duration".to_string(),
                    TrendStats {
                        p99: 0.0,
                        avg: 500.0,
                        min: 10.0,
                        med: 400.0,
                        max: 5000.0,
                        p90: 1000.0,
                        p95: 2000.0,
                        count: 1000,
                    },
                ),
                (
                    "http_req_duration{scenario:light}".to_string(),
                    TrendStats {
                        p99: 0.0,
                        avg: 100.0,
                        min: 5.0,
                        med: 80.0,
                        max: 500.0,
                        p90: 200.0,
                        p95: 300.0,
                        count: 500,
                    },
                ),
                (
                    "http_req_duration{scenario:heavy}".to_string(),
                    TrendStats {
                        p99: 0.0,
                        avg: 900.0,
                        min: 100.0,
                        med: 800.0,
                        max: 5000.0,
                        p90: 2000.0,
                        p95: 3000.0,
                        count: 500,
                    },
                ),
            ],
        };

        let mut thresholds = HashMap::new();
        // Light scenario should pass p(95)<500
        thresholds.insert(
            "http_req_duration{scenario:light}".to_string(),
            vec![Threshold::from_expression("p(95)<500")],
        );
        // Heavy scenario should fail p(95)<500
        thresholds.insert(
            "http_req_duration{scenario:heavy}".to_string(),
            vec![Threshold::from_expression("p(95)<500")],
        );

        let results = evaluate(&thresholds, &snapshot);
        assert!(!results.all_passed()); // heavy fails

        let light = results
            .results
            .iter()
            .find(|r| r.metric.contains("light"))
            .unwrap();
        assert!(light.passed);
        assert!((light.actual_value - 300.0).abs() < 1.0);

        let heavy = results
            .results
            .iter()
            .find(|r| r.metric.contains("heavy"))
            .unwrap();
        assert!(!heavy.passed);
        assert!((heavy.actual_value - 3000.0).abs() < 1.0);
    }
}
