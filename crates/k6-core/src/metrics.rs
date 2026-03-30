use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A thread-safe metrics registry that collects all built-in and custom metrics.
///
/// VU threads send metric samples through lock-free atomics (Counter, Gauge, Rate)
/// or Mutex-guarded histograms (Trend). The Mutex is held only for the duration of
/// a single `record()` call (~nanoseconds), so contention is minimal.
#[derive(Default)]
pub struct MetricsRegistry {
    counters: Mutex<HashMap<String, CounterMetric>>,
    gauges: Mutex<HashMap<String, GaugeMetric>>,
    rates: Mutex<HashMap<String, RateMetric>>,
    trends: Mutex<HashMap<String, TrendMetric>>,
}

/// A monotonically increasing counter (e.g., http_reqs, iterations, data_sent).
#[derive(Debug)]
pub struct CounterMetric {
    pub value: AtomicU64,
}

/// A gauge that holds the latest value (e.g., vus, vus_max).
#[derive(Debug)]
pub struct GaugeMetric {
    pub value: AtomicU64,
    pub min: AtomicU64,
    pub max: AtomicU64,
}

/// A rate metric — tracks percentage of non-zero values (e.g., http_req_failed, checks).
#[derive(Debug)]
pub struct RateMetric {
    pub passes: AtomicU64,
    pub total: AtomicU64,
}

/// A trend metric backed by HdrHistogram for percentile calculations.
/// (e.g., http_req_duration, iteration_duration).
pub struct TrendMetric {
    histogram: hdrhistogram::Histogram<u64>,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// The type of data a metric contains (affects display formatting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricContains {
    Time,
    Data,
    Default,
}

/// Snapshot of a trend metric's statistics.
#[derive(Debug, Clone)]
pub struct TrendStats {
    pub avg: f64,
    pub min: f64,
    pub med: f64,
    pub max: f64,
    pub p90: f64,
    pub p95: f64,
    pub count: u64,
}

/// Snapshot of all metrics for summary output.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub counters: Vec<(String, u64, f64)>,    // name, value, rate_per_sec
    pub gauges: Vec<(String, f64, f64, f64)>, // name, value, min, max
    pub rates: Vec<(String, f64, u64, u64)>,  // name, rate, passes, total
    pub trends: Vec<(String, TrendStats)>,     // name, stats
}

impl CounterMetric {
    fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }
}

impl GaugeMetric {
    fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }
}

impl RateMetric {
    fn new() -> Self {
        Self {
            passes: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }
}

impl TrendMetric {
    fn new() -> Self {
        Self {
            // Record values in microseconds — covers 1us to ~1 hour
            histogram: hdrhistogram::Histogram::new_with_bounds(1, 3_600_000_000, 3)
                .expect("valid histogram params"),
            count: 0,
            sum: 0.0,
            min: f64::MAX,
            max: f64::MIN,
        }
    }

    fn record(&mut self, value_ms: f64) {
        let micros = (value_ms * 1000.0).max(1.0) as u64;
        let _ = self.histogram.record(micros);
        self.count += 1;
        self.sum += value_ms;
        if value_ms < self.min {
            self.min = value_ms;
        }
        if value_ms > self.max {
            self.max = value_ms;
        }
    }

    fn stats(&self) -> TrendStats {
        if self.count == 0 {
            return TrendStats {
                avg: 0.0,
                min: 0.0,
                med: 0.0,
                max: 0.0,
                p90: 0.0,
                p95: 0.0,
                count: 0,
            };
        }
        TrendStats {
            avg: self.sum / self.count as f64,
            min: self.min,
            med: self.histogram.value_at_quantile(0.5) as f64 / 1000.0,
            max: self.max,
            p90: self.histogram.value_at_quantile(0.9) as f64 / 1000.0,
            p95: self.histogram.value_at_quantile(0.95) as f64 / 1000.0,
            count: self.count,
        }
    }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // --- Tagged metric helpers ---

    /// Record a trend value with tags. Records both the base metric and tagged sub-metrics.
    pub fn trend_add_tagged(&self, name: &str, value_ms: f64, tags: &[(String, String)]) {
        self.trend_add(name, value_ms);
        for (k, v) in tags {
            let tagged_name = format!("{name}{{{k}:{v}}}");
            self.trend_add(&tagged_name, value_ms);
        }
    }

    /// Record a rate value with tags.
    pub fn rate_add_tagged(&self, name: &str, passed: bool, tags: &[(String, String)]) {
        self.rate_add(name, passed);
        for (k, v) in tags {
            let tagged_name = format!("{name}{{{k}:{v}}}");
            self.rate_add(&tagged_name, passed);
        }
    }

    /// Record a counter value with tags.
    pub fn counter_add_tagged(&self, name: &str, value: u64, tags: &[(String, String)]) {
        self.counter_add(name, value);
        for (k, v) in tags {
            let tagged_name = format!("{name}{{{k}:{v}}}");
            self.counter_add(&tagged_name, value);
        }
    }

    // --- Counter operations ---

    pub fn counter_add(&self, name: &str, value: u64) {
        let mut counters = self.counters.lock().unwrap();
        counters
            .entry(name.to_string())
            .or_insert_with(CounterMetric::new)
            .value
            .fetch_add(value, Ordering::Relaxed);
    }

    pub fn counter_get(&self, name: &str) -> u64 {
        let counters = self.counters.lock().unwrap();
        counters
            .get(name)
            .map(|c| c.value.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    // --- Gauge operations ---

    pub fn gauge_set(&self, name: &str, value: f64) {
        let mut gauges = self.gauges.lock().unwrap();
        let gauge = gauges
            .entry(name.to_string())
            .or_insert_with(GaugeMetric::new);

        let bits = value.to_bits();
        gauge.value.store(bits, Ordering::Relaxed);
        gauge.min.fetch_min(bits, Ordering::Relaxed);
        gauge.max.fetch_max(bits, Ordering::Relaxed);
    }

    pub fn gauge_get(&self, name: &str) -> f64 {
        let gauges = self.gauges.lock().unwrap();
        gauges
            .get(name)
            .map(|g| f64::from_bits(g.value.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    // --- Rate operations ---

    pub fn rate_add(&self, name: &str, passed: bool) {
        let mut rates = self.rates.lock().unwrap();
        let rate = rates
            .entry(name.to_string())
            .or_insert_with(RateMetric::new);

        rate.total.fetch_add(1, Ordering::Relaxed);
        if passed {
            rate.passes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn rate_get(&self, name: &str) -> (f64, u64, u64) {
        let rates = self.rates.lock().unwrap();
        rates
            .get(name)
            .map(|r| {
                let passes = r.passes.load(Ordering::Relaxed);
                let total = r.total.load(Ordering::Relaxed);
                let rate = if total > 0 {
                    passes as f64 / total as f64
                } else {
                    0.0
                };
                (rate, passes, total)
            })
            .unwrap_or((0.0, 0, 0))
    }

    // --- Trend operations ---

    pub fn trend_add(&self, name: &str, value_ms: f64) {
        let mut trends = self.trends.lock().unwrap();
        trends
            .entry(name.to_string())
            .or_insert_with(TrendMetric::new)
            .record(value_ms);
    }

    pub fn trend_stats(&self, name: &str) -> Option<TrendStats> {
        let trends = self.trends.lock().unwrap();
        trends.get(name).map(|t| t.stats())
    }

    // --- Snapshot for summary output ---

    pub fn snapshot(&self, duration_secs: f64) -> MetricsSnapshot {
        let counters = self.counters.lock().unwrap();
        let gauges = self.gauges.lock().unwrap();
        let rates = self.rates.lock().unwrap();
        let trends = self.trends.lock().unwrap();

        let mut counter_snap: Vec<_> = counters
            .iter()
            .map(|(name, c)| {
                let val = c.value.load(Ordering::Relaxed);
                let rate = if duration_secs > 0.0 {
                    val as f64 / duration_secs
                } else {
                    0.0
                };
                (name.clone(), val, rate)
            })
            .collect();
        counter_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut gauge_snap: Vec<_> = gauges
            .iter()
            .map(|(name, g)| {
                let val = f64::from_bits(g.value.load(Ordering::Relaxed));
                let min = f64::from_bits(g.min.load(Ordering::Relaxed));
                let max = f64::from_bits(g.max.load(Ordering::Relaxed));
                let min = if min == f64::from_bits(u64::MAX) { val } else { min };
                (name.clone(), val, min, max)
            })
            .collect();
        gauge_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rate_snap: Vec<_> = rates
            .iter()
            .map(|(name, r)| {
                let passes = r.passes.load(Ordering::Relaxed);
                let total = r.total.load(Ordering::Relaxed);
                let rate = if total > 0 {
                    passes as f64 / total as f64
                } else {
                    0.0
                };
                (name.clone(), rate, passes, total)
            })
            .collect();
        rate_snap.sort_by(|a, b| a.0.cmp(&b.0));

        let mut trend_snap: Vec<_> = trends
            .iter()
            .map(|(name, t)| (name.clone(), t.stats()))
            .collect();
        trend_snap.sort_by(|a, b| a.0.cmp(&b.0));

        MetricsSnapshot {
            counters: counter_snap,
            gauges: gauge_snap,
            rates: rate_snap,
            trends: trend_snap,
        }
    }
}

/// Convenience wrapper that holds an `Arc<MetricsRegistry>` and provides
/// methods matching the k6 built-in metric names.
#[derive(Clone)]
pub struct BuiltinMetrics {
    pub registry: Arc<MetricsRegistry>,
}

impl BuiltinMetrics {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(MetricsRegistry::new()),
        }
    }

    // --- Execution metrics ---

    pub fn set_vus(&self, count: u32) {
        self.registry.gauge_set("vus", count as f64);
    }

    pub fn set_vus_max(&self, count: u32) {
        self.registry.gauge_set("vus_max", count as f64);
    }

    pub fn record_iteration(&self, duration_ms: f64) {
        self.registry.counter_add("iterations", 1);
        self.registry.trend_add("iteration_duration", duration_ms);
    }

    pub fn record_dropped_iteration(&self) {
        self.registry.counter_add("dropped_iterations", 1);
    }

    // --- Check metrics ---

    pub fn record_check(&self, passed: bool) {
        self.registry.rate_add("checks", passed);
    }

    pub fn record_group_duration(&self, duration_ms: f64) {
        self.registry.trend_add("group_duration", duration_ms);
    }

    // --- HTTP metrics ---

    pub fn record_http_request(&self, timings: &crate::traits::Timings, failed: bool) {
        self.record_http_request_tagged(timings, failed, &[]);
    }

    pub fn record_http_request_tagged(
        &self,
        timings: &crate::traits::Timings,
        failed: bool,
        tags: &[(String, String)],
    ) {
        self.registry.counter_add_tagged("http_reqs", 1, tags);
        self.registry.rate_add_tagged("http_req_failed", !failed, tags);
        self.registry.trend_add_tagged("http_req_duration", timings.duration, tags);
        self.registry.trend_add_tagged("http_req_blocked", timings.blocked, tags);
        self.registry.trend_add_tagged("http_req_connecting", timings.connecting, tags);
        self.registry.trend_add_tagged("http_req_tls_handshaking", timings.tls_handshaking, tags);
        self.registry.trend_add_tagged("http_req_sending", timings.sending, tags);
        self.registry.trend_add_tagged("http_req_waiting", timings.waiting, tags);
        self.registry.trend_add_tagged("http_req_receiving", timings.receiving, tags);
    }

    // --- Network metrics ---

    pub fn record_data_sent(&self, bytes: u64) {
        self.registry.counter_add("data_sent", bytes);
    }

    pub fn record_data_received(&self, bytes: u64) {
        self.registry.counter_add("data_received", bytes);
    }

    // --- WebSocket metrics ---

    pub fn record_ws_session(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("ws_sessions", 1, tags);
        self.registry
            .trend_add_tagged("ws_session_duration", duration_ms, tags);
    }

    pub fn record_ws_connecting(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry
            .trend_add_tagged("ws_connecting", duration_ms, tags);
    }

    pub fn record_ws_msg_sent(&self, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("ws_msgs_sent", 1, tags);
    }

    pub fn record_ws_msg_received(&self, tags: &[(String, String)]) {
        self.registry
            .counter_add_tagged("ws_msgs_received", 1, tags);
    }

    pub fn record_ws_ping(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry
            .trend_add_tagged("ws_ping", duration_ms, tags);
    }

    // --- gRPC metrics ---

    pub fn record_grpc_request(&self, duration_ms: f64, tags: &[(String, String)]) {
        self.registry.counter_add_tagged("grpc_reqs", 1, tags);
        self.registry
            .trend_add_tagged("grpc_req_duration", duration_ms, tags);
    }
}

impl Default for BuiltinMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_basic() {
        let reg = MetricsRegistry::new();
        reg.counter_add("http_reqs", 1);
        reg.counter_add("http_reqs", 1);
        reg.counter_add("http_reqs", 5);
        assert_eq!(reg.counter_get("http_reqs"), 7);
    }

    #[test]
    fn gauge_basic() {
        let reg = MetricsRegistry::new();
        reg.gauge_set("vus", 10.0);
        assert!((reg.gauge_get("vus") - 10.0).abs() < 0.01);
        reg.gauge_set("vus", 5.0);
        assert!((reg.gauge_get("vus") - 5.0).abs() < 0.01);
    }

    #[test]
    fn rate_basic() {
        let reg = MetricsRegistry::new();
        reg.rate_add("checks", true);
        reg.rate_add("checks", true);
        reg.rate_add("checks", false);

        let (rate, passes, total) = reg.rate_get("checks");
        assert_eq!(passes, 2);
        assert_eq!(total, 3);
        assert!((rate - 0.6667).abs() < 0.01);
    }

    #[test]
    fn trend_basic() {
        let reg = MetricsRegistry::new();
        reg.trend_add("http_req_duration", 100.0);
        reg.trend_add("http_req_duration", 200.0);
        reg.trend_add("http_req_duration", 300.0);

        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.avg - 200.0).abs() < 0.01);
        assert!((stats.min - 100.0).abs() < 0.01);
        assert!((stats.max - 300.0).abs() < 0.01);
    }

    #[test]
    fn trend_percentiles() {
        let reg = MetricsRegistry::new();
        // Add 100 values from 1 to 100
        for i in 1..=100 {
            reg.trend_add("latency", i as f64);
        }

        let stats = reg.trend_stats("latency").unwrap();
        assert_eq!(stats.count, 100);
        assert!((stats.avg - 50.5).abs() < 0.1);
        assert!((stats.min - 1.0).abs() < 0.01);
        assert!((stats.max - 100.0).abs() < 0.01);
        // p50 ≈ 50, p90 ≈ 90, p95 ≈ 95
        assert!((stats.med - 50.0).abs() < 2.0);
        assert!((stats.p90 - 90.0).abs() < 2.0);
        assert!((stats.p95 - 95.0).abs() < 2.0);
    }

    #[test]
    fn snapshot_sorted() {
        let reg = MetricsRegistry::new();
        reg.counter_add("z_counter", 1);
        reg.counter_add("a_counter", 2);
        reg.trend_add("z_trend", 10.0);
        reg.trend_add("a_trend", 20.0);

        let snap = reg.snapshot(1.0);
        assert_eq!(snap.counters[0].0, "a_counter");
        assert_eq!(snap.counters[1].0, "z_counter");
        assert_eq!(snap.trends[0].0, "a_trend");
        assert_eq!(snap.trends[1].0, "z_trend");
    }

    #[test]
    fn builtin_metrics_http() {
        let m = BuiltinMetrics::new();
        let timings = crate::traits::Timings {
            duration: 150.0,
            waiting: 120.0,
            receiving: 25.0,
            sending: 5.0,
            ..Default::default()
        };

        m.record_http_request(&timings, false);
        m.record_http_request(&timings, false);
        m.record_http_request(&timings, true); // failed

        assert_eq!(m.registry.counter_get("http_reqs"), 3);

        let (fail_rate, _, _) = m.registry.rate_get("http_req_failed");
        // 2 passed (non-failed), 1 failed → rate_add(!failed) → 2/3 passes
        assert!((fail_rate - 0.6667).abs() < 0.01);

        let stats = m.registry.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.avg - 150.0).abs() < 0.01);
    }

    #[test]
    fn builtin_metrics_iterations() {
        let m = BuiltinMetrics::new();
        m.record_iteration(100.0);
        m.record_iteration(200.0);
        m.record_dropped_iteration();

        assert_eq!(m.registry.counter_get("iterations"), 2);
        assert_eq!(m.registry.counter_get("dropped_iterations"), 1);

        let stats = m.registry.trend_stats("iteration_duration").unwrap();
        assert_eq!(stats.count, 2);
        assert!((stats.avg - 150.0).abs() < 0.01);
    }

    #[test]
    fn concurrent_metric_access() {
        let reg = Arc::new(MetricsRegistry::new());
        let mut handles = vec![];

        for _ in 0..20 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    reg.counter_add("http_reqs", 1);
                    reg.trend_add("http_req_duration", i as f64);
                    reg.rate_add("checks", i % 2 == 0);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(reg.counter_get("http_reqs"), 2000);

        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 2000);

        let (_, passes, total) = reg.rate_get("checks");
        assert_eq!(total, 2000);
        assert_eq!(passes, 1000);
    }

    #[test]
    fn snapshot_with_rate() {
        let reg = MetricsRegistry::new();
        reg.counter_add("http_reqs", 100);

        let snap = reg.snapshot(10.0); // 10 seconds
        assert_eq!(snap.counters[0].1, 100); // value
        assert!((snap.counters[0].2 - 10.0).abs() < 0.01); // rate = 100/10
    }

    #[test]
    fn tagged_trend_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![
            ("scenario".to_string(), "light".to_string()),
            ("method".to_string(), "GET".to_string()),
        ];

        reg.trend_add_tagged("http_req_duration", 100.0, &tags);
        reg.trend_add_tagged("http_req_duration", 200.0, &tags);

        // Base metric has both
        let stats = reg.trend_stats("http_req_duration").unwrap();
        assert_eq!(stats.count, 2);

        // Tagged sub-metrics also have both
        let tagged = reg.trend_stats("http_req_duration{scenario:light}").unwrap();
        assert_eq!(tagged.count, 2);

        let method_tagged = reg.trend_stats("http_req_duration{method:GET}").unwrap();
        assert_eq!(method_tagged.count, 2);
    }

    #[test]
    fn tagged_counter_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![("scenario".to_string(), "heavy".to_string())];

        reg.counter_add_tagged("http_reqs", 1, &tags);
        reg.counter_add_tagged("http_reqs", 1, &tags);

        assert_eq!(reg.counter_get("http_reqs"), 2);
        assert_eq!(reg.counter_get("http_reqs{scenario:heavy}"), 2);
    }

    #[test]
    fn tagged_rate_records_both() {
        let reg = MetricsRegistry::new();
        let tags = vec![("scenario".to_string(), "api".to_string())];

        reg.rate_add_tagged("http_req_failed", true, &tags);
        reg.rate_add_tagged("http_req_failed", false, &tags);

        let (_, passes, total) = reg.rate_get("http_req_failed");
        assert_eq!(total, 2);

        let (_, tagged_passes, tagged_total) = reg.rate_get("http_req_failed{scenario:api}");
        assert_eq!(tagged_total, 2);
        assert_eq!(tagged_passes, 1);
    }
}
