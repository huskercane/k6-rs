//! DuckDB output plugin — writes metrics to a DuckDB file for post-hoc analysis.
//!
//! Usage: `--out duckdb=results.duckdb`
//! Creates tables: counters, gauges, rates, trends with timestamps.

use std::fs::File;
use std::io::{BufWriter, Write};

use super::{snapshot_to_samples, MetricValue, Output};
use crate::metrics::MetricsSnapshot;

/// DuckDB output — writes metrics as CSV files that can be loaded into DuckDB.
///
/// Since the `duckdb` crate adds significant compile time and binary size,
/// we output CSV files alongside a `.sql` loader script. Users can then:
/// ```sh
/// duckdb results.duckdb < results.sql
/// ```
pub struct DuckDbOutput {
    path: String,
    csv_path: String,
    sql_path: String,
    writer: Option<BufWriter<File>>,
}

impl DuckDbOutput {
    pub fn new(path: &str) -> Self {
        let base = path.trim_end_matches(".duckdb");
        Self {
            path: path.to_string(),
            csv_path: format!("{base}_metrics.csv"),
            sql_path: format!("{base}_load.sql"),
            writer: None,
        }
    }
}

impl Output for DuckDbOutput {
    fn start(&mut self) -> anyhow::Result<()> {
        let file = File::create(&self.csv_path)?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "metric_name,metric_type,timestamp_ms,value_avg,value_min,value_med,value_max,value_p90,value_p95,value_count,value_rate,tags"
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
            .ok_or_else(|| anyhow::anyhow!("DuckDB output not started"))?;

        let samples = snapshot_to_samples(snapshot, elapsed_secs);

        for sample in &samples {
            let tags_str = sample
                .tags
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(";");

            let (avg, min, med, max, p90, p95, count, rate) = match &sample.value {
                MetricValue::Counter { count, rate } => {
                    (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, *count, *rate)
                }
                MetricValue::Gauge { value, min, max } => {
                    (*value, *min, 0.0, *max, 0.0, 0.0, 0, 0.0)
                }
                MetricValue::Rate { rate, passes, total } => {
                    (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, *total, *rate)
                }
                MetricValue::Trend {
                    avg,
                    min,
                    med,
                    max,
                    p90,
                    p95,
                    count,
                } => (*avg, *min, *med, *max, *p90, *p95, *count, 0.0),
            };

            writeln!(
                writer,
                "{},{:?},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{:.6},{}",
                sample.metric,
                sample.metric_type,
                sample.timestamp,
                avg,
                min,
                med,
                max,
                p90,
                p95,
                count,
                rate,
                tags_str,
            )?;
        }
        writer.flush()?;

        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }

        // Write the SQL loader script
        let csv_filename = std::path::Path::new(&self.csv_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();

        let sql = format!(
            r#"-- Load k6-rs metrics into DuckDB
-- Usage: duckdb {db} < {sql}

CREATE TABLE IF NOT EXISTS metrics AS
SELECT * FROM read_csv_auto('{csv}');

-- Useful queries:
-- SELECT metric_name, AVG(value_avg) FROM metrics WHERE metric_type = 'trend' GROUP BY metric_name;
-- SELECT * FROM metrics WHERE metric_name = 'http_req_duration' ORDER BY timestamp_ms;
"#,
            db = self.path,
            sql = std::path::Path::new(&self.sql_path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            csv = csv_filename,
        );
        std::fs::write(&self.sql_path, sql)?;

        Ok(())
    }

    fn description(&self) -> String {
        format!("duckdb ({})", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSnapshot, TrendStats};

    #[test]
    fn duckdb_output_writes_csv_and_sql() {
        let dir = std::env::temp_dir().join("k6rs_duckdb_test");
        let _ = std::fs::create_dir_all(&dir);
        let db_path = dir.join("test.duckdb");

        let mut output = DuckDbOutput::new(db_path.to_str().unwrap());
        output.start().unwrap();

        let snapshot = MetricsSnapshot {
            counters: vec![("http_reqs".to_string(), 100, 10.0)],
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
        output.stop().unwrap();

        // CSV should exist with header + 2 rows
        let csv_content = std::fs::read_to_string(&output.csv_path).unwrap();
        let lines: Vec<&str> = csv_content.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 metrics

        // SQL loader should exist
        let sql_content = std::fs::read_to_string(&output.sql_path).unwrap();
        assert!(sql_content.contains("CREATE TABLE"));
        assert!(sql_content.contains("read_csv_auto"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
