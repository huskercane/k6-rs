//! CSV output plugin — writes metric samples as CSV rows.
//!
//! Usage: `--out csv=results.csv`
//! Columns: metric_name, timestamp, metric_value, ...tags

use std::fs::File;
use std::io::{BufWriter, Write};

use super::{snapshot_to_samples, MetricValue, Output};
use crate::metrics::MetricsSnapshot;

pub struct CsvOutput {
    path: String,
    writer: Option<BufWriter<File>>,
}

impl CsvOutput {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            writer: None,
        }
    }
}

impl Output for CsvOutput {
    fn start(&mut self) -> anyhow::Result<()> {
        let file = File::create(&self.path)?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "metric_name,timestamp,metric_type,metric_value,tags"
        )?;
        self.writer = Some(writer);
        Ok(())
    }

    fn add_snapshot(
        &mut self,
        snapshot: &MetricsSnapshot,
        elapsed_secs: f64,
    ) -> anyhow::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("CSV output not started"))?;

        let samples = snapshot_to_samples(snapshot, elapsed_secs);
        for sample in &samples {
            let value_str = match &sample.value {
                MetricValue::Counter { count, rate } => format!("{count},{rate:.6}"),
                MetricValue::Gauge { value, min, max } => format!("{value},{min},{max}"),
                MetricValue::Rate { rate, passes, total } => {
                    format!("{rate:.6},{passes},{total}")
                }
                MetricValue::Trend {
                    avg,
                    min,
                    med,
                    max,
                    p90,
                    p95,
                    count,
                } => format!("{avg:.2},{min:.2},{med:.2},{max:.2},{p90:.2},{p95:.2},{count}"),
            };

            let tags_str = sample
                .tags
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(";");

            writeln!(
                writer,
                "{},{:.3},{:?},{},{}",
                sample.metric, sample.timestamp, sample.metric_type, value_str, tags_str
            )?;
        }
        writer.flush()?;

        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        Ok(())
    }

    fn description(&self) -> String {
        format!("csv ({})", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSnapshot, TrendStats};

    #[test]
    fn csv_output_writes_header_and_rows() {
        let dir = std::env::temp_dir().join("k6rs_csv_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.csv");

        let mut output = CsvOutput::new(path.to_str().unwrap());
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
            gauges: vec![],
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

        output.add_snapshot(&snapshot, 5.0).unwrap();
        output.stop().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], "metric_name,timestamp,metric_type,metric_value,tags");
        assert_eq!(lines.len(), 4); // header + 3 metrics

        let _ = std::fs::remove_dir_all(&dir);
    }
}
