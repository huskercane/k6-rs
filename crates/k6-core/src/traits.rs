use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

/// Result of a single VU iteration.
pub struct IterationResult {
    pub duration: Duration,
}

/// A virtual user that can execute script iterations.
///
/// Each VU owns its own JS context and HTTP client state.
/// Implementations must be `Send` to move between threads.
pub trait VirtualUser: Send {
    /// Run one iteration of the test script.
    fn run_iteration(&mut self) -> Result<IterationResult>;

    /// Reset VU state between iterations (e.g., clear per-iteration data).
    /// Called automatically when the VU is returned to the pool.
    fn reset(&mut self);
}

/// Summary returned when an executor finishes.
pub struct RunSummary {
    pub iterations_completed: u64,
    pub iterations_dropped: u64,
    pub duration: Duration,
}

/// An executor that schedules VU iterations according to a strategy.
///
/// Different executor types (constant-vus, constant-arrival-rate, etc.)
/// implement this trait. Executors are generic over the VU type —
/// they don't know about JS or HTTP.
pub trait Executor: Send + Sync {
    /// Run the executor until completion or cancellation.
    fn run(
        &self,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<RunSummary>> + Send;
}

/// Abstraction over HTTP clients for testability.
///
/// Production uses reqwest; tests use a mock that returns canned responses.
pub trait HttpClient: Send + Sync {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse>> + Send;
}

/// An HTTP request to be sent.
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
}

pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

/// Timing breakdown for an HTTP request (all in milliseconds).
#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub blocked: f64,
    pub connecting: f64,
    pub tls_handshaking: f64,
    pub sending: f64,
    pub waiting: f64,
    pub receiving: f64,
    pub duration: f64,
}

/// An HTTP response returned to the JS layer.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
    pub timings: Timings,
    pub url: String,
}

/// Response body with memory controls.
pub enum ResponseBody {
    /// Body was read and buffered (up to size cap).
    Buffered(Vec<u8>),
    /// Body was drained without storing (discardResponseBodies=true).
    Discarded,
}

/// Tags attached to metrics samples.
pub type Tags = Vec<(String, String)>;

/// Collects metrics from VU execution.
///
/// Implementations can aggregate in-memory (for summary output)
/// or stream to external systems (JSON, InfluxDB, etc.).
pub trait MetricsCollector: Send + Sync {
    fn record_http(&self, timings: &Timings, tags: &Tags);
    fn record_check(&self, passed: bool, name: &str, tags: &Tags);
    fn record_iteration(&self, duration: Duration, tags: &Tags);
    fn record_dropped(&self);
    fn record_data_sent(&self, bytes: u64, tags: &Tags);
    fn record_data_received(&self, bytes: u64, tags: &Tags);
}
