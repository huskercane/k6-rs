//! Prometheus remote write output plugin.
//!
//! Usage: `--out prometheus=http://localhost:9090/api/v1/write`
//! Converts k6 metrics to Prometheus time series and buffers for remote write.

use super::{snapshot_to_samples, MetricValue, Output};
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
            let mut labels: Vec<(String, String)> = sample
                .tags
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            // Sanitize metric name for Prometheus (replace dots/dashes with underscores)
            let prom_name = sample.metric.replace(['.', '-'], "_");

            match &sample.value {
                MetricValue::Counter { count, rate } => {
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}_total"),
                        labels: labels.clone(),
                        value: *count as f64,
                        timestamp_ms,
                    });
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}_rate"),
                        labels: labels.clone(),
                        value: *rate,
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
                        name: format!("k6_{prom_name}"),
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
                    // Emit as histogram-style summary
                    for (suffix, value) in [
                        ("avg", *avg),
                        ("min", *min),
                        ("med", *med),
                        ("max", *max),
                        ("p90", *p90),
                        ("p95", *p95),
                    ] {
                        let mut ts_labels = labels.clone();
                        ts_labels.push(("stat".to_string(), suffix.to_string()));
                        self.buffer.push(TimeSeries {
                            name: format!("k6_{prom_name}"),
                            labels: ts_labels,
                            value,
                            timestamp_ms,
                        });
                    }
                    self.buffer.push(TimeSeries {
                        name: format!("k6_{prom_name}_count"),
                        labels: labels.clone(),
                        value: *count as f64,
                        timestamp_ms,
                    });
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
        format!("prometheus ({})", self.url)
    }
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
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
            rates: vec![],
            trends: vec![],
        };

        output.add_snapshot(&snapshot, 5.0).unwrap();

        // Counter generates _total and _rate
        assert_eq!(output.buffer.len(), 2);
        assert_eq!(output.buffer[0].name, "k6_http_reqs_total");
        assert_eq!(output.buffer[1].name, "k6_http_reqs_rate");
    }

    #[test]
    fn prometheus_buffers_trend() {
        let mut output = PrometheusOutput::new("http://localhost:9090/api/v1/write");
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            counters: vec![],
            gauges: vec![],
            rates: vec![],
            trends: vec![(
                "http_req_duration".to_string(),
                TrendStats {
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

        // Trend: 6 stat samples + 1 count = 7
        assert_eq!(output.buffer.len(), 7);
    }
}
