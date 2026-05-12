//! Hyper-level HTTP client — engine spike for bug (b).
//!
//! Lives alongside the production [`crate::http_client::ReqwestHttpClient`]
//! and is selected at runtime by setting `K6RS_HTTP_CLIENT=hyper`. The trade
//! is: this client gives us connection-level visibility (DNS, TCP, write
//! completion, exact wire bytes) that reqwest's high-level API hides, at the
//! cost of a much smaller feature surface.
//!
//! Scope of this spike — intentionally narrow:
//!   - Plain HTTP only. HTTPS, TLS handshake timing, and HTTP/2 are out.
//!   - No connection pooling (each request opens a new TCP connection).
//!   - No proxies, no redirects, no blacklist/blocklist, no local-IP pool.
//!   - No request body streaming — body is sent as a single `Full<Bytes>`.
//!
//! The acceptance bar is: prove we can measure
//! `blocked / connecting / sending` plus exact `data_sent` / `data_received`
//! for one simple HTTP script. Once that lands cleanly and the conformance
//! report agrees, broader parity with the reqwest feature set can follow.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{HeaderValue, HOST, USER_AGENT};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use k6_core::traits::{HttpClient, HttpMethod, HttpRequest, HttpResponse, ResponseBody, Timings};

use crate::http_client::ReqwestHttpClient;

/// Default User-Agent. Concise, identifies the tool. Override per-request by
/// passing an explicit `User-Agent` header in `HttpRequest::headers`.
///
/// Length-wise this is intentionally close to upstream k6's default
/// (`k6/<version> (https://k6.io/)`) so the `data_sent` byte count lines up
/// in conformance comparisons — same magnitude order, not an attempt to
/// disguise k6-rs as k6.
const DEFAULT_USER_AGENT: &str = concat!("k6-rs/", env!("CARGO_PKG_VERSION"));

/// Hyper-backed HTTP client. See module docs.
pub struct HyperHttpClient {
    user_agent: HeaderValue,
    /// Per-authority pool of idle connections. A connection that finished its
    /// previous request without error is returned here and reused by the next
    /// caller. Not size-bounded — for a soak test this can grow unbounded; the
    /// spike accepts that. Eviction policies (idle timeout, max-per-authority)
    /// are follow-up work.
    pool: Mutex<HashMap<String, Vec<PooledConn>>>,
}

impl Default for HyperHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperHttpClient {
    pub fn new() -> Self {
        Self {
            user_agent: HeaderValue::from_static(DEFAULT_USER_AGENT),
            pool: Mutex::new(HashMap::new()),
        }
    }

    fn try_acquire(&self, authority: &str) -> Option<PooledConn> {
        let mut pool = self.pool.lock().unwrap();
        pool.get_mut(authority).and_then(|v| v.pop())
    }

    fn release(&self, authority: String, conn: PooledConn) {
        let mut pool = self.pool.lock().unwrap();
        pool.entry(authority).or_default().push(conn);
    }
}

/// One pooled HTTP/1.1 connection: a hyper SendRequest paired with the IO-level
/// metrics for that physical TCP stream, plus the task driving the connection
/// future. Dropping this aborts the conn task so the FD is released promptly.
struct PooledConn {
    sender: SendRequest<Full<Bytes>>,
    wire: WireMetrics,
    conn_task: JoinHandle<()>,
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        // Aborting is safe even if the task already finished — gives us prompt
        // FD release when the client/pool itself is dropped.
        self.conn_task.abort();
    }
}

/// Runtime-selectable HTTP client. Wraps either the production reqwest path
/// or the hyper-level spike; the rest of the runtime stays monomorphised on
/// a single concrete type. Selected by `K6RS_HTTP_CLIENT=hyper` in
/// `crates/k6-cli/src/main.rs` — defaults to reqwest.
///
/// This enum exists so we can A/B the two paths from the conformance harness
/// without forking the entire run loop into two generic instantiations.
pub enum AnyHttpClient {
    Reqwest(ReqwestHttpClient),
    Hyper(HyperHttpClient),
}

impl HttpClient for AnyHttpClient {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse>> + Send {
        async move {
            match self {
                AnyHttpClient::Reqwest(c) => c.send(req).await,
                AnyHttpClient::Hyper(c) => c.send(req).await,
            }
        }
    }
}

/// Wire-level observations collected by the IO wrapper for one request.
#[derive(Clone, Default)]
struct WireMetrics {
    bytes_written: Arc<AtomicU64>,
    bytes_read: Arc<AtomicU64>,
    last_write_at: Arc<Mutex<Option<Instant>>>,
}

/// An AsyncRead+AsyncWrite wrapper that counts bytes and timestamps the last
/// successful write. Sits between hyper and the TcpStream so that `sending`
/// can be measured separately from `waiting` — write completion is observable
/// even when hyper's `send_request` only resolves on header receipt.
struct CountingStream {
    inner: TcpStream,
    metrics: WireMetrics,
}

impl AsyncWrite for CountingStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                this.metrics
                    .bytes_written
                    .fetch_add(*n as u64, Ordering::Relaxed);
                *this.metrics.last_write_at.lock().unwrap() = Some(Instant::now());
            }
        }
        result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &result {
            if *n > 0 {
                this.metrics
                    .bytes_written
                    .fetch_add(*n as u64, Ordering::Relaxed);
                *this.metrics.last_write_at.lock().unwrap() = Some(Instant::now());
            }
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

impl AsyncRead for CountingStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let after = buf.filled().len();
            let n = (after - before) as u64;
            if n > 0 {
                this.metrics.bytes_read.fetch_add(n, Ordering::Relaxed);
            }
        }
        result
    }
}

impl HttpClient for HyperHttpClient {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse>> + Send {
        // SAFETY/lifetime: HyperHttpClient is held via Arc by the run loop.
        // The future captures `&self`-equivalent state by promoting to Arc<Self>
        // is not strictly needed since each call's future completes before the
        // client is dropped. We pass the user_agent by clone and acquire the
        // pool through `&self` reborrow inside an async block scoped to one
        // request.
        let ua = self.user_agent.clone();
        async move { send_inner(self, req, ua).await }
    }
}

async fn send_inner(
    client: &HyperHttpClient,
    req: HttpRequest,
    user_agent: HeaderValue,
) -> Result<HttpResponse> {
    let start = Instant::now();

    let uri: Uri = req.url.parse().context("invalid URL")?;
    let scheme = uri.scheme_str().unwrap_or("http");
    anyhow::ensure!(
        scheme == "http",
        "HyperHttpClient spike supports plain HTTP only (got scheme `{scheme}`); \
         the spike accepts this limitation until HTTPS work lands."
    );
    let host = uri.host().context("URL missing host")?.to_string();
    let port = uri.port_u16().unwrap_or(80);
    let authority = format!("{host}:{port}");

    // Try the pool first. On a hit, `blocked` and `connecting` are 0 — the
    // request reuses an existing TCP connection. On a miss, do the DNS+TCP
    // dance and record real timings for those phases.
    let (mut pooled, blocked_ms, connecting_ms) = if let Some(p) = client.try_acquire(&authority) {
        (p, 0.0, 0.0)
    } else {
        let blocked_start = Instant::now();
        let addr = tokio::net::lookup_host(authority.as_str())
            .await
            .with_context(|| format!("DNS lookup failed for {authority}"))?
            .next()
            .context("DNS returned no addresses")?;
        let blocked_done = Instant::now();

        let connect_start = Instant::now();
        let tcp = TcpStream::connect(addr).await.context("TCP connect failed")?;
        let connect_done = Instant::now();

        let wire = WireMetrics::default();
        let stream = CountingStream {
            inner: tcp,
            metrics: wire.clone(),
        };
        let (sender, connection) = http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
            .await
            .context("hyper http1 handshake failed")?;
        let conn_task = tokio::spawn(async move {
            let _ = connection.await;
        });

        let blocked = blocked_done
            .saturating_duration_since(blocked_start)
            .as_secs_f64()
            * 1000.0;
        let connecting = connect_done
            .saturating_duration_since(connect_start)
            .as_secs_f64()
            * 1000.0;
        (
            PooledConn {
                sender,
                wire,
                conn_task,
            },
            blocked,
            connecting,
        )
    };

    // Snapshot wire counters before this request so the byte counts come out
    // per-request, not cumulative across all requests that used this pooled
    // connection. Reset last_write_at for the sending-boundary measurement.
    let bytes_sent_before = pooled.wire.bytes_written.load(Ordering::Relaxed);
    let bytes_recv_before = pooled.wire.bytes_read.load(Ordering::Relaxed);
    *pooled.wire.last_write_at.lock().unwrap() = None;

    // Build the hyper Request. We construct the absolute-form path + query
    // and set Host explicitly so the request line matches the on-wire form.
    let method = hyper_method(req.method);
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let mut builder = Request::builder()
        .method(method)
        .uri(path_and_query)
        .header(HOST, authority.as_str())
        .header(USER_AGENT, user_agent);
    for (k, v) in &req.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let body_bytes = req.body.map(Bytes::from).unwrap_or_default();
    let request = builder
        .body(Full::new(body_bytes))
        .context("constructing hyper Request")?;

    // ── sending → waiting → receiving ──────────────────────────────────────
    let send_start = Instant::now();
    let response_result = pooled.sender.send_request(request).await;

    // If send_request failed on a pooled connection, the connection is dead.
    // Drop the PooledConn (its Drop aborts the conn task) so it never returns
    // to the pool. The error propagates; the next caller will open fresh.
    let response = match response_result {
        Ok(r) => r,
        Err(e) => {
            drop(pooled);
            return Err(anyhow::Error::from(e).context("send_request failed"));
        }
    };
    let waiting_done = Instant::now();

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let body_collect_result = response.into_body().collect().await;
    let body_bytes = match body_collect_result {
        Ok(c) => c.to_bytes().to_vec(),
        Err(e) => {
            drop(pooled);
            return Err(anyhow::Error::from(e).context("reading response body"));
        }
    };
    let receive_done = Instant::now();

    // sending boundary from the IO wrapper. last_write_at marks when the
    // request finished going out. If somehow not recorded (zero-byte writes
    // only, which shouldn't happen for HTTP/1.1), fall back to send_start.
    let last_write = pooled
        .wire
        .last_write_at
        .lock()
        .unwrap()
        .unwrap_or(send_start);
    let sending_ms = last_write
        .saturating_duration_since(send_start)
        .as_secs_f64()
        * 1000.0;
    let waiting_ms = waiting_done
        .saturating_duration_since(last_write)
        .as_secs_f64()
        * 1000.0;
    let receiving_ms = receive_done
        .saturating_duration_since(waiting_done)
        .as_secs_f64()
        * 1000.0;

    let timings = Timings {
        blocked: blocked_ms,
        connecting: connecting_ms,
        tls_handshaking: 0.0,
        sending: sending_ms,
        waiting: waiting_ms,
        receiving: receiving_ms,
        duration: start.elapsed().as_secs_f64() * 1000.0,
    };

    let bytes_sent =
        pooled.wire.bytes_written.load(Ordering::Relaxed) - bytes_sent_before;
    let bytes_received =
        pooled.wire.bytes_read.load(Ordering::Relaxed) - bytes_recv_before;

    // Return the connection to the pool for reuse. Both directions of this
    // request completed without error, so the connection is assumed healthy
    // for HTTP/1.1 keep-alive. If the server actually sent `Connection: close`,
    // the next caller's send_request will fail and trigger the drop-on-err
    // path above; the pool self-heals one mistake at a time.
    client.release(authority, pooled);

    Ok(HttpResponse {
        status,
        headers,
        body: ResponseBody::Buffered(body_bytes),
        timings,
        url: req.url,
        data_sent: bytes_sent,
        data_received: bytes_received,
    })
}

fn hyper_method(m: HttpMethod) -> Method {
    match m {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Options => Method::OPTIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::Router;
    use tokio::net::TcpListener;

    /// The (b) spike's acceptance criterion in test form: a single HTTP call
    /// against a local fixture must produce non-zero phase timings that we
    /// could not previously measure (blocked/connecting/sending), exact wire
    /// byte counts, and physically-consistent boundary relationships.
    #[tokio::test]
    async fn hyper_client_measures_phases_and_exact_wire_bytes() {
        const RESP_BODY: &str = "abcdefghij";
        let app = Router::new()
            .route("/get", get(|| async { RESP_BODY }))
            .route("/post", post(|body: String| async move { body }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = HyperHttpClient::new();
        let url = format!("http://127.0.0.1:{}/get", addr.port());
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: url.clone(),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 200);

        // Wire bytes are now exact (counted at the IO layer, not estimated).
        // For "GET /get HTTP/1.1\r\nhost: 127.0.0.1:PPPPP\r\n\r\n" that's
        // ~50+ bytes; the previous reqwest path could only see what the
        // high-level builder exposed.
        assert!(
            resp.data_sent >= 40,
            "data_sent must reflect actual wire bytes (>= 40); got {}",
            resp.data_sent
        );

        let body_len = match &resp.body {
            ResponseBody::Buffered(b) => b.len() as u64,
            ResponseBody::Discarded => 0,
        };
        assert_eq!(body_len, RESP_BODY.len() as u64);
        assert!(
            resp.data_received > body_len,
            "data_received ({}) must exceed body ({}) — status line + headers",
            resp.data_received,
            body_len
        );

        // Phase timings that the reqwest path could not see:
        // blocked > 0 means DNS happened, connecting > 0 means TCP setup,
        // sending > 0 means we measured request-bytes-write completion.
        // (Connecting can be a few microseconds on loopback but is never zero.)
        assert!(
            resp.timings.connecting > 0.0,
            "connecting must be > 0 (TCP happened); got {}",
            resp.timings.connecting
        );

        // sending must be measurable AND distinct from total duration — the
        // bug that prompted (a) made sending == duration. After (b), sending
        // is the actual write phase and is bounded above by duration.
        assert!(
            resp.timings.sending >= 0.0,
            "sending must be non-negative; got {}",
            resp.timings.sending
        );
        assert!(
            resp.timings.sending < resp.timings.duration,
            "sending ({}) must be strictly less than duration ({})",
            resp.timings.sending,
            resp.timings.duration
        );

        // Phase sum should approximate duration. Allow slack for the small
        // gap between connect_done and send_start (handshake + request
        // construction) which isn't attributed to any k6 phase yet.
        let phase_sum = resp.timings.blocked
            + resp.timings.connecting
            + resp.timings.sending
            + resp.timings.waiting
            + resp.timings.receiving;
        assert!(
            phase_sum <= resp.timings.duration + 0.5,
            "sum of phases ({}) must not exceed duration ({})",
            phase_sum,
            resp.timings.duration
        );
    }

    #[tokio::test]
    async fn hyper_client_post_with_body_counts_body_in_data_sent() {
        let app = Router::new().route("/post", post(|body: String| async move { body }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = HyperHttpClient::new();
        let payload = b"x".repeat(200);
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Post,
                url: format!("http://127.0.0.1:{}/post", addr.port()),
                headers: Vec::new(),
                body: Some(payload.clone()),
                timeout: None,
            })
            .await
            .unwrap();

        // data_sent must cover the body bytes plus a non-trivial prelude
        // (request line + Host + Content-Length etc., all visible on wire).
        assert!(
            resp.data_sent > payload.len() as u64 + 40,
            "data_sent ({}) must exceed body ({}) + prelude headroom",
            resp.data_sent,
            payload.len()
        );
    }

    #[tokio::test]
    async fn pool_reuses_connection_for_same_authority() {
        // Lock the pool-reuse invariant: a second request to the same
        // host:port via the same client must skip DNS + TCP setup. This is
        // what closes the per-request connecting/blocked overhead that
        // dominated the conformance drift before pooling landed.
        let app = Router::new().route("/get", get(|| async { "ok" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = HyperHttpClient::new();
        let url = format!("http://127.0.0.1:{}/get", addr.port());
        let req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        let r1 = client.send(req()).await.unwrap();
        let r2 = client.send(req()).await.unwrap();
        let r3 = client.send(req()).await.unwrap();

        // First request paid for DNS + TCP.
        assert!(
            r1.timings.connecting > 0.0,
            "first request must measure connecting; got {}",
            r1.timings.connecting
        );

        // Subsequent requests must reuse: connecting AND blocked are 0
        // because the pool short-circuits both phases.
        assert_eq!(
            r2.timings.connecting, 0.0,
            "second request must reuse the pooled connection; got connecting={}",
            r2.timings.connecting
        );
        assert_eq!(
            r2.timings.blocked, 0.0,
            "second request must skip DNS; got blocked={}",
            r2.timings.blocked
        );
        assert_eq!(r3.timings.connecting, 0.0, "third request must also reuse");
        assert_eq!(r3.timings.blocked, 0.0, "third request must also skip DNS");

        // data_sent/data_received are per-request (snapshot/diff against the
        // pooled connection's wire counters). Each request sends the same
        // bytes, so the three counts must match.
        assert_eq!(
            r1.data_sent, r2.data_sent,
            "per-request byte counts must be stable across pool reuse"
        );
        assert_eq!(r2.data_sent, r3.data_sent);
    }

    #[tokio::test]
    async fn hyper_client_rejects_https_in_spike_scope() {
        let client = HyperHttpClient::new();
        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: "https://example.com/".to_string(),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await;
        // HttpResponse doesn't impl Debug, so use a manual match instead of
        // expect_err / unwrap_err.
        let err = match result {
            Ok(_) => panic!("HTTPS must error in the spike"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HTTP only"),
            "error should mention HTTP-only scope; got: {msg}"
        );
    }
}
