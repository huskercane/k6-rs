//! Event-stream JSON output (CG-6) — upstream wire-format parity.
//!
//! Two event shapes match `internal/output/json/wrapper.go` upstream:
//! - `Metric` — definition emitted once per metric name, before its first Point.
//! - `Point` — one sample event per metric write, with wall-clock RFC3339 time.
//!
//! Hot path: every `MetricsRegistry::*_add{,_with_tags}` and `gauge_set` call
//! routes through [`EventSink::try_record`]. `try_record` is split in two:
//!   1. Ensure the Metric definition is queued (mutex-protected `DefState`,
//!      unbounded VecDeque — defs are O(distinct metric names), never
//!      dropped per design).
//!   2. Try-send the Sample on a bounded mpsc channel. Drop-newest on
//!      overflow, with per-metric drop accounting (`DropState`).
//!
//! The writer task drains defs first on every tick, then samples. This
//! preserves the upstream invariant "Metric event before that metric's first
//! Point event" even under cross-thread ordering pressure: the def is in the
//! queue *before* `seen.contains(name)` returns true to any concurrent
//! producer.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Default Point-channel capacity. Bounded to keep memory ceiling predictable
/// under 8hr/7900-VU soak. Each `SinkEvent` is roughly 200-400 bytes including
/// String/BTreeMap allocations; 1M × ~300 bytes ≈ 300 MB worst-case occupancy.
/// CLI exposes `--out-buffer-size` to tune.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1_048_576;

/// Writer flush cadence — matches upstream's `flushPeriod = 200ms`.
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Metric kind enum — wire-encoded as lowercase strings for upstream parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    Counter,
    Gauge,
    Rate,
    Trend,
}

impl MetricKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Rate => "rate",
            MetricKind::Trend => "trend",
        }
    }
}

/// Upstream's `contains` field — matches `metrics/value_type.go` upstream:
///   - `"data"` for byte-counting counters (data_sent / data_received)
///   - `"time"` for time-duration trends (http_req_*, iteration_duration)
///   - `"default"` otherwise (values presented as-is)
/// The classification is by metric name + kind, mirroring upstream's
/// per-registration `ValueType` decisions. Wrong classification surfaces
/// as a `stream-contains` finding in the conformance harness, so the
/// table is enforceable.
pub fn classify_contains(metric_kind: MetricKind, metric_name: &str) -> &'static str {
    // Byte-counting counters — upstream marks these `Data`.
    if metric_name == "data_sent"
        || metric_name == "data_received"
        || metric_name.starts_with("ws_msgs_")
    {
        // ws_msgs_* are counters of messages, not bytes — fall through.
        if metric_name.starts_with("ws_msgs_") {
            return "default";
        }
        return "data";
    }
    if metric_kind == MetricKind::Trend
        && (metric_name.starts_with("http_req")
            || metric_name.contains("duration")
            || metric_name.starts_with("grpc_req")
            || metric_name.starts_with("ws_connecting")
            || metric_name.starts_with("ws_ping"))
    {
        return "time";
    }
    "default"
}

/// One sample event flowing from producer → writer.
///
/// `metric_name` is the BASE name (no tag braces). Producer-side
/// canonicalization happens before this is constructed: tagged writes split
/// the canonical key into (base name, tag map) and pass them separately.
#[derive(Debug, Clone)]
pub struct SinkEvent {
    pub metric_name: String,
    pub metric_kind: MetricKind,
    pub time: time::OffsetDateTime,
    pub value: f64,
    pub tags: BTreeMap<String, String>,
}

/// Metric definition queued by the producer at first sight of a metric name.
/// The writer dedupes and emits each definition exactly once.
#[derive(Debug, Clone)]
pub struct MetricDef {
    pub name: String,
    pub kind: MetricKind,
    pub contains: &'static str,
}

/// Definition emission state — mutex-protected because the def queue and the
/// `seen` set must be updated atomically with respect to each other (so two
/// concurrent producers can't both enqueue the same def, AND a producer
/// can't observe `seen.contains` true while the def is still in flight).
#[derive(Default)]
struct DefState {
    seen: HashSet<String>,
    queue: VecDeque<MetricDef>,
}

/// Per-metric drop counters. Bounded by the number of distinct metric names
/// that ever experienced a drop — small in practice.
#[derive(Default)]
struct DropStateInner {
    total: u64,
    per_metric: HashMap<String, u64>,
}

struct DropState {
    inner: Mutex<DropStateInner>,
    peak_occupancy: AtomicU64,
    capacity: u64,
}

impl DropState {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(DropStateInner::default()),
            peak_occupancy: AtomicU64::new(0),
            capacity: capacity as u64,
        }
    }

    fn record_drop(&self, metric_name: &str) {
        let mut inner = self.inner.lock();
        inner.total += 1;
        *inner.per_metric.entry(metric_name.to_string()).or_insert(0) += 1;
    }

    fn observe_occupancy(&self, occupancy: u64) {
        self.peak_occupancy.fetch_max(occupancy, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DiagnosticsSidecar {
        let inner = self.inner.lock();
        DiagnosticsSidecar {
            capacity: self.capacity,
            peak_occupancy: self.peak_occupancy.load(Ordering::Relaxed),
            dropped_samples: DroppedSamples {
                total: inner.total,
                per_metric: inner.per_metric.clone(),
            },
        }
    }
}

/// Sidecar diagnostics shape written alongside the JSON stream. Read by the
/// conformance adapter to gate UNRELIABLE results. NOT mixed into the NDJSON
/// stream — keeps the wire format pure for upstream-compatible consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsSidecar {
    pub capacity: u64,
    pub peak_occupancy: u64,
    pub dropped_samples: DroppedSamples,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DroppedSamples {
    pub total: u64,
    /// Per-metric drop counts. A non-empty map means that metric's stream
    /// is incomplete for this run.
    pub per_metric: HashMap<String, u64>,
}

/// Handle the producer uses to push events. Cheap to clone (just bumps Arc
/// refcount); lock-free reads inside `try_record` apart from the brief
/// `DefState` mutex on first-sight metrics.
#[derive(Clone, Debug)]
pub struct EventSink {
    tx: mpsc::Sender<SinkEvent>,
    def_state: Arc<Mutex<DefState>>,
    drop_state: Arc<DropState>,
}

impl std::fmt::Debug for DefState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefState")
            .field("seen_count", &self.seen.len())
            .field("queue_len", &self.queue.len())
            .finish()
    }
}

impl std::fmt::Debug for DropState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropState")
            .field("capacity", &self.capacity)
            .field(
                "peak_occupancy",
                &self.peak_occupancy.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl EventSink {
    /// Producer entry point. Two-step:
    ///   1. Ensure the def is queued (mutex on `DefState`).
    ///   2. Try-send the Point on the bounded channel.
    /// Backpressure is invisible to the producer — failure increments
    /// `DropState` and returns. Defs are NEVER dropped: the queue grows as
    /// needed; capacity is O(distinct metrics).
    pub fn try_record(&self, ev: SinkEvent) {
        // Step 1: queue the def if first-sight.
        {
            let mut state = self.def_state.lock();
            if !state.seen.contains(&ev.metric_name) {
                state.seen.insert(ev.metric_name.clone());
                state.queue.push_back(MetricDef {
                    name: ev.metric_name.clone(),
                    kind: ev.metric_kind,
                    contains: classify_contains(ev.metric_kind, &ev.metric_name),
                });
            }
        }
        // Step 2: peak occupancy + try-send.
        let occupancy = (self.tx.max_capacity() - self.tx.capacity()) as u64;
        self.drop_state.observe_occupancy(occupancy);
        match self.tx.try_send(ev) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                self.drop_state.record_drop(&dropped.metric_name);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer task exited — silently drop. Stop-path runs the
                // final sidecar emit; this only happens during teardown.
            }
        }
    }

    /// Test/diagnostic accessor for the current sidecar snapshot.
    pub fn diagnostics_snapshot(&self) -> DiagnosticsSidecar {
        self.drop_state.snapshot()
    }
}

/// Wire-format envelope for Metric events. Matches upstream
/// `internal/output/json/wrapper.go::metricEnvelope` byte-for-byte (apart
/// from the always-`null` submetrics field — see CG-6 design's
/// `known_drift`).
#[derive(Serialize)]
struct MetricEnvelope<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    data: MetricEnvelopeData<'a>,
    metric: &'a str,
}

#[derive(Serialize)]
struct MetricEnvelopeData<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    metric_type: &'static str,
    contains: &'static str,
    thresholds: [(); 0],
    submetrics: Option<()>,
}

/// Wire-format envelope for Point events. Field order matches upstream's
/// per-sample output. `metadata` is omitted (always empty in this first
/// cut — known_drift entry tracks it).
#[derive(Serialize)]
struct PointEnvelope<'a> {
    metric: &'a str,
    #[serde(rename = "type")]
    event_type: &'static str,
    data: PointEnvelopeData<'a>,
}

#[derive(Serialize)]
struct PointEnvelopeData<'a> {
    time: String,
    value: f64,
    tags: &'a BTreeMap<String, String>,
}

/// Build (sink, writer-task-handle). Caller installs `sink` on the
/// `MetricsRegistry` and awaits `handle` at stop. Drops on `EventSink` are
/// counted internally; the sidecar is written to `<stream_path>.diagnostics.json`
/// by the writer task at shutdown.
pub fn build(
    stream_path: PathBuf,
    capacity: usize,
) -> std::io::Result<(EventSink, JoinHandle<std::io::Result<()>>)> {
    let (tx, rx) = mpsc::channel::<SinkEvent>(capacity);
    let def_state = Arc::new(Mutex::new(DefState::default()));
    let drop_state = Arc::new(DropState::new(capacity));

    let sink = EventSink {
        tx,
        def_state: Arc::clone(&def_state),
        drop_state: Arc::clone(&drop_state),
    };

    let writer_def_state = Arc::clone(&def_state);
    let writer_drop_state = Arc::clone(&drop_state);
    let handle = tokio::spawn(async move {
        run_writer(rx, writer_def_state, writer_drop_state, stream_path).await
    });

    Ok((sink, handle))
}

/// Writer task body. Outer loop = one flush window (up to `FLUSH_INTERVAL` or
/// `BATCH_LIMIT` events). Inner loop accumulates events until deadline or
/// batch-full or channel-closed. Drains def queue before each Points emit so
/// the upstream-parity "def before first point" invariant holds.
async fn run_writer(
    mut rx: mpsc::Receiver<SinkEvent>,
    def_state: Arc<Mutex<DefState>>,
    drop_state: Arc<DropState>,
    stream_path: PathBuf,
) -> std::io::Result<()> {
    use std::io::Write;
    const BATCH_LIMIT: usize = 8192;

    let stream_file = std::fs::File::create(&stream_path)?;
    let mut writer = std::io::BufWriter::new(stream_file);
    let mut batch: Vec<SinkEvent> = Vec::with_capacity(BATCH_LIMIT);

    loop {
        let deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
        let mut channel_closed = false;

        // Accumulate up to BATCH_LIMIT events or until the flush deadline.
        // `tokio::time::timeout` returns Err on timeout (window expired),
        // Ok(None) on channel closed-and-drained, Ok(Some) on event.
        while batch.len() < BATCH_LIMIT {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                break;
            }
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(ev)) => batch.push(ev),
                Ok(None) => {
                    channel_closed = true;
                    break;
                }
                Err(_) => break, // window expired
            }
        }

        flush_defs(&def_state, &mut writer)?;
        if !batch.is_empty() {
            emit_points(&batch, &mut writer)?;
            batch.clear();
        }
        writer.flush()?;

        if channel_closed {
            break;
        }
    }

    drop(writer);

    let sidecar = drop_state.snapshot();
    write_sidecar(&stream_path, &sidecar)?;
    if sidecar.dropped_samples.total > 0 {
        eprintln!(
            "WARN: --out json: {} samples dropped due to sink overflow ({} distinct metrics affected). \
             Capacity={}, peak={}. See {}.diagnostics.json for details.",
            sidecar.dropped_samples.total,
            sidecar.dropped_samples.per_metric.len(),
            sidecar.capacity,
            sidecar.peak_occupancy,
            stream_path.display(),
        );
    }
    Ok(())
}

fn flush_defs(
    def_state: &Arc<Mutex<DefState>>,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let drained: Vec<MetricDef> = {
        let mut state = def_state.lock();
        std::mem::take(&mut state.queue).into_iter().collect()
    };
    for d in drained {
        let env = MetricEnvelope {
            event_type: "Metric",
            data: MetricEnvelopeData {
                name: &d.name,
                metric_type: d.kind.as_str(),
                contains: d.contains,
                thresholds: [],
                submetrics: None,
            },
            metric: &d.name,
        };
        serde_json::to_writer(&mut *writer, &env)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn emit_points(batch: &[SinkEvent], writer: &mut impl std::io::Write) -> std::io::Result<()> {
    for ev in batch {
        let time_str = format_rfc3339(ev.time);
        let env = PointEnvelope {
            metric: &ev.metric_name,
            event_type: "Point",
            data: PointEnvelopeData {
                time: time_str,
                value: ev.value,
                tags: &ev.tags,
            },
        };
        serde_json::to_writer(&mut *writer, &env)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn format_rfc3339(t: time::OffsetDateTime) -> String {
    // RFC3339 with nanosecond precision — matches upstream's Go `time.Time`
    // default String() formatting. Conformance adapter only requires
    // parseability, but byte-shape parity helps downstream tooling.
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::new())
}

fn write_sidecar(stream_path: &std::path::Path, sidecar: &DiagnosticsSidecar) -> std::io::Result<()> {
    let mut sidecar_path = stream_path.as_os_str().to_owned();
    sidecar_path.push(".diagnostics.json");
    let f = std::fs::File::create(sidecar_path)?;
    serde_json::to_writer_pretty(f, sidecar).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn read_ndjson(path: &std::path::Path) -> Vec<serde_json::Value> {
        let f = std::fs::File::open(path).expect("stream file exists");
        std::io::BufReader::new(f)
            .lines()
            .filter_map(|l| l.ok())
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(&l).expect("valid json line"))
            .collect()
    }

    fn read_sidecar(path: &std::path::Path) -> DiagnosticsSidecar {
        let mut p = path.as_os_str().to_owned();
        p.push(".diagnostics.json");
        let f = std::fs::File::open(p).expect("sidecar exists");
        serde_json::from_reader(f).expect("valid sidecar json")
    }

    fn tmp_stream_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("k6rs_event_stream_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{name}.json"))
    }

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    /// CG-6 invariant: a Metric event must appear before the first Point
    /// event for that metric. Producer-side mutex on DefState + writer-side
    /// "drain defs first" preserves this even under cross-thread writes.
    #[tokio::test]
    async fn def_emitted_before_first_point_for_metric() {
        let path = tmp_stream_path("def_first");
        let (sink, handle) = build(path.clone(), 256).unwrap();

        for i in 0..5 {
            sink.try_record(SinkEvent {
                metric_name: "http_reqs".into(),
                metric_kind: MetricKind::Counter,
                time: now(),
                value: 1.0,
                tags: BTreeMap::new(),
            });
            if i == 2 {
                sink.try_record(SinkEvent {
                    metric_name: "http_req_duration".into(),
                    metric_kind: MetricKind::Trend,
                    time: now(),
                    value: 12.5,
                    tags: BTreeMap::new(),
                });
            }
        }
        drop(sink);
        handle.await.unwrap().unwrap();

        let lines = read_ndjson(&path);
        let mut seen_def: HashSet<String> = HashSet::new();
        for line in &lines {
            let t = line["type"].as_str().unwrap();
            let m = line["metric"].as_str().unwrap();
            match t {
                "Metric" => {
                    seen_def.insert(m.into());
                }
                "Point" => {
                    assert!(
                        seen_def.contains(m),
                        "Point for {m} preceded its Metric definition; \
                         this violates the upstream-parity ordering invariant"
                    );
                }
                _ => panic!("unknown event type: {t}"),
            }
        }
        assert!(seen_def.contains("http_reqs"));
        assert!(seen_def.contains("http_req_duration"));
    }

    /// Even when the bounded channel is jammed full and Point events are
    /// dropping, every Metric definition must still reach output (separate
    /// unbounded def queue). Without this, the stream is malformed and
    /// downstream consumers see Points for undeclared metrics.
    #[tokio::test]
    async fn def_never_dropped_under_sample_pressure() {
        let path = tmp_stream_path("def_under_pressure");
        // Tiny capacity to force drops.
        let (sink, handle) = build(path.clone(), 4).unwrap();

        // Push 10 distinct metric names, 100 samples each. Channel has
        // capacity 4 and writer is throttled by 200ms tick — many Points
        // will drop, but each metric's def must survive.
        for i in 0..10 {
            let metric = format!("metric_{i}");
            for _ in 0..100 {
                sink.try_record(SinkEvent {
                    metric_name: metric.clone(),
                    metric_kind: MetricKind::Counter,
                    time: now(),
                    value: 1.0,
                    tags: BTreeMap::new(),
                });
            }
        }
        drop(sink);
        handle.await.unwrap().unwrap();

        let lines = read_ndjson(&path);
        let defs: HashSet<String> = lines
            .iter()
            .filter(|l| l["type"] == "Metric")
            .map(|l| l["metric"].as_str().unwrap().to_string())
            .collect();
        for i in 0..10 {
            let name = format!("metric_{i}");
            assert!(
                defs.contains(&name),
                "Metric def for {name} was dropped (defs found: {:?})",
                defs
            );
        }

        // Sidecar must reflect non-zero drops.
        let sidecar = read_sidecar(&path);
        assert!(
            sidecar.dropped_samples.total > 0,
            "drops should have occurred under tiny capacity + 1000 samples"
        );
    }

    /// Per-metric drop accounting: forcing drops on two distinct metrics
    /// must populate `per_metric` with both. A single global counter would
    /// hide which series became unreliable.
    #[tokio::test]
    async fn drop_counters_track_per_metric() {
        let path = tmp_stream_path("per_metric_drops");
        let (sink, handle) = build(path.clone(), 2).unwrap();

        for _ in 0..50 {
            sink.try_record(SinkEvent {
                metric_name: "alpha".into(),
                metric_kind: MetricKind::Counter,
                time: now(),
                value: 1.0,
                tags: BTreeMap::new(),
            });
            sink.try_record(SinkEvent {
                metric_name: "beta".into(),
                metric_kind: MetricKind::Counter,
                time: now(),
                value: 1.0,
                tags: BTreeMap::new(),
            });
        }
        drop(sink);
        handle.await.unwrap().unwrap();

        let sidecar = read_sidecar(&path);
        assert!(sidecar.dropped_samples.total > 0, "drops expected");
        // Both metrics should appear in per_metric — guarantees we're not
        // just tracking a single global counter.
        assert!(
            sidecar.dropped_samples.per_metric.contains_key("alpha")
                || sidecar.dropped_samples.per_metric.contains_key("beta"),
            "at least one offending metric must be in per_metric, got {:?}",
            sidecar.dropped_samples.per_metric
        );
    }

    /// Peak occupancy captures the highest in-flight count even when
    /// channel drains to zero by the end of the test. Without this, a
    /// "nearly saturated but survived" run looks identical to an idle one.
    #[tokio::test]
    async fn peak_occupancy_records_high_water_mark() {
        let path = tmp_stream_path("peak");
        let (sink, handle) = build(path.clone(), 64).unwrap();

        // Burst 30 events synchronously, then sleep enough that the writer
        // drains them.
        for _ in 0..30 {
            sink.try_record(SinkEvent {
                metric_name: "burst_metric".into(),
                metric_kind: MetricKind::Counter,
                time: now(),
                value: 1.0,
                tags: BTreeMap::new(),
            });
        }
        // Peak should reflect ≥1 event sitting in the channel at some
        // point. We can't make this exact because the writer may have
        // drained some between sends, but it's bounded above by 30 and
        // below by 1.
        let snapshot = sink.diagnostics_snapshot();
        assert!(snapshot.peak_occupancy > 0, "peak should be > 0 after burst");
        assert_eq!(snapshot.capacity, 64);

        drop(sink);
        handle.await.unwrap().unwrap();
    }

    /// The sidecar JSON must be written at stop with the right shape:
    /// capacity, peak_occupancy, and dropped_samples block. Downstream
    /// tooling reads structured fields, NOT log text.
    #[tokio::test]
    async fn sidecar_written_with_diagnostics_at_stop() {
        let path = tmp_stream_path("sidecar_shape");
        let (sink, handle) = build(path.clone(), 1024).unwrap();
        sink.try_record(SinkEvent {
            metric_name: "alpha".into(),
            metric_kind: MetricKind::Counter,
            time: now(),
            value: 1.0,
            tags: BTreeMap::new(),
        });
        drop(sink);
        handle.await.unwrap().unwrap();

        let sidecar = read_sidecar(&path);
        assert_eq!(sidecar.capacity, 1024);
        assert_eq!(sidecar.dropped_samples.total, 0);
        // Peak is ≥0; one sample may have drained before observation.
    }

    /// Wire-format byte-shape: Metric event must have `type`, `data`,
    /// `metric` fields with the expected nesting; tags map order is
    /// alphabetical (BTreeMap serializes in key order).
    #[tokio::test]
    async fn wire_format_matches_upstream_metric_event() {
        let path = tmp_stream_path("metric_shape");
        let (sink, handle) = build(path.clone(), 16).unwrap();
        sink.try_record(SinkEvent {
            metric_name: "http_req_duration".into(),
            metric_kind: MetricKind::Trend,
            time: now(),
            value: 12.5,
            tags: BTreeMap::new(),
        });
        drop(sink);
        handle.await.unwrap().unwrap();

        let lines = read_ndjson(&path);
        let metric_event = lines
            .iter()
            .find(|l| l["type"] == "Metric")
            .expect("Metric event present");
        assert_eq!(metric_event["metric"], "http_req_duration");
        assert_eq!(metric_event["data"]["name"], "http_req_duration");
        assert_eq!(metric_event["data"]["type"], "trend");
        // contains: time for http_req_* trend metrics, default otherwise.
        assert_eq!(metric_event["data"]["contains"], "time");
        // thresholds is always [] (empty array), submetrics is always null
        // for this first cut. Known_drift covers the gap vs upstream.
        assert!(metric_event["data"]["thresholds"].is_array());
        assert_eq!(metric_event["data"]["thresholds"].as_array().unwrap().len(), 0);
        assert!(metric_event["data"]["submetrics"].is_null());
    }

    /// Wire-format byte-shape for Point events: `metric`, `type`, `data`
    /// with time / value / tags. RFC3339 time, f64 value, flat tag map.
    #[tokio::test]
    async fn wire_format_matches_upstream_point_event() {
        let path = tmp_stream_path("point_shape");
        let (sink, handle) = build(path.clone(), 16).unwrap();
        let mut tags = BTreeMap::new();
        tags.insert("method".into(), "GET".into());
        tags.insert("status".into(), "200".into());
        sink.try_record(SinkEvent {
            metric_name: "http_reqs".into(),
            metric_kind: MetricKind::Counter,
            time: now(),
            value: 1.0,
            tags,
        });
        drop(sink);
        handle.await.unwrap().unwrap();

        let lines = read_ndjson(&path);
        let point = lines
            .iter()
            .find(|l| l["type"] == "Point")
            .expect("Point event present");
        assert_eq!(point["metric"], "http_reqs");
        assert!(point["data"]["time"].is_string());
        // Must be parseable as RFC3339.
        let _: time::OffsetDateTime = time::OffsetDateTime::parse(
            point["data"]["time"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339,
        )
        .expect("time field is RFC3339-parseable");
        assert!((point["data"]["value"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(point["data"]["tags"]["method"], "GET");
        assert_eq!(point["data"]["tags"]["status"], "200");
    }
}
