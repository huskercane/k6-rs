//! JSON output — event-stream emitter (CG-6). Matches upstream's
//! `--out json` wire format: NDJSON of `Metric` definitions and `Point`
//! samples, RFC3339 timestamps, ~200ms flush cadence.
//!
//! Architecture: the work lives in [`event_stream`](super::event_stream). This
//! module is a thin `Output` adapter that owns the writer task handle and
//! holds the path so the snapshot-based `Output` trait still composes with
//! the runner. `add_snapshot` is intentionally a no-op — sample-level
//! emission happens via the [`EventSink`] installed on the registry; the
//! periodic snapshot is irrelevant to this output.
//!
//! Usage: `--out json=results.json`. The companion diagnostics file is
//! written at `results.json.diagnostics.json` (per the CG-6 design's
//! sidecar contract; see `event_stream::DiagnosticsSidecar`).

use std::path::PathBuf;

use super::Output;
use super::event_stream::{self, DEFAULT_CHANNEL_CAPACITY, EventSink};
use crate::metrics::MetricsSnapshot;

/// Builder for the JSON output. Caller `take_sink()` after `new` to install
/// the sink on the `MetricsRegistry`. The writer task is spawned on `start`.
pub struct JsonOutput {
    path: PathBuf,
    capacity: usize,
    sink: Option<EventSink>,
    writer_handle: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl JsonOutput {
    pub fn new(path: &str) -> Self {
        Self::with_capacity(path, DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(path: &str, capacity: usize) -> Self {
        Self {
            path: PathBuf::from(path),
            capacity,
            sink: None,
            writer_handle: None,
        }
    }
}

impl Output for JsonOutput {
    fn start(&mut self) -> anyhow::Result<()> {
        let (sink, handle) = event_stream::build(self.path.clone(), self.capacity)?;
        self.sink = Some(sink);
        self.writer_handle = Some(handle);
        Ok(())
    }

    /// CG-6: snapshot-style emission is replaced by per-sample event
    /// emission via the `EventSink`. This method is intentionally a
    /// no-op — the sample stream is fed directly from the metric record
    /// sites in `MetricsRegistry`, not from periodic snapshots.
    fn add_snapshot(
        &mut self,
        _snapshot: &MetricsSnapshot,
        _elapsed_secs: f64,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        // Dropping the sink closes the channel. The writer task drains
        // remaining events and writes the sidecar. The caller is expected
        // to await `writer_handle` to ensure clean shutdown.
        self.sink = None;
        Ok(())
    }

    fn description(&self) -> String {
        format!("json ({})", self.path.display())
    }

    fn event_sink(&self) -> Option<EventSink> {
        self.sink.clone()
    }

    fn take_writer_handle(&mut self) -> Option<tokio::task::JoinHandle<std::io::Result<()>>> {
        self.writer_handle.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsRegistry;
    use std::collections::BTreeMap;
    use std::io::BufRead;

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("k6rs_json_output_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{name}.json"))
    }

    /// End-to-end: write a metric through the `MetricsRegistry` with the
    /// sink installed → event ends up in the NDJSON file with the expected
    /// wire shape. Locks the integration: registry hot path → sink →
    /// writer task → file.
    #[tokio::test]
    async fn registry_write_lands_in_json_stream() {
        let path = tmp_path("registry_to_stream");
        let mut out = JsonOutput::with_capacity(path.to_str().unwrap(), 16);
        out.start().unwrap();
        let sink = Output::event_sink(&out).expect("sink available after start");

        let reg = MetricsRegistry::new();
        reg.set_event_sink(sink).expect("first install");

        let mut tags = BTreeMap::new();
        tags.insert("status".into(), "200".into());
        reg.counter_add_with_tags("http_reqs", 1, &tags);
        reg.trend_add_with_tags("http_req_duration", 12.5, &tags);

        // Stop + clear registry's sender clone + await writer drain.
        // Same ordering invariant as main.rs: all EventSink clones must be
        // dropped before the writer's `rx.recv()` can return None.
        out.stop().unwrap();
        reg.clear_event_sink();
        let handle = Output::take_writer_handle(&mut out).unwrap();
        handle.await.unwrap().unwrap();

        let f = std::fs::File::open(&path).unwrap();
        let lines: Vec<serde_json::Value> = std::io::BufReader::new(f)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(&l).unwrap())
            .collect();

        // Expected: 2 Metric events (http_reqs, http_req_duration) + 2 Point events.
        let metric_defs: Vec<_> = lines.iter().filter(|l| l["type"] == "Metric").collect();
        let points: Vec<_> = lines.iter().filter(|l| l["type"] == "Point").collect();
        assert_eq!(metric_defs.len(), 2, "two metric defs expected");
        assert_eq!(points.len(), 2, "two point events expected");

        // Tag canonicalization: the storage key would be
        // `http_reqs{status:200}` but the sink emits the base name with
        // the tag as a structured field.
        for p in &points {
            assert_eq!(p["data"]["tags"]["status"], "200");
        }
    }
}
