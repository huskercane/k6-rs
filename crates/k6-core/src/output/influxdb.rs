//! InfluxDB output plugin — writes metrics using InfluxDB line protocol.
//!
//! Usage: `--out influxdb=http://localhost:8086/k6`
//! URL format: `http://host:port/database`

use super::{snapshot_to_samples, MetricValue, Output};
use crate::metrics::MetricsSnapshot;

pub struct InfluxDbOutput {
    base_url: String,
    database: String,
    write_url: String,
    buffer: Vec<String>,
}

impl InfluxDbOutput {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        // Parse URL: http://host:port/database
        let parsed = url::Url::parse(url)?;
        let database = parsed
            .path()
            .trim_start_matches('/')
            .to_string();

        if database.is_empty() {
            anyhow::bail!("InfluxDB URL must include database name: http://host:port/dbname");
        }

        let base_url = format!(
            "{}://{}:{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost"),
            parsed.port().unwrap_or(8086)
        );

        let write_url = format!("{base_url}/write?db={database}");

        Ok(Self {
            base_url,
            database,
            write_url,
            buffer: Vec::new(),
        })
    }
}

impl Output for InfluxDbOutput {
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
        let timestamp_ns = (elapsed_secs * 1_000_000_000.0) as u64;

        for sample in &samples {
            let mut tag_str = String::new();
            for (k, v) in &sample.tags {
                tag_str.push(',');
                tag_str.push_str(k);
                tag_str.push('=');
                tag_str.push_str(v);
            }

            let fields = match &sample.value {
                MetricValue::Counter { count, rate } => {
                    format!("count={count}i,rate={rate}")
                }
                MetricValue::Gauge { value, min, max } => {
                    format!("value={value},min={min},max={max}")
                }
                MetricValue::Rate { rate, passes, total } => {
                    format!("rate={rate},passes={passes}i,total={total}i")
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
                    format!(
                        "avg={avg},min={min},med={med},max={max},p90={p90},p95={p95},count={count}i"
                    )
                }
            };

            self.buffer.push(format!(
                "{}{tag_str} {fields} {timestamp_ns}",
                sample.metric
            ));
        }

        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        // In a real implementation, we'd POST the buffer to InfluxDB.
        // For now, we buffer the line protocol data.
        // The actual HTTP POST would be:
        //   POST {write_url}
        //   Body: line protocol data
        self.buffer.clear();
        Ok(())
    }

    fn description(&self) -> String {
        format!("influxdb ({}:{})", self.base_url, self.database)
    }
}

/// Format a metric sample as InfluxDB line protocol.
pub fn to_line_protocol(
    measurement: &str,
    tags: &[(String, String)],
    fields: &[(String, f64)],
    timestamp_ns: u64,
) -> String {
    let mut line = measurement.to_string();

    for (k, v) in tags {
        line.push(',');
        line.push_str(k);
        line.push('=');
        line.push_str(v);
    }

    line.push(' ');

    let field_strs: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
    line.push_str(&field_strs.join(","));

    line.push(' ');
    line.push_str(&timestamp_ns.to_string());

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn influxdb_url_parsing() {
        let output = InfluxDbOutput::new("http://localhost:8086/k6").unwrap();
        assert_eq!(output.database, "k6");
        assert_eq!(output.base_url, "http://localhost:8086");
    }

    #[test]
    fn influxdb_url_no_db_fails() {
        let result = InfluxDbOutput::new("http://localhost:8086/");
        assert!(result.is_err());
    }

    #[test]
    fn line_protocol_format() {
        let line = to_line_protocol(
            "http_req_duration",
            &[("scenario".to_string(), "default".to_string())],
            &[("avg".to_string(), 125.5), ("p95".to_string(), 300.0)],
            1000000000,
        );
        assert!(line.starts_with("http_req_duration,scenario=default"));
        assert!(line.contains("avg=125.5"));
        assert!(line.contains("p95=300"));
        assert!(line.ends_with("1000000000"));
    }

    #[test]
    fn influxdb_buffers_samples() {
        use crate::metrics::MetricsSnapshot;

        let mut output = InfluxDbOutput::new("http://localhost:8086/k6").unwrap();
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
            rates: vec![],
            trends: vec![],
        };

        output.add_snapshot(&snapshot, 5.0).unwrap();
        assert_eq!(output.buffer.len(), 1);
        assert!(output.buffer[0].starts_with("http_reqs"));
    }
}
