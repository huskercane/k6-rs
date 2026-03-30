//! Output plugins for streaming metrics to external systems.
//!
//! Each plugin implements the `Output` trait. Plugins are instantiated via `--out` CLI flag:
//! - `--out json=results.json`
//! - `--out csv=results.csv`
//! - `--out influxdb=http://localhost:8086/k6`
//! - `--out prometheus=http://localhost:9090/api/v1/write`
//! - `--out duckdb=results.duckdb`

pub mod csv;
pub mod duckdb;
pub mod influxdb;
pub mod json;
pub mod prometheus;

use crate::metrics::MetricsSnapshot;

/// A metric sample emitted during test execution.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricSample {
    /// Metric name (e.g., "http_req_duration")
    pub metric: String,
    /// Metric type: "counter", "gauge", "rate", "trend"
    pub metric_type: MetricType,
    /// Timestamp (milliseconds since test start)
    pub timestamp: f64,
    /// The metric value(s)
    pub value: MetricValue,
    /// Tags attached to this sample
    pub tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricType {
    Counter,
    Gauge,
    Rate,
    Trend,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum MetricValue {
    Counter { count: u64, rate: f64 },
    Gauge { value: f64, min: f64, max: f64 },
    Rate { rate: f64, passes: u64, total: u64 },
    Trend { avg: f64, min: f64, med: f64, max: f64, p90: f64, p95: f64, count: u64 },
}

/// Trait for output plugins that receive periodic metric snapshots.
pub trait Output: Send {
    /// Called once before the test starts. Initialize connections, open files, etc.
    fn start(&mut self) -> anyhow::Result<()>;

    /// Called periodically (every ~1s) with a snapshot of all metrics.
    fn add_snapshot(&mut self, snapshot: &MetricsSnapshot, elapsed_secs: f64) -> anyhow::Result<()>;

    /// Called once after the test ends. Flush buffers, close connections.
    fn stop(&mut self) -> anyhow::Result<()>;

    /// Human-readable description for banner output (e.g., "json (results.json)")
    fn description(&self) -> String;
}

/// Parse `--out` flag value into (plugin_name, arg).
/// Format: `name=arg` or just `name`.
pub fn parse_out_flag(value: &str) -> (&str, Option<&str>) {
    if let Some(idx) = value.find('=') {
        (&value[..idx], Some(&value[idx + 1..]))
    } else {
        (value, None)
    }
}

/// Create an output plugin from a parsed `--out` flag.
pub fn create_output(name: &str, arg: Option<&str>) -> anyhow::Result<Box<dyn Output>> {
    match name {
        "json" => {
            let path = arg.unwrap_or("results.json");
            Ok(Box::new(json::JsonOutput::new(path)))
        }
        "csv" => {
            let path = arg.unwrap_or("results.csv");
            Ok(Box::new(csv::CsvOutput::new(path)))
        }
        "influxdb" => {
            let url = arg.ok_or_else(|| {
                anyhow::anyhow!("influxdb output requires a URL: --out influxdb=http://host:8086/dbname")
            })?;
            Ok(Box::new(influxdb::InfluxDbOutput::new(url)?))
        }
        "prometheus" => {
            let url = arg.ok_or_else(|| {
                anyhow::anyhow!(
                    "prometheus output requires a URL: --out prometheus=http://host:9090/api/v1/write"
                )
            })?;
            Ok(Box::new(prometheus::PrometheusOutput::new(url)))
        }
        "duckdb" => {
            let path = arg.unwrap_or("results.duckdb");
            Ok(Box::new(duckdb::DuckDbOutput::new(path)))
        }
        _ => anyhow::bail!("unknown output plugin: {name}. Available: json, csv, influxdb, prometheus, duckdb"),
    }
}

/// Convert a MetricsSnapshot into a flat list of MetricSamples.
pub fn snapshot_to_samples(snapshot: &MetricsSnapshot, elapsed_secs: f64) -> Vec<MetricSample> {
    let mut samples = Vec::new();
    let timestamp = elapsed_secs * 1000.0;

    for (name, count, rate) in &snapshot.counters {
        let (metric, tags) = parse_metric_name(name);
        samples.push(MetricSample {
            metric,
            metric_type: MetricType::Counter,
            timestamp,
            value: MetricValue::Counter { count: *count, rate: *rate },
            tags,
        });
    }

    for (name, value, min, max) in &snapshot.gauges {
        let (metric, tags) = parse_metric_name(name);
        samples.push(MetricSample {
            metric,
            metric_type: MetricType::Gauge,
            timestamp,
            value: MetricValue::Gauge { value: *value, min: *min, max: *max },
            tags,
        });
    }

    for (name, rate, passes, total) in &snapshot.rates {
        let (metric, tags) = parse_metric_name(name);
        samples.push(MetricSample {
            metric,
            metric_type: MetricType::Rate,
            timestamp,
            value: MetricValue::Rate { rate: *rate, passes: *passes, total: *total },
            tags,
        });
    }

    for (name, stats) in &snapshot.trends {
        let (metric, tags) = parse_metric_name(name);
        samples.push(MetricSample {
            metric,
            metric_type: MetricType::Trend,
            timestamp,
            value: MetricValue::Trend {
                avg: stats.avg,
                min: stats.min,
                med: stats.med,
                max: stats.max,
                p90: stats.p90,
                p95: stats.p95,
                count: stats.count,
            },
            tags,
        });
    }

    samples
}

/// Parse tagged metric names like `http_req_duration{scenario:light}` into (name, tags).
fn parse_metric_name(name: &str) -> (String, std::collections::HashMap<String, String>) {
    let mut tags = std::collections::HashMap::new();

    if let Some(brace_start) = name.find('{') {
        let metric = name[..brace_start].to_string();
        let tag_str = &name[brace_start + 1..name.len() - 1];
        for pair in tag_str.split(',') {
            if let Some(colon) = pair.find(':') {
                tags.insert(pair[..colon].to_string(), pair[colon + 1..].to_string());
            }
        }
        (metric, tags)
    } else {
        (name.to_string(), tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_out_flag_with_arg() {
        let (name, arg) = parse_out_flag("json=results.json");
        assert_eq!(name, "json");
        assert_eq!(arg, Some("results.json"));
    }

    #[test]
    fn parse_out_flag_without_arg() {
        let (name, arg) = parse_out_flag("json");
        assert_eq!(name, "json");
        assert_eq!(arg, None);
    }

    #[test]
    fn parse_metric_name_no_tags() {
        let (name, tags) = parse_metric_name("http_reqs");
        assert_eq!(name, "http_reqs");
        assert!(tags.is_empty());
    }

    #[test]
    fn parse_metric_name_with_tags() {
        let (name, tags) = parse_metric_name("http_req_duration{scenario:light}");
        assert_eq!(name, "http_req_duration");
        assert_eq!(tags.get("scenario").unwrap(), "light");
    }

    #[test]
    fn create_json_output() {
        let output = create_output("json", Some("/tmp/test.json"));
        assert!(output.is_ok());
        assert!(output.unwrap().description().contains("json"));
    }

    #[test]
    fn create_csv_output() {
        let output = create_output("csv", Some("/tmp/test.csv"));
        assert!(output.is_ok());
    }

    #[test]
    fn create_unknown_output_fails() {
        let output = create_output("unknown", None);
        assert!(output.is_err());
    }

    #[test]
    fn snapshot_to_samples_converts() {
        use crate::metrics::{MetricsSnapshot, TrendStats};

        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![("vus".to_string(), 5.0, 1.0, 10.0)],
            rates: vec![("checks".to_string(), 0.95, 95, 100)],
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

        let samples = snapshot_to_samples(&snapshot, 10.0);
        assert_eq!(samples.len(), 4);
        assert_eq!(samples[0].metric, "http_reqs");
        assert_eq!(samples[1].metric, "vus");
        assert_eq!(samples[2].metric, "checks");
        assert_eq!(samples[3].metric, "http_req_duration");
    }
}
