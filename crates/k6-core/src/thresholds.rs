use std::collections::HashMap;

use crate::metrics::MetricsSnapshot;

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
/// `thresholds` maps metric name → list of expression strings (e.g., "p(95)<2000").
pub fn evaluate(
    thresholds: &HashMap<String, Vec<String>>,
    snapshot: &MetricsSnapshot,
) -> ThresholdResults {
    let mut results = Vec::new();

    for (metric, conditions) in thresholds {
        for expr in conditions {
            let result = evaluate_one(metric, expr, snapshot);
            results.push(result);
        }
    }

    results.sort_by(|a, b| a.metric.cmp(&b.metric));

    ThresholdResults { results }
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

fn resolve_stat(metric: &str, stat: &ThresholdStat, snapshot: &MetricsSnapshot) -> f64 {
    // Check if it's a trend metric
    if let Some((_, stats)) = snapshot.trends.iter().find(|(n, _)| n == metric) {
        return match stat {
            ThresholdStat::Avg => stats.avg,
            ThresholdStat::Min => stats.min,
            ThresholdStat::Max => stats.max,
            ThresholdStat::Med => stats.med,
            ThresholdStat::Percentile(p) => {
                // We only have p90, p95 precomputed — map to nearest
                if (*p - 90.0).abs() < 1.0 {
                    stats.p90
                } else if (*p - 95.0).abs() < 1.0 {
                    stats.p95
                } else {
                    // For other percentiles, use p95 as approximation
                    // (full support would require access to the histogram)
                    stats.p95
                }
            }
            ThresholdStat::Count => stats.count as f64,
            ThresholdStat::Value => stats.avg, // fallback
            _ => stats.avg,
        };
    }

    // Check counter metrics
    if let Some((_, value, rate)) = snapshot.counters.iter().find(|(n, _, _)| n == metric) {
        return match stat {
            ThresholdStat::Count | ThresholdStat::Value => *value as f64,
            ThresholdStat::Rate => *rate,
            _ => *value as f64,
        };
    }

    // Check rate metrics (like http_req_failed, checks)
    if let Some((_, rate, _passes, total)) = snapshot.rates.iter().find(|(n, _, _, _)| n == metric) {
        return match stat {
            ThresholdStat::Rate | ThresholdStat::Value => *rate,
            ThresholdStat::Count => *total as f64,
            _ => *rate,
        };
    }

    // Check gauge metrics
    if let Some((_, value, min, max)) = snapshot.gauges.iter().find(|(n, _, _, _)| n == metric) {
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
    fn threshold_p95_passes() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec!["p(95)<2000".to_string()],
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
            vec!["p(95)<500".to_string()],
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
            vec!["rate<0.01".to_string()],
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
            vec!["rate<0.001".to_string()],
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());
    }

    #[test]
    fn threshold_checks_rate() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert("checks".to_string(), vec!["rate>0.95".to_string()]);

        let results = evaluate(&thresholds, &snap);
        assert!(results.all_passed());
    }

    #[test]
    fn threshold_counter_count() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_reqs".to_string(),
            vec!["count>500".to_string()],
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
            vec!["avg<200".to_string(), "max<5000".to_string()],
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
            vec!["p(95)<2000".to_string(), "avg<100".to_string()], // avg=150 > 100
        );

        let results = evaluate(&thresholds, &snap);
        assert!(!results.all_passed());

        let passed_count = results.results.iter().filter(|r| r.passed).count();
        assert_eq!(passed_count, 1);
    }

    #[test]
    fn invalid_expression_fails() {
        let snap = sample_snapshot();
        let mut thresholds = HashMap::new();
        thresholds.insert(
            "http_req_duration".to_string(),
            vec!["invalid_expr".to_string()],
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
            vec!["avg<100".to_string()],
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
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![
                (
                    "http_req_duration".to_string(),
                    TrendStats {
                        avg: 500.0, min: 10.0, med: 400.0, max: 5000.0,
                        p90: 1000.0, p95: 2000.0, count: 1000,
                    },
                ),
                (
                    "http_req_duration{scenario:light}".to_string(),
                    TrendStats {
                        avg: 100.0, min: 5.0, med: 80.0, max: 500.0,
                        p90: 200.0, p95: 300.0, count: 500,
                    },
                ),
                (
                    "http_req_duration{scenario:heavy}".to_string(),
                    TrendStats {
                        avg: 900.0, min: 100.0, med: 800.0, max: 5000.0,
                        p90: 2000.0, p95: 3000.0, count: 500,
                    },
                ),
            ],
        };

        let mut thresholds = HashMap::new();
        // Light scenario should pass p(95)<500
        thresholds.insert(
            "http_req_duration{scenario:light}".to_string(),
            vec!["p(95)<500".to_string()],
        );
        // Heavy scenario should fail p(95)<500
        thresholds.insert(
            "http_req_duration{scenario:heavy}".to_string(),
            vec!["p(95)<500".to_string()],
        );

        let results = evaluate(&thresholds, &snapshot);
        assert!(!results.all_passed()); // heavy fails

        let light = results.results.iter().find(|r| r.metric.contains("light")).unwrap();
        assert!(light.passed);
        assert!((light.actual_value - 300.0).abs() < 1.0);

        let heavy = results.results.iter().find(|r| r.metric.contains("heavy")).unwrap();
        assert!(!heavy.passed);
        assert!((heavy.actual_value - 3000.0).abs() < 1.0);
    }
}
