use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;

use crate::metrics::MetricsSnapshot;
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

#[derive(Debug, Serialize)]
pub struct SummaryGroup {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct SummaryState {
    pub is_std_out_tty: bool,
    pub test_run_duration_ms: f64,
}

/// Build the data object that gets passed to handleSummary().
pub fn build_summary_data(snapshot: &MetricsSnapshot, duration: Duration) -> SummaryData {
    let mut metrics = HashMap::new();

    for (name, value, rate) in &snapshot.counters {
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
        let mut values = HashMap::new();
        values.insert("avg".to_string(), stats.avg);
        values.insert("min".to_string(), stats.min);
        values.insert("med".to_string(), stats.med);
        values.insert("max".to_string(), stats.max);
        values.insert("p(90)".to_string(), stats.p90);
        values.insert("p(95)".to_string(), stats.p95);
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

    SummaryData {
        metrics,
        root_group: SummaryGroup {
            name: "".to_string(),
            path: "".to_string(),
        },
        state: SummaryState {
            is_std_out_tty: std::io::IsTerminal::is_terminal(&std::io::stdout()),
            test_run_duration_ms: duration.as_secs_f64() * 1000.0,
        },
    }
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
        let snapshot = MetricsSnapshot {
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
                TrendStats {
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
        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
            rates: vec![("http_req_failed".to_string(), 0.02, 2, 100)],
            trends: vec![(
                "http_req_duration".to_string(),
                TrendStats {
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
    fn summary_shows_custom_metrics() {
        let snapshot = MetricsSnapshot {
            counters: vec![],
            gauges: vec![],
            rates: vec![("my_custom_rate".to_string(), 0.75, 75, 100)],
            trends: vec![(
                "my_custom_trend".to_string(),
                TrendStats {
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
