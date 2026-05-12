use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use md5::{Digest, Md5};
use serde::Serialize;

use crate::metrics::{GroupSnapshot, MetricsSnapshot};
use crate::thresholds::ThresholdResults;

/// Summary data object passed to handleSummary().
/// Matches the k6 data structure that handleSummary receives.
#[derive(Debug, Serialize)]
pub struct SummaryData {
    pub metrics: HashMap<String, SummaryMetric>,
    pub root_group: SummaryGroup,
    pub state: SummaryState,
}

#[derive(Debug, Serialize)]
pub struct SummaryMetric {
    #[serde(rename = "type")]
    pub metric_type: String,
    pub contains: String,
    pub values: HashMap<String, f64>,
}

/// Group node in the `root_group` tree. CG-1 schema bump: mirrors upstream
/// k6's `lib.Group` JSON shape — `groups` and `checks` are always present
/// (empty maps if no children), `id` is md5(path). CG-1 routes per-check
/// records to the right group node by walking each check's `group_path`;
/// CG-2 will additionally surface group_duration stats on each node.
#[derive(Debug, Serialize)]
pub struct SummaryGroup {
    pub name: String,
    pub path: String,
    pub id: String,
    pub groups: BTreeMap<String, SummaryGroup>,
    pub checks: BTreeMap<String, SummaryCheck>,
}

/// Per-check entry inside a `SummaryGroup`. Matches upstream's `lib.Check`
/// JSON shape: `path` is `group_path + "::" + name`, `id` is md5(path),
/// and `passes`/`fails` are int counts (not booleans or rates — that's the
/// aggregate `checks` metric's job).
#[derive(Debug, Serialize)]
pub struct SummaryCheck {
    pub name: String,
    pub path: String,
    pub id: String,
    pub passes: u64,
    pub fails: u64,
}

#[derive(Debug, Serialize)]
pub struct SummaryState {
    pub is_std_out_tty: bool,
    pub test_run_duration_ms: f64,
}

fn md5_hex(s: &str) -> String {
    let digest = Md5::digest(s.as_bytes());
    let mut out = String::with_capacity(32);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

/// Convert one `GroupSnapshot` tree node into the corresponding
/// `SummaryGroup` JSON node. CG-2: the engine's snapshot already carries the
/// full tree, so this is a pure structural map — no pre-materialization or
/// path-walking is needed here anymore.
fn convert_group(snap: &GroupSnapshot) -> SummaryGroup {
    let mut groups = BTreeMap::new();
    for (key, child) in &snap.children {
        groups.insert(key.clone(), convert_group(child));
    }
    let mut checks = BTreeMap::new();
    for (name, (passes, fails)) in &snap.checks {
        let check_path = format!("{}::{}", snap.path, name);
        checks.insert(
            name.clone(),
            SummaryCheck {
                name: name.clone(),
                path: check_path.clone(),
                id: md5_hex(&check_path),
                passes: *passes,
                fails: *fails,
            },
        );
    }
    SummaryGroup {
        name: snap.name.clone(),
        path: snap.path.clone(),
        id: md5_hex(&snap.path),
        groups,
        checks,
    }
}

/// Build the data object that gets passed to handleSummary() and emitted via
/// --summary-export. CG-3: the snapshot now derives the full power set of
/// single-tag projections + untagged aggregates from each stored full-tag
/// entry, which is internally useful for threshold evaluation. For user-
/// facing output we filter to match upstream k6's behavior: emit every
/// untagged aggregate, but only tagged submetrics that are explicitly
/// targeted by a threshold. `thresholds = None` means no filtering
/// (used by tests that want to inspect every derived view).
pub fn build_summary_data(
    snapshot: &MetricsSnapshot,
    duration: Duration,
    thresholds: Option<&HashMap<String, Vec<crate::thresholds::Threshold>>>,
) -> SummaryData {
    // Pre-compute the set of tagged submetric names users have asked for.
    // Each threshold key parses through MetricSelector so tag order /
    // whitespace differences don't cause a filter miss.
    use crate::selector::MetricSelector;
    let filter_active = thresholds.is_some();
    let threshold_selectors: Vec<MetricSelector> = thresholds
        .map(|t| {
            t.keys()
                .filter_map(|k| MetricSelector::parse(k).ok())
                .filter(|s| !s.tags.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let allowed_tagged: std::collections::HashSet<String> = threshold_selectors
        .iter()
        .map(|s| s.canonical())
        .collect();
    let include_tagged = |name: &str| -> bool {
        if !name.contains('{') {
            return true; // untagged aggregates always shown
        }
        // `thresholds = None` is the explicit "no filtering" mode — emit
        // every derived view. Without this, tests/tooling that pass None
        // would see only untagged metrics, contradicting the docstring.
        if !filter_active {
            return true;
        }
        allowed_tagged.contains(name)
    };
    let mut metrics = HashMap::new();

    for (name, value, rate) in &snapshot.counters {
        if !include_tagged(name) {
            continue;
        }
        let mut values = HashMap::new();
        values.insert("count".to_string(), *value as f64);
        values.insert("rate".to_string(), *rate);
        metrics.insert(
            name.clone(),
            SummaryMetric {
                metric_type: "counter".to_string(),
                contains: "default".to_string(),
                values,
            },
        );
    }

    for (name, value, min, max) in &snapshot.gauges {
        // Gauges don't carry tags today but the filter is uniform across kinds.
        if !include_tagged(name) {
            continue;
        }
        let mut values = HashMap::new();
        values.insert("value".to_string(), *value);
        values.insert("min".to_string(), *min);
        values.insert("max".to_string(), *max);
        metrics.insert(
            name.clone(),
            SummaryMetric {
                metric_type: "gauge".to_string(),
                contains: "default".to_string(),
                values,
            },
        );
    }

    for (name, rate, passes, total) in &snapshot.rates {
        if !include_tagged(name) {
            continue;
        }
        let mut values = HashMap::new();
        values.insert("rate".to_string(), *rate);
        values.insert("passes".to_string(), *passes as f64);
        values.insert("fails".to_string(), (*total - *passes) as f64);
        metrics.insert(
            name.clone(),
            SummaryMetric {
                metric_type: "rate".to_string(),
                contains: "default".to_string(),
                values,
            },
        );
    }

    for (name, stats) in &snapshot.trends {
        if !include_tagged(name) {
            continue;
        }
        let mut values = HashMap::new();
        values.insert("avg".to_string(), stats.avg);
        values.insert("min".to_string(), stats.min);
        values.insert("med".to_string(), stats.med);
        values.insert("max".to_string(), stats.max);
        values.insert("p(90)".to_string(), stats.p90);
        values.insert("p(95)".to_string(), stats.p95);
        values.insert("p(99)".to_string(), stats.p99);
        values.insert("count".to_string(), stats.count as f64);

        let contains = if name.starts_with("http_req") || name.contains("duration") {
            "time"
        } else {
            "default"
        };

        metrics.insert(
            name.clone(),
            SummaryMetric {
                metric_type: "trend".to_string(),
                contains: contains.to_string(),
                values,
            },
        );
    }

    // CG-3: synthesize each threshold-targeted submetric via subset-merge
    // across stored entries whose tags are a SUPERSET of the threshold's.
    // We do NOT skip when a canonical-equal pass-through is already in
    // `metrics` — the synth result correctly *includes* that pass-through
    // (it's the trivial subset match) and additionally aggregates any
    // wider-cardinality stored series. Earlier versions skipped here, so
    // a threshold on `name{a:1}` saw only the exact `name{a:1}` series
    // and ignored superset `name{a:1,b:2}` samples; the overwrite below
    // closes that gap.
    for sel in &threshold_selectors {
        let canon = sel.canonical();
        if let Some(synth) = synthesize_subset_metric(sel, snapshot) {
            metrics.insert(canon, synth);
        }
    }

    // CG-2: the engine's MetricsSnapshot already holds the full group tree
    // as first-class data. The JSON view is a direct one-to-one conversion.
    let root_group = convert_group(&snapshot.group_tree);

    SummaryData {
        metrics,
        root_group,
        state: SummaryState {
            is_std_out_tty: std::io::IsTerminal::is_terminal(&std::io::stdout()),
            test_run_duration_ms: duration.as_secs_f64() * 1000.0,
        },
    }
}

/// Synthesize a single threshold-targeted submetric by subset-merging the
/// snapshot's pass-through storage entries. Tries trend → counter → rate
/// (a metric name is one kind only). Returns `None` if no stored entry
/// matches — the caller can decide what to do (today: omit).
fn synthesize_subset_metric(
    target: &crate::selector::MetricSelector,
    snapshot: &MetricsSnapshot,
) -> Option<SummaryMetric> {
    use crate::selector::MetricSelector;
    let matches = |stored: &str| -> bool {
        let Ok(s) = MetricSelector::parse(stored) else {
            return false;
        };
        s.name == target.name && target.tags.iter().all(|(k, v)| s.tags.get(k) == Some(v))
    };

    // Trends: merge by histogram add + count/sum/min/max.
    let mut trend_count: u64 = 0;
    let mut trend_sum: f64 = 0.0;
    let mut trend_min = f64::MAX;
    let mut trend_max = f64::MIN;
    let mut trend_hist: Option<hdrhistogram::Histogram<u64>> = None;
    let mut had_trend = false;
    for (stored, stats) in &snapshot.trends {
        if !matches(stored) {
            continue;
        }
        had_trend = true;
        trend_count += stats.count;
        trend_sum += stats.avg * stats.count as f64;
        if stats.min < trend_min {
            trend_min = stats.min;
        }
        if stats.max > trend_max {
            trend_max = stats.max;
        }
        if let Some(h) = snapshot.trend_histograms.get(stored) {
            match trend_hist.as_mut() {
                Some(m) => {
                    let _ = m.add(h);
                }
                None => trend_hist = Some(h.clone()),
            }
        }
    }
    if had_trend {
        let (med, p90, p95, p99) = match &trend_hist {
            Some(h) if h.len() > 0 => (
                h.value_at_quantile(0.5) as f64 / 1000.0,
                h.value_at_quantile(0.9) as f64 / 1000.0,
                h.value_at_quantile(0.95) as f64 / 1000.0,
                h.value_at_quantile(0.99) as f64 / 1000.0,
            ),
            _ => (0.0, 0.0, 0.0, 0.0),
        };
        let avg = if trend_count > 0 {
            trend_sum / trend_count as f64
        } else {
            0.0
        };
        let mut values = HashMap::new();
        values.insert("avg".to_string(), avg);
        values.insert(
            "min".to_string(),
            if trend_min == f64::MAX { 0.0 } else { trend_min },
        );
        values.insert("med".to_string(), med);
        values.insert(
            "max".to_string(),
            if trend_max == f64::MIN { 0.0 } else { trend_max },
        );
        values.insert("p(90)".to_string(), p90);
        values.insert("p(95)".to_string(), p95);
        values.insert("p(99)".to_string(), p99);
        values.insert("count".to_string(), trend_count as f64);
        let contains = if target.name.starts_with("http_req") || target.name.contains("duration") {
            "time"
        } else {
            "default"
        };
        return Some(SummaryMetric {
            metric_type: "trend".to_string(),
            contains: contains.to_string(),
            values,
        });
    }

    // Counters: sum values + rates.
    let mut had_counter = false;
    let mut counter_value: u64 = 0;
    let mut counter_rate: f64 = 0.0;
    for (stored, value, rate) in &snapshot.counters {
        if !matches(stored) {
            continue;
        }
        had_counter = true;
        counter_value += *value;
        counter_rate += *rate;
    }
    if had_counter {
        let mut values = HashMap::new();
        values.insert("count".to_string(), counter_value as f64);
        values.insert("rate".to_string(), counter_rate);
        return Some(SummaryMetric {
            metric_type: "counter".to_string(),
            contains: "default".to_string(),
            values,
        });
    }

    // Rates: sum passes + total, recompute rate.
    let mut had_rate = false;
    let mut r_passes: u64 = 0;
    let mut r_total: u64 = 0;
    for (stored, _rate, passes, total) in &snapshot.rates {
        if !matches(stored) {
            continue;
        }
        had_rate = true;
        r_passes += *passes;
        r_total += *total;
    }
    if had_rate {
        let mut values = HashMap::new();
        let rate = if r_total > 0 {
            r_passes as f64 / r_total as f64
        } else {
            0.0
        };
        values.insert("rate".to_string(), rate);
        values.insert("passes".to_string(), r_passes as f64);
        values.insert("fails".to_string(), (r_total - r_passes) as f64);
        return Some(SummaryMetric {
            metric_type: "rate".to_string(),
            contains: "default".to_string(),
            values,
        });
    }

    None
}

/// Format a k6-style end-of-test summary.
///
/// Matches k6 output format:
/// ```text
///      checks.........................: 98.00% ✓ 980  ✗ 20
///      data_received..................: 1.2 MB 120 kB/s
///      http_req_duration..............: avg=150ms min=10ms med=120ms max=2.5s p(90)=350ms p(95)=800ms
///      iterations.....................: 500    50/s
/// ```
pub fn format_summary(
    snapshot: &MetricsSnapshot,
    _duration: Duration,
    thresholds: Option<&ThresholdResults>,
) -> String {
    let mut out = String::new();
    let threshold_map = build_threshold_map(thresholds);

    // Checks (rate metrics named "checks")
    for (name, rate, passes, total) in &snapshot.rates {
        if name == "checks" {
            let pct = rate * 100.0;
            let fails = total - passes;
            let mark = threshold_mark(&threshold_map, name);
            out.push_str(&format!(
                "     {name}{dots} {pct:.2}% ✓ {passes:<6} ✗ {fails}{mark}\n",
                dots = dots(name, 33),
            ));
        }
    }
    if snapshot.rates.iter().any(|(n, _, _, _)| n == "checks") {
        out.push('\n');
    }

    // Data metrics
    for (name, value, rate) in &snapshot.counters {
        if name == "data_received" || name == "data_sent" {
            let mark = threshold_mark(&threshold_map, name);
            out.push_str(&format!(
                "     {name}{dots} {val} {rate_str}/s{mark}\n",
                dots = dots(name, 33),
                val = format_data(*value),
                rate_str = format_data(*rate as u64),
            ));
        }
    }

    // HTTP metrics (trends)
    let http_order = [
        "http_req_blocked",
        "http_req_connecting",
        "http_req_duration",
        "http_req_failed",
        "http_req_receiving",
        "http_req_sending",
        "http_req_tls_handshaking",
        "http_req_waiting",
    ];

    for metric_name in &http_order {
        // Check if it's a trend
        if let Some((_, stats)) = snapshot.trends.iter().find(|(n, _)| n == *metric_name) {
            let mark = threshold_mark(&threshold_map, metric_name);
            out.push_str(&format!(
                "     {metric_name}{dots} avg={avg} min={min} med={med} max={max} p(90)={p90} p(95)={p95}{mark}\n",
                dots = dots(metric_name, 33),
                avg = format_ms(stats.avg),
                min = format_ms(stats.min),
                med = format_ms(stats.med),
                max = format_ms(stats.max),
                p90 = format_ms(stats.p90),
                p95 = format_ms(stats.p95),
            ));
        }
        // Check if it's a rate (http_req_failed)
        if let Some((_, rate, passes, total)) =
            snapshot.rates.iter().find(|(n, _, _, _)| n == *metric_name)
        {
            let pct = rate * 100.0;
            let fails = total - passes;
            let mark = threshold_mark(&threshold_map, metric_name);
            out.push_str(&format!(
                "     {metric_name}{dots} {pct:.2}% ✓ {passes:<6} ✗ {fails}{mark}\n",
                dots = dots(metric_name, 33),
            ));
        }
    }

    // http_reqs counter
    if let Some((_, value, rate)) = snapshot.counters.iter().find(|(n, _, _)| n == "http_reqs") {
        let mark = threshold_mark(&threshold_map, "http_reqs");
        out.push_str(&format!(
            "     http_reqs{dots} {value:<7} {rate:.6}/s{mark}\n",
            dots = dots("http_reqs", 33),
        ));
    }

    // Iteration metrics
    if let Some((_, stats)) = snapshot
        .trends
        .iter()
        .find(|(n, _)| n == "iteration_duration")
    {
        let mark = threshold_mark(&threshold_map, "iteration_duration");
        out.push_str(&format!(
            "     iteration_duration{dots} avg={avg} min={min} med={med} max={max} p(90)={p90} p(95)={p95}{mark}\n",
            dots = dots("iteration_duration", 33),
            avg = format_ms(stats.avg),
            min = format_ms(stats.min),
            med = format_ms(stats.med),
            max = format_ms(stats.max),
            p90 = format_ms(stats.p90),
            p95 = format_ms(stats.p95),
        ));
    }

    if let Some((_, value, rate)) = snapshot.counters.iter().find(|(n, _, _)| n == "iterations") {
        let mark = threshold_mark(&threshold_map, "iterations");
        out.push_str(&format!(
            "     iterations{dots} {value:<7} {rate:.6}/s{mark}\n",
            dots = dots("iterations", 33),
        ));
    }

    if let Some((_, value, _)) = snapshot
        .counters
        .iter()
        .find(|(n, _, _)| n == "dropped_iterations")
    {
        if *value > 0 {
            out.push_str(&format!(
                "     dropped_iterations{dots} {value}\n",
                dots = dots("dropped_iterations", 33),
            ));
        }
    }

    // VU gauges
    for (name, value, min, max) in &snapshot.gauges {
        out.push_str(&format!(
            "     {name}{dots} {val} min={min_v} max={max_v}\n",
            dots = dots(name, 33),
            val = *value as u32,
            min_v = *min as u32,
            max_v = *max as u32,
        ));
    }

    // Custom metrics (anything not already printed)
    let printed: std::collections::HashSet<&str> = [
        "checks",
        "data_received",
        "data_sent",
        "http_req_blocked",
        "http_req_connecting",
        "http_req_duration",
        "http_req_failed",
        "http_req_receiving",
        "http_req_sending",
        "http_req_tls_handshaking",
        "http_req_waiting",
        "http_reqs",
        "iteration_duration",
        "iterations",
        "dropped_iterations",
        "vus",
        "vus_max",
    ]
    .into_iter()
    .collect();

    for (name, stats) in &snapshot.trends {
        if !printed.contains(name.as_str()) {
            let mark = threshold_mark(&threshold_map, name);
            out.push_str(&format!(
                "     {name}{dots} avg={avg} min={min} med={med} max={max} p(90)={p90} p(95)={p95}{mark}\n",
                dots = dots(name, 33),
                avg = format_ms(stats.avg),
                min = format_ms(stats.min),
                med = format_ms(stats.med),
                max = format_ms(stats.max),
                p90 = format_ms(stats.p90),
                p95 = format_ms(stats.p95),
            ));
        }
    }

    for (name, rate, passes, total) in &snapshot.rates {
        if !printed.contains(name.as_str()) {
            let pct = rate * 100.0;
            let fails = total - passes;
            let mark = threshold_mark(&threshold_map, name);
            out.push_str(&format!(
                "     {name}{dots} {pct:.2}% ✓ {passes:<6} ✗ {fails}{mark}\n",
                dots = dots(name, 33),
            ));
        }
    }

    for (name, value, rate) in &snapshot.counters {
        if !printed.contains(name.as_str()) {
            let mark = threshold_mark(&threshold_map, name);
            out.push_str(&format!(
                "     {name}{dots} {value:<7} {rate:.6}/s{mark}\n",
                dots = dots(name, 33),
            ));
        }
    }

    out
}

fn dots(name: &str, total_width: usize) -> String {
    let name_len = name.len();
    if name_len >= total_width {
        return ".: ".to_string();
    }
    let dot_count = total_width - name_len;
    format!("{:.<width$}: ", "", width = dot_count)
}

fn format_ms(ms: f64) -> String {
    if ms < 1.0 {
        format!("{:.0}µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{:.2}ms", ms)
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

fn format_data(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} kB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn build_threshold_map(thresholds: Option<&ThresholdResults>) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    if let Some(results) = thresholds {
        for r in &results.results {
            let entry = map.entry(r.metric.clone()).or_insert(true);
            if !r.passed {
                *entry = false;
            }
        }
    }
    map
}

fn threshold_mark(map: &HashMap<String, bool>, metric: &str) -> &'static str {
    match map.get(metric) {
        Some(true) => " ✓",
        Some(false) => " ✗",
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::TrendStats;

    #[test]
    fn format_ms_ranges() {
        assert_eq!(format_ms(0.5), "500µs");
        assert_eq!(format_ms(150.0), "150.00ms");
        assert_eq!(format_ms(2500.0), "2.50s");
    }

    #[test]
    fn format_data_ranges() {
        assert_eq!(format_data(512), "512 B");
        assert_eq!(format_data(1536), "1.5 kB");
        assert_eq!(format_data(1_572_864), "1.5 MB");
    }

    #[test]
    fn dots_padding() {
        let d = dots("vus", 33);
        assert!(d.contains(".."));
        assert!(d.ends_with(": "));
    }

    #[test]
    fn summary_contains_key_metrics() {
        let snapshot = MetricsSnapshot { trend_histograms: std::collections::HashMap::new(), group_tree: GroupSnapshot::default(),
            counters: vec![
                ("http_reqs".to_string(), 100, 10.0),
                ("iterations".to_string(), 50, 5.0),
                ("data_received".to_string(), 1_048_576, 104857.0),
            ],
            gauges: vec![("vus".to_string(), 10.0, 1.0, 10.0)],
            rates: vec![
                ("checks".to_string(), 0.95, 95, 100),
                ("http_req_failed".to_string(), 0.02, 2, 100),
            ],
            trends: vec![(
                "http_req_duration".to_string(),
                TrendStats { p99: 0.0,
                    avg: 150.0,
                    min: 10.0,
                    med: 120.0,
                    max: 2500.0,
                    p90: 350.0,
                    p95: 800.0,
                    count: 100,
                },
            )],
        };

        let output = format_summary(&snapshot, Duration::from_secs(10), None);

        assert!(output.contains("checks"), "missing checks line");
        assert!(output.contains("95.00%"), "missing check rate");
        assert!(output.contains("http_req_duration"), "missing duration");
        assert!(output.contains("avg=150.00ms"), "missing avg");
        assert!(output.contains("p(95)=800.00ms"), "missing p95");
        assert!(output.contains("http_reqs"), "missing http_reqs");
        assert!(output.contains("data_received"), "missing data_received");
    }

    #[test]
    fn summary_with_thresholds() {
        let snapshot = MetricsSnapshot { trend_histograms: std::collections::HashMap::new(), group_tree: GroupSnapshot::default(),
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
            rates: vec![("http_req_failed".to_string(), 0.02, 2, 100)],
            trends: vec![(
                "http_req_duration".to_string(),
                TrendStats { p99: 0.0,
                    avg: 150.0,
                    min: 10.0,
                    med: 120.0,
                    max: 2500.0,
                    p90: 350.0,
                    p95: 800.0,
                    count: 100,
                },
            )],
        };

        let thresholds = ThresholdResults {
            results: vec![
                crate::thresholds::ThresholdResult {
                    metric: "http_req_duration".to_string(),
                    expression: "p(95)<2000".to_string(),
                    passed: true,
                    actual_value: 800.0,
                },
                crate::thresholds::ThresholdResult {
                    metric: "http_req_failed".to_string(),
                    expression: "rate<0.01".to_string(),
                    passed: false,
                    actual_value: 0.02,
                },
            ],
        };

        let output = format_summary(&snapshot, Duration::from_secs(10), Some(&thresholds));

        assert!(output.contains("http_req_duration") && output.contains("✓"));
        assert!(output.contains("http_req_failed") && output.contains("✗"));
    }

    #[test]
    fn threshold_targeted_submetric_is_synthesized_from_subset_merge() {
        // CG-3 bug fix on real data. The snapshot only holds a stored
        // full-tag entry `http_req_duration{method:GET,name:get,status:200}`.
        // A threshold targets the 2-tag subset `{name:get,status:200}`
        // — that exact form is NOT in the snapshot, but build_summary_data
        // must synthesize it via subset-merge so users see the metric
        // referenced by their threshold.
        use crate::metrics::MetricsRegistry;
        use std::collections::HashMap;
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("method".to_string(), "GET".to_string());
        tags.insert("name".to_string(), "get".to_string());
        tags.insert("status".to_string(), "200".to_string());
        for ms in 1..=100 {
            reg.trend_add_with_tags("http_req_duration", ms as f64, &tags);
            reg.counter_add_with_tags("http_reqs", 1, &tags);
        }
        let snap = reg.snapshot(1.0);

        let mut thresholds: HashMap<String, Vec<crate::thresholds::Threshold>> = HashMap::new();
        thresholds.insert(
            "http_req_duration{name:get,status:200}".to_string(),
            vec![crate::thresholds::Threshold::from_expression("p(95)<5000")],
        );
        thresholds.insert(
            "http_reqs{name:get,status:200}".to_string(),
            vec![crate::thresholds::Threshold::from_expression("count==100")],
        );

        let summary = build_summary_data(&snap, Duration::from_secs(1), Some(&thresholds));

        // Subset-merge synthesized the 2-tag submetric from the 3-tag
        // pass-through.
        let dur = summary
            .metrics
            .get("http_req_duration{name:get,status:200}")
            .expect("threshold-target submetric synthesized");
        assert_eq!(dur.metric_type, "trend");
        assert!((dur.values["count"] - 100.0).abs() < 0.01);
        assert!(dur.values["p(95)"] > 0.0, "p(95) computed from merged histogram");

        let req = summary
            .metrics
            .get("http_reqs{name:get,status:200}")
            .expect("counter threshold-target submetric synthesized");
        assert_eq!(req.metric_type, "counter");
        assert!((req.values["count"] - 100.0).abs() < 0.01);
    }

    #[test]
    fn threshold_filter_drops_non_targeted_tagged_metrics() {
        // CG-3: an http run produces single-tag and multi-tag stored
        // entries via tagged writes. summary-export must NOT emit
        // tagged submetrics that aren't threshold targets (matches
        // upstream behavior).
        use crate::metrics::MetricsRegistry;
        use std::collections::HashMap;
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("status".to_string(), "200".to_string());
        reg.counter_add_with_tags("http_reqs", 5, &tags);

        // No thresholds — only untagged aggregate is emitted.
        let thresholds: HashMap<String, Vec<crate::thresholds::Threshold>> = HashMap::new();
        let summary = build_summary_data(&reg.snapshot(1.0), Duration::from_secs(1), Some(&thresholds));
        assert!(summary.metrics.contains_key("http_reqs"));
        assert!(
            !summary.metrics.contains_key("http_reqs{status:200}"),
            "tagged submetric NOT in summary without a threshold targeting it"
        );

        // With a threshold targeting the tagged form, it shows up.
        let mut thresholds: HashMap<String, Vec<crate::thresholds::Threshold>> = HashMap::new();
        thresholds.insert("http_reqs{status:200}".to_string(), vec![crate::thresholds::Threshold::from_expression("count>0")]);
        let summary = build_summary_data(&reg.snapshot(1.0), Duration::from_secs(1), Some(&thresholds));
        assert!(summary.metrics.contains_key("http_reqs{status:200}"));
    }

    #[test]
    fn summary_synthesis_merges_exact_plus_superset_for_threshold_target() {
        // CG-3 follow-up bug: storage has BOTH `http_reqs{a:1}` AND
        // `http_reqs{a:1,b:2}`. Threshold targets `http_reqs{a:1}`. The
        // synthesized summary entry must aggregate across both; earlier
        // code skipped synthesis when a canonical-equal pass-through
        // already existed, so the threshold target showed only the
        // exact-match count, ignoring superset samples.
        use crate::metrics::MetricsRegistry;
        use std::collections::HashMap;
        let reg = MetricsRegistry::new();
        let mut one_tag = std::collections::BTreeMap::new();
        one_tag.insert("a".to_string(), "1".to_string());
        for _ in 0..5 {
            reg.counter_add_with_tags("http_reqs", 1, &one_tag);
        }
        let mut two_tag = std::collections::BTreeMap::new();
        two_tag.insert("a".to_string(), "1".to_string());
        two_tag.insert("b".to_string(), "2".to_string());
        for _ in 0..3 {
            reg.counter_add_with_tags("http_reqs", 1, &two_tag);
        }
        let mut thresholds: HashMap<String, Vec<crate::thresholds::Threshold>> = HashMap::new();
        thresholds.insert(
            "http_reqs{a:1}".to_string(),
            vec![crate::thresholds::Threshold::from_expression("count==8")],
        );

        let summary = build_summary_data(
            &reg.snapshot(1.0),
            Duration::from_secs(1),
            Some(&thresholds),
        );
        let entry = summary
            .metrics
            .get("http_reqs{a:1}")
            .expect("threshold target synthesized");
        assert!(
            (entry.values["count"] - 8.0).abs() < 0.01,
            "synth must aggregate exact + superset (5 + 3 = 8), got {}",
            entry.values["count"]
        );
    }

    #[test]
    fn summary_thresholds_none_means_no_filtering() {
        // CG-3 follow-up: build_summary_data's docstring promises
        // `thresholds = None` = no filtering (for tests/tooling that want
        // every derived view). Earlier code built an empty allowed_tagged
        // set and silently filtered out every tagged metric — the
        // implementation contradicted the doc. With `None`, every
        // pass-through tagged entry should appear in the summary.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("status".to_string(), "200".to_string());
        reg.counter_add_with_tags("http_reqs", 5, &tags);

        let summary = build_summary_data(&reg.snapshot(1.0), Duration::from_secs(1), None);
        assert!(
            summary.metrics.contains_key("http_reqs{status:200}"),
            "thresholds=None must NOT filter tagged metrics; summary had keys: {:?}",
            summary.metrics.keys().collect::<Vec<_>>()
        );
        // Untagged aggregate also still present.
        assert!(summary.metrics.contains_key("http_reqs"));
    }

    #[test]
    fn build_summary_data_includes_p99_for_trends() {
        // P0.3 regression: --summary-export JSON must carry p(99) so the
        // conformance harness can diff that percentile against upstream.
        // Before this change, only p(90) and p(95) were exported and any
        // tool reading the JSON had no view into the right-tail latency.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        for i in 1..=1000 {
            reg.trend_add("http_req_duration", i as f64);
        }
        let snapshot = reg.snapshot(1.0);
        let summary = build_summary_data(&snapshot, Duration::from_secs(1), None);
        let trend = summary
            .metrics
            .get("http_req_duration")
            .expect("trend metric present in summary");
        assert!(
            trend.values.contains_key("p(99)"),
            "p(99) key must be exported"
        );
        let p99 = trend.values["p(99)"];
        assert!(
            (p99 - 990.0).abs() < 5.0,
            "p(99) ~= 990 for 1..1000, got {p99}"
        );
        // Companion: p(95) still present (don't accidentally drop existing keys).
        assert!(trend.values.contains_key("p(95)"));
        assert!(trend.values.contains_key("count"));
    }

    #[test]
    fn root_group_carries_per_check_counts_under_correct_paths() {
        // CG-1 regression: per-check pass/fail counts must land in the
        // root_group tree at the path indicated by their group_path.
        //   - "" (root) → root_group.checks[NAME]
        //   - "::api" → root_group.groups["api"].checks[NAME]
        //   - "::api::v2" → root_group.groups["api"].groups["v2"].checks[NAME]
        // Each new node has its `id` = md5(path) so the conformance harness
        // can diff identity without re-deriving paths from names.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        reg.check_add("", "is healthy", true);
        reg.check_add("", "is healthy", false);
        reg.check_add("::api", "v1 ok", true);
        reg.check_add("::api::v2", "post ok", true);
        reg.check_add("::api::v2", "post ok", true);
        reg.check_add("::api::v2", "post ok", false);

        let snap = reg.snapshot(1.0);
        let summary = build_summary_data(&snap, Duration::from_secs(1), None);

        // Root-level check.
        let root_chk = summary
            .root_group
            .checks
            .get("is healthy")
            .expect("root-level check present");
        assert_eq!(root_chk.passes, 1);
        assert_eq!(root_chk.fails, 1);
        assert_eq!(root_chk.path, "::is healthy");
        assert_eq!(root_chk.id, md5_hex("::is healthy"));

        // Nested under ::api.
        let api = summary
            .root_group
            .groups
            .get("api")
            .expect("api subgroup present");
        assert_eq!(api.path, "::api");
        assert_eq!(api.id, md5_hex("::api"));
        let v1 = api.checks.get("v1 ok").expect("v1 ok check present");
        assert_eq!(v1.passes, 1);
        assert_eq!(v1.fails, 0);

        // Nested under ::api::v2.
        let v2 = api.groups.get("v2").expect("v2 subgroup present");
        assert_eq!(v2.path, "::api::v2");
        let post = v2.checks.get("post ok").expect("post ok check present");
        assert_eq!(post.passes, 2);
        assert_eq!(post.fails, 1);
        assert_eq!(post.path, "::api::v2::post ok");

        // Schema invariant: every group node always carries groups{}
        // and checks{} maps. A consumer that grew up expecting them not to
        // exist would misread the JSON — we're declaring them always present.
        assert!(summary.root_group.groups.contains_key("api"));
        let leaf = api.groups.get("v2").unwrap();
        // v2 has no further subgroups; the map exists and is empty.
        assert!(leaf.groups.is_empty());
    }

    #[test]
    fn root_group_materializes_group_only_paths() {
        // CG-1 follow-up regression: `group('x', fn)` with no check inside
        // must still appear in `root_group.groups["x"]` (empty checks{} +
        // groups{} maps). Without this, the upstream-shape schema would be
        // structurally wrong for group-only scripts.
        use crate::metrics::MetricsRegistry;
        let reg = MetricsRegistry::new();
        // Two paths registered but no checks recorded.
        reg.group_register("::outer");
        reg.group_register("::outer::inner");
        // Plus one path that DOES have a check inside, to confirm both
        // sources of group materialization compose correctly.
        reg.group_register("::api");
        reg.check_add("::api", "ok", true);

        let snap = reg.snapshot(1.0);
        let summary = build_summary_data(&snap, Duration::from_secs(1), None);

        // Empty group nodes present.
        let outer = summary
            .root_group
            .groups
            .get("outer")
            .expect("outer group materialized from group_register, even with no checks");
        assert_eq!(outer.path, "::outer");
        assert_eq!(outer.id, md5_hex("::outer"));
        assert!(outer.checks.is_empty(), "outer has no checks recorded");

        let inner = outer
            .groups
            .get("inner")
            .expect("nested inner group also materialized");
        assert_eq!(inner.path, "::outer::inner");
        assert!(inner.checks.is_empty());
        assert!(inner.groups.is_empty());

        // Group with a check inside still works.
        let api = summary.root_group.groups.get("api").expect("api group present");
        assert!(api.checks.contains_key("ok"));
    }

    #[test]
    fn summary_shows_custom_metrics() {
        let snapshot = MetricsSnapshot { trend_histograms: std::collections::HashMap::new(), group_tree: GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![("my_custom_rate".to_string(), 0.75, 75, 100)],
            trends: vec![(
                "my_custom_trend".to_string(),
                TrendStats { p99: 0.0,
                    avg: 42.0,
                    min: 1.0,
                    med: 40.0,
                    max: 100.0,
                    p90: 80.0,
                    p95: 90.0,
                    count: 100,
                },
            )],
        };

        let output = format_summary(&snapshot, Duration::from_secs(10), None);
        assert!(output.contains("my_custom_trend"), "missing custom trend");
        assert!(output.contains("my_custom_rate"), "missing custom rate");
    }
}
