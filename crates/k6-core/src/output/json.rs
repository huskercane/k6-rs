//! JSON output plugin — writes metric samples as newline-delimited JSON.
//!
//! Usage: `--out json=results.json`
//! Each line is a JSON object representing a metric sample.

use std::fs::File;
use std::io::{BufWriter, Write};

use super::{snapshot_to_samples, Output};
use crate::metrics::MetricsSnapshot;

pub struct JsonOutput {
    path: String,
    writer: Option<BufWriter<File>>,
}

impl JsonOutput {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            writer: None,
        }
    }
}

impl Output for JsonOutput {
    fn start(&mut self) -> anyhow::Result<()> {
        let file = File::create(&self.path)?;
        self.writer = Some(BufWriter::new(file));
        Ok(())
    }

    fn add_snapshot(&mut self, snapshot: &MetricsSnapshot, elapsed_secs: f64) -> anyhow::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("JSON output not started"))?;

        let samples = snapshot_to_samples(snapshot, elapsed_secs);
        for sample in &samples {
            serde_json::to_writer(&mut *writer, sample)?;
            writeln!(writer)?;
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
        format!("json ({})", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSnapshot, TrendStats};

    #[test]
    fn json_output_writes_ndjson() {
        let dir = std::env::temp_dir().join("k6rs_json_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.json");

        let mut output = JsonOutput::new(path.to_str().unwrap());
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 50, 5.0)],
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
                    count: 50,
                },
            )],
        };

        output.add_snapshot(&snapshot, 10.0).unwrap();
        output.stop().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2); // counter + trend

        // Each line should be valid JSON
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("metric").is_some());
            assert!(v.get("metric_type").is_some());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
