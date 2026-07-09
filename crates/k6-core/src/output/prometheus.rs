//! Prometheus remote write output plugin.
//!
//! Usage: `--out prometheus=http://localhost:9090/api/v1/write`
//! Converts k6 metrics to Prometheus time series and buffers for remote writing.

use super::{MetricValue, Output, snapshot_to_samples};
use crate::metrics::MetricsSnapshot;

pub struct PrometheusOutput {
    url: String,
    buffer: Vec<TimeSeries>,
}

/// A Prometheus time series sample.
#[derive(Debug, Clone)]
pub struct TimeSeries {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
    pub timestamp_ms: i64,
}

impl PrometheusOutput {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            buffer: Vec::new(),
        }
    }
}

impl Output for PrometheusOutput {
    fn start(&mut self) -> anyhow::Result<()> {
        self.buffer.clear();
        Ok(())
    }

    fn add_snapshot(
        &mut self,
        snapshot: &MetricsSnapshot,
        elapsed_secs: f64,
    ) -> anyhow::Result<()> {
        let samples = snapshot_to_samples(snapshot, elapsed_secs);
        let timestamp_ms = (elapsed_secs * 1000.0) as i64;

        for sample in &samples {
            let labels = prometheus_labels(&sample.tags);

            // Sanitize metric name for Prometheus (replace dots/dashes with underscores)
            let prom_name = sample.metric.replace(['.', '-'], "_");

            match &sample.value {
                MetricValue::Counter { count, .. } => {
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}_total"),
                        labels: labels.clone(),
                        value: *count as f64,
                        timestamp_ms,
                    });
                }
                MetricValue::Gauge { value, .. } => {
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}"),
                        labels: labels.clone(),
                        value: *value,
                        timestamp_ms,
                    });
                }
                MetricValue::Rate { rate, .. } => {
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}_rate"),
                        labels: labels.clone(),
                        value: *rate,
                        timestamp_ms,
                    });
                }
                MetricValue::Trend {
                    avg,
                    min,
                    med,
                    max,
                    p90,
                    p95,
                    count,
                } => {
                    for (suffix, value) in [
                        ("avg", *avg),
                        ("min", *min),
                        ("med", *med),
                        ("max", *max),
                        ("p90", *p90),
                        ("p95", *p95),
                        ("count", *count as f64),
                    ] {
                        self.buffer.push(TimeSeries {
                            name: format!("k6_{prom_name}_{suffix}"),
                            labels: labels.clone(),
                            value,
                            timestamp_ms,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        // In a real implementation, we'd serialize to Prometheus remote write protobuf
        // and POST to the configured URL with Snappy compression.
        // For now, metrics are buffered for testing.
        self.buffer.clear();
        Ok(())
    }

    fn description(&self) -> String {
        format!("Prometheus ({})", self.url)
    }
}

fn prometheus_labels(tags: &std::collections::HashMap<String, String>) -> Vec<(String, String)> {
    let mut labels: Vec<(String, String)> = tags
        .iter()
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.sort_by(|a, b| a.0.cmp(&b.0));
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSnapshot, TrendStats};

    #[test]
    fn prometheus_buffers_counter() {
        let mut output = PrometheusOutput::new("http://localhost:9090/api/v1/write");
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
            rates: vec![],
            trends: vec![],
        };

        output.add_snapshot(&snapshot, 5.0).unwrap();

        // Upstream remote write maps counters to cumulative _total series.
        assert_eq!(output.buffer.len(), 1);
        assert_eq!(output.buffer[0].name, "k6_http_reqs_total");
    }

    #[test]
    fn prometheus_buffers_trend() {
        let mut output = PrometheusOutput::new("http://localhost:9090/api/v1/write");
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![(
                "http_req_duration".to_string(),
                TrendStats {
                    p99: 0.0,
                    avg: 100.0,
                    min: 10.0,
                    med: 90.0,
                    max: 500.0,
                    p90: 200.0,
                    p95: 300.0,
                    count: 100,
                },
            )],
        };

        output.add_snapshot(&snapshot, 5.0).unwrap();

        assert_eq!(
            output
                .buffer
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "k6_http_req_duration_avg",
                "k6_http_req_duration_min",
                "k6_http_req_duration_med",
                "k6_http_req_duration_max",
                "k6_http_req_duration_p90",
                "k6_http_req_duration_p95",
                "k6_http_req_duration_count",
            ]
        );
        assert_eq!(output.buffer.len(), 7);
    }

    #[test]
    fn prometheus_rate_uses_rate_suffix_and_sorted_nonempty_labels() {
        let mut output = PrometheusOutput::new("http://localhost:9090/api/v1/write");
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            trend_histograms: std::collections::HashMap::new(),
            group_tree: crate::metrics::GroupSnapshot::default(),
            counters: vec![],
            gauges: vec![],
            rates: vec![(
                "checks{tagk1:tagv1,b1:v1,tagEmptyValue:}".to_string(),
                0.75,
                3,
                4,
            )],
            trends: vec![],
        };

        output.add_snapshot(&snapshot, 5.0).unwrap();

        assert_eq!(output.buffer.len(), 1);
        assert_eq!(output.buffer[0].name, "k6_checks_rate");
        assert_eq!(
            output.buffer[0].labels,
            vec![
                ("b1".to_string(), "v1".to_string()),
                ("tagk1".to_string(), "tagv1".to_string()),
            ]
        );
    }
}
