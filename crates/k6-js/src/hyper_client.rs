//! Hyper-level HTTP client.
//!
//! Lives alongside the production [`crate::http_client::ReqwestHttpClient`]
//! and is selected at runtime by setting `K6RS_HTTP_CLIENT=hyper`. The trade
//! is: this client gives us connection-level visibility (DNS, TCP, write
//! completion, exact wire bytes) that reqwest's high-level API hides, at the
//! cost of a smaller feature surface today.
//!
//! Originally landed as the (b) spike for bug (b) — phase timing
//! instrumentation. As of S0 it has graduated into the "promote hyper toward
//! default" track: [`HyperHttpClient::from_config`] consumes the same
//! [`TestConfig`] surface that `ReqwestHttpClient::from_config` does.
//! Feature parity is being closed slice by slice (S0..S12 in the plan); fields
//! present on [`HyperConfig`] but not yet read are marked with the slice that
//! will consume them, so subsequent slices are purely additive.
//!
//! Scope at S0+S1+Phase 1:
//!   - Plain HTTP only. HTTPS, TLS handshake timing, and HTTP/2 are out (S7).
//!   - Pooling by [`RouteKey`] — same as the spike's per-authority pool, but
//!     keyed on a type whose shape is frozen now (transport + origin host +
//!     port + source-IP bind + proxy route) so HTTPS/local_ips/proxy slices
//!     don't have to mutate pool identity.
//!   - Request timeout, `no_connection_reuse`, user-agent override, and
//!     `http_debug` are wired.
//!   - `blockHostnames`, `blacklistIPs`, and static `hosts` mappings are
//!     wired for plain HTTP.
//!   - No redirects (S8), no local-IP source-bind (S9), no proxy (S10).
//!   - Body reader uses a frame loop with cap-then-drain semantics: buffer
//!     truncates at [`HyperConfig::max_response_body_size`] but the remainder
//!     of the response is ALWAYS drained to end-of-stream before the pooled
//!     conn is released, so a capped or discarded body never poisons the
//!     pool. Drain failures evict the conn rather than re-pooling it.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{HOST, HeaderValue, USER_AGENT};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use k6_core::config::TestConfig;
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

/// Default per-request timeout, matching reqwest's `from_config` default.
/// Applied around the full send path: DNS, TCP connect, request write, response
/// headers, and body drain.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default response-body buffer cap, matching reqwest's hardcoded 10 MiB.
/// Beyond this, the response body is drained but not retained.
const DEFAULT_MAX_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Identity of a reusable connection.
///
/// Keying the pool on this struct (rather than a raw `"host:port"` string)
/// means subsequent slices can extend connection identity without overloading
/// existing fields' semantics:
///   - S7 will add `Transport::Https`.
///   - S5 will write the **resolved** host into `target_host` after the
///     static `hosts` override resolves.
///   - S9 will populate `source_ip` when `localIPs` round-robin binds a
///     specific outbound IP.
///   - S10 will populate `proxy` for proxied routes.
///
/// `target_host` is the **origin authority** — the URL's host after any
/// `hosts` static override, but BEFORE any HTTP proxy rewrite. With
/// `proxy: Some(...)` the socket actually opens against the proxy and the
/// origin authority still matters for CONNECT and Host/SNI behavior;
/// [`ProxyRoute`] will carry the proxy's identity separately when S10 lands.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(crate) struct RouteKey {
    pub(crate) transport: Transport,
    pub(crate) target_host: String,
    pub(crate) target_port: u16,
    pub(crate) source_ip: Option<IpAddr>,
    pub(crate) proxy: Option<ProxyRoute>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub(crate) enum Transport {
    Http,
    // S7 adds Https.
}

/// Stub today — exists so [`RouteKey`]'s shape is frozen for S5..S10. S10
/// will populate this with the proxy socket destination and any required
/// auth identity.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(crate) struct ProxyRoute {
    _unused: (),
}

enum ConnectTarget {
    Host(String),
    Ip(SocketAddr),
}

/// Snapshot of every HTTP-shaping knob the hyper client cares about, captured
/// at [`HyperHttpClient::from_config`] time.
///
/// Fields actively read today: `discard_response_bodies`,
/// `max_response_body_size`, `request_timeout`, `no_connection_reuse`,
/// `user_agent_override`, `blacklist_ips`, `block_hostnames`, `hosts`, and
/// `http_debug`. Other fields are stored so later slices' edits are pure
/// additions; each is annotated with the slice that will consume it.
#[derive(Clone)]
#[allow(dead_code)] // Future-phase fields stored intentionally; see comments.
struct HyperConfig {
    // S1 — body buffer policy.
    discard_response_bodies: bool,
    max_response_body_size: usize,

    // Phase 1 — per-request timeout wrapper.
    request_timeout: Duration,

    // Phase 1 — when true, bypass pool acquire/release entirely.
    no_connection_reuse: bool,

    // Phase 1 — replaces the hardcoded default user-agent if set.
    user_agent_override: Option<HeaderValue>,

    // Phase 2 — pre-send filters and static DNS override.
    blacklist_ips: Vec<ipnet::IpNet>,
    block_hostnames: Vec<String>,
    hosts: HashMap<String, String>,

    // Phase 1 — request/response logging to stderr.
    http_debug: Option<String>,
    // S7 fields (insecure_skip_tls_verify, tls_version min/max) enter
    // HyperConfig when HTTPS lands.
}

/// Hyper-backed HTTP client. See module docs.
pub struct HyperHttpClient {
    config: HyperConfig,
    user_agent: HeaderValue,
    /// Per-[`RouteKey`] pool of idle connections. A connection whose previous
    /// request **and** subsequent body drain both completed without error is
    /// returned here and reused by the next caller. Not size-bounded —
    /// per-authority caps + idle-timeout eviction are S3 follow-up work.
    pool: Mutex<HashMap<RouteKey, Vec<PooledConn>>>,
}

impl Default for HyperHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperHttpClient {
    /// Convenience constructor. Equivalent to
    /// `from_config(&TestConfig::default()).expect(...)` — `TestConfig`'s
    /// defaults never produce a build failure, so the unwrap is total.
    /// Kept primarily for test ergonomics and as a back-compat shim for the
    /// pre-S0 spike callers.
    pub fn new() -> Self {
        Self::from_config(&TestConfig::default())
            .expect("default TestConfig must always build a HyperHttpClient")
    }

    /// Build a hyper client from the parsed test configuration.
    ///
    /// Stores every field S1..S6 will consume. S0 only reads the body-buffer
    /// fields; the rest are stored so each subsequent slice's diff is a pure
    /// addition (locks the API surface up front).
    pub fn from_config(config: &TestConfig) -> Result<Self> {
        let blacklist_ips: Vec<ipnet::IpNet> = config
            .blacklist_ips
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        let user_agent_override = match &config.user_agent {
            Some(ua) => Some(
                HeaderValue::from_str(ua).with_context(|| format!("invalid userAgent {ua:?}"))?,
            ),
            None => None,
        };

        let hcfg = HyperConfig {
            discard_response_bodies: config.discard_response_bodies,
            max_response_body_size: DEFAULT_MAX_RESPONSE_BODY_SIZE,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            no_connection_reuse: config.no_connection_reuse,
            user_agent_override,
            blacklist_ips,
            block_hostnames: config.block_hostnames.clone(),
            hosts: config.hosts.clone(),
            http_debug: config.http_debug.clone(),
        };

        let user_agent = hcfg
            .user_agent_override
            .clone()
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_USER_AGENT));

        Ok(Self {
            config: hcfg,
            user_agent,
            pool: Mutex::new(HashMap::new()),
        })
    }

    fn try_acquire(&self, key: &RouteKey) -> Option<PooledConn> {
        let mut pool = self.pool.lock().unwrap();
        pool.get_mut(key).and_then(|v| v.pop())
    }

    fn release(&self, key: RouteKey, conn: PooledConn) {
        let mut pool = self.pool.lock().unwrap();
        pool.entry(key).or_default().push(conn);
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

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
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
        let request_timeout = req.timeout.unwrap_or(self.config.request_timeout);
        async move {
            match tokio::time::timeout(request_timeout, send_inner(self, req, ua)).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "request timed out after {:?}",
                    request_timeout
                )),
            }
        }
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
        "HyperHttpClient supports plain HTTP only (got scheme `{scheme}`); \
         HTTPS support lands in S7."
    );
    let host = uri.host().context("URL missing host")?.to_string();
    let port = uri.port_u16().unwrap_or(80);
    let origin_authority = format!("{host}:{port}");

    check_blocked_hostname(&client.config, &host)?;
    check_literal_ip_blacklist(&client.config, &host)?;

    let (target_host, connect_target) = resolve_connect_target(&client.config, &host, port)?;

    // RouteKey identifies the pool slot for this connection. In S0 every axis
    // except `transport`/`target_host`/`target_port` is fixed (no local-IP
    // bind, no proxy, plain HTTP). The shape is frozen so S5/S7/S9/S10 each
    // populate their own axis without overloading existing semantics — see
    // the type's doc-comment.
    let route_key = RouteKey {
        transport: Transport::Http,
        target_host: target_host.clone(),
        target_port: port,
        source_ip: None,
        proxy: None,
    };

    // Try the pool first. On a hit, `blocked` and `connecting` are 0 — the
    // request reuses an existing TCP connection. On a miss, do the DNS+TCP
    // dance and record real timings for those phases.
    let (mut pooled, blocked_ms, connecting_ms) = if !client.config.no_connection_reuse {
        if let Some(p) = client.try_acquire(&route_key) {
            (p, 0.0, 0.0)
        } else {
            open_connection(&connect_target, &client.config).await?
        }
    } else {
        open_connection(&connect_target, &client.config).await?
    };

    // Snapshot wire counters before this request so the byte counts come out
    // per-request, not cumulative across all requests that used this pooled
    // connection. Reset last_write_at for the sending-boundary measurement.
    let bytes_sent_before = pooled.wire.bytes_written.load(Ordering::Relaxed);
    let bytes_recv_before = pooled.wire.bytes_read.load(Ordering::Relaxed);
    *pooled.wire.last_write_at.lock().unwrap() = None;

    // Build the hyper Request. We construct the absolute-form path + query
    // and set Host explicitly so the request line matches the on-wire form.
    let method = hyper_method(&req.method);
    debug_request(&client.config, method.as_str(), &req.url, &req.headers);
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let has_user_agent = req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
    let mut builder = Request::builder()
        .method(method)
        .uri(path_and_query)
        .header(HOST, origin_authority.as_str());
    if !has_user_agent {
        builder = builder.header(USER_AGENT, user_agent);
    }
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
    debug_response(&client.config, status, &req.url, &headers);

    // Read the response body with the cap + drain rule. CRUCIAL: even when
    // we are not buffering bytes (discard mode, or post-cap in buffered mode)
    // the body is iterated to end-of-stream. Stopping early would leave
    // pending bytes in hyper's internal buffer; the next request reusing
    // this pooled connection would either see them as part of its response
    // or hit a protocol error. Drain failures evict the conn.
    let response_body = match read_body_with_cap_and_drain(
        response.into_body(),
        client.config.discard_response_bodies,
        client.config.max_response_body_size,
    )
    .await
    {
        Ok(rb) => rb,
        Err(e) => {
            drop(pooled);
            return Err(e.context("reading response body"));
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

    let bytes_sent = pooled.wire.bytes_written.load(Ordering::Relaxed) - bytes_sent_before;
    let bytes_received = pooled.wire.bytes_read.load(Ordering::Relaxed) - bytes_recv_before;

    // Return the connection to the pool for reuse. Send + drain both
    // completed without error, so the connection is assumed healthy for
    // HTTP/1.1 keep-alive. If the server actually sent `Connection: close`,
    // the next caller's send_request will fail and trigger the drop-on-err
    // path above; the pool self-heals one mistake at a time.
    if client.config.no_connection_reuse {
        drop(pooled);
    } else {
        client.release(route_key, pooled);
    }

    Ok(HttpResponse {
        status,
        headers,
        body: response_body,
        timings,
        url: req.url,
        data_sent: bytes_sent,
        data_received: bytes_received,
    })
}

fn resolve_connect_target(
    config: &HyperConfig,
    host: &str,
    port: u16,
) -> Result<(String, ConnectTarget)> {
    if let Some(mapped) = config.hosts.get(host) {
        if let Ok(ip) = mapped.parse::<IpAddr>() {
            check_ip_blacklist(config, ip)?;
            return Ok((ip.to_string(), ConnectTarget::Ip(SocketAddr::new(ip, port))));
        }
    }

    Ok((
        host.to_string(),
        ConnectTarget::Host(format!("{host}:{port}")),
    ))
}

fn check_blocked_hostname(config: &HyperConfig, host: &str) -> Result<()> {
    for pattern in &config.block_hostnames {
        if hostname_matches(host, pattern) {
            anyhow::bail!("hostname {host} is blocked by blockHostnames");
        }
    }
    Ok(())
}

fn check_literal_ip_blacklist(config: &HyperConfig, host: &str) -> Result<()> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        check_ip_blacklist(config, ip)?;
    }
    Ok(())
}

fn check_ip_blacklist(config: &HyperConfig, ip: IpAddr) -> Result<()> {
    for net in &config.blacklist_ips {
        if net.contains(&ip) {
            anyhow::bail!("IP {ip} is blocked by blacklistIPs");
        }
    }
    Ok(())
}

fn hostname_matches(host: &str, pattern: &str) -> bool {
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..];
        host.ends_with(suffix) && host.len() > suffix.len()
    } else {
        host == pattern
    }
}

async fn open_connection(
    target: &ConnectTarget,
    config: &HyperConfig,
) -> Result<(PooledConn, f64, f64)> {
    let blocked_start = Instant::now();
    let addr = match target {
        ConnectTarget::Ip(addr) => *addr,
        ConnectTarget::Host(authority) => {
            let addr = tokio::net::lookup_host(authority)
                .await
                .with_context(|| format!("DNS lookup failed for {authority}"))?
                .next()
                .context("DNS returned no addresses")?;
            check_ip_blacklist(config, addr.ip())?;
            addr
        }
    };
    let blocked_done = Instant::now();

    let connect_start = Instant::now();
    let tcp = TcpStream::connect(addr)
        .await
        .context("TCP connect failed")?;
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

    Ok((
        PooledConn {
            sender,
            wire,
            conn_task,
        },
        blocked,
        connecting,
    ))
}

fn debug_request(config: &HyperConfig, method: &str, url: &str, headers: &[(String, String)]) {
    if let Some(ref mode) = config.http_debug {
        eprintln!("HTTP DEBUG > {method} {url}");
        if mode == "full" {
            for (k, v) in headers {
                eprintln!("HTTP DEBUG >   {k}: {v}");
            }
        }
    }
}

fn debug_response(config: &HyperConfig, status: u16, url: &str, headers: &[(String, String)]) {
    if let Some(ref mode) = config.http_debug {
        eprintln!("HTTP DEBUG < {status} {url}");
        if mode == "full" {
            for (k, v) in headers {
                eprintln!("HTTP DEBUG <   {k}: {v}");
            }
        }
    }
}

/// Read the response body to end-of-stream while applying the cap-buffer or
/// discard rule.
///
/// **Invariant:** the body is always iterated to end-of-stream before this
/// function returns `Ok(_)`. Stopping early when the buffer hits the cap (or
/// in discard mode) would leave bytes pending in hyper's internal channel;
/// the pooled connection would then be returned in a dirty state and the
/// next request reusing it would either see those leftover bytes as part of
/// its response or hit a protocol error mid-handshake.
///
/// `data_received` byte accounting is handled separately by the IO-level
/// [`CountingStream`] wrapper — every byte read from the socket is counted
/// regardless of whether this loop retains it. The role of this function is
/// purely buffer-policy + drain.
async fn read_body_with_cap_and_drain(
    mut body: hyper::body::Incoming,
    discard: bool,
    cap: usize,
) -> Result<ResponseBody> {
    if discard {
        while let Some(frame_result) = body.frame().await {
            // Any error mid-drain poisons the connection. Propagating Err
            // here causes the caller to drop the PooledConn (the Drop impl
            // aborts the conn task) so the dirty conn never re-enters the
            // pool.
            let _frame = frame_result.context("draining response body (discard mode)")?;
        }
        return Ok(ResponseBody::Discarded);
    }

    let mut buffer: Vec<u8> = Vec::new();
    while let Some(frame_result) = body.frame().await {
        let frame = frame_result.context("reading response body frame")?;
        if let Some(data) = frame.data_ref() {
            if buffer.len() < cap {
                let remaining = cap - buffer.len();
                let take = remaining.min(data.len());
                buffer.extend_from_slice(&data[..take]);
            }
            // Frames past the cap are intentionally discarded but the loop
            // continues so the body drains to completion. Bytes still flow
            // through CountingStream and contribute to data_received.
        }
    }
    Ok(ResponseBody::Buffered(buffer))
}

fn hyper_method(m: &HttpMethod) -> Method {
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
    use axum::Router;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use tokio::net::TcpListener;

    fn buffered_string(resp: &HttpResponse) -> String {
        match &resp.body {
            ResponseBody::Buffered(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            ResponseBody::Discarded => panic!("buffered response expected"),
        }
    }

    async fn start_app(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        format!("http://127.0.0.1:{}", addr.port())
    }

    #[tokio::test]
    async fn reqwest_and_hyper_basic_http_match_status_body_and_url() {
        let base_url = start_app(Router::new().route("/get", get(|| async { "parity-ok" }))).await;
        let url = format!("{base_url}/get");

        let make_req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        let reqwest = ReqwestHttpClient::new(false).unwrap();
        let hyper = HyperHttpClient::new();
        let reqwest_resp = reqwest.send(make_req()).await.unwrap();
        let hyper_resp = hyper.send(make_req()).await.unwrap();

        assert_eq!(reqwest_resp.status, hyper_resp.status);
        assert_eq!(buffered_string(&reqwest_resp), buffered_string(&hyper_resp));
        assert_eq!(reqwest_resp.url, hyper_resp.url);
    }

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

    // ── S0+S1 regression tests ─────────────────────────────────────────────

    /// S0 — from_config must carry the body-policy fields and the four config
    /// fields S5/S6 will consume. Locks the API surface before subsequent
    /// slices add behavior to it.
    #[test]
    fn from_config_persists_discard_cap_and_route_fields() {
        let mut config = TestConfig::default();
        config.discard_response_bodies = true;
        config.user_agent = Some("custom-ua/1.0".to_string());
        config.no_connection_reuse = true;
        config.block_hostnames = vec!["*.internal".to_string()];
        config.blacklist_ips = vec!["10.0.0.0/8".to_string()];
        config
            .hosts
            .insert("svc.local".to_string(), "127.0.0.1".to_string());
        config.http_debug = Some("full".to_string());

        let client = HyperHttpClient::from_config(&config).unwrap();
        // S1 fields actively read in S0+S1
        assert!(client.config.discard_response_bodies);
        assert_eq!(
            client.config.max_response_body_size,
            DEFAULT_MAX_RESPONSE_BODY_SIZE
        );
        // Stored-but-not-yet-read fields (S2..S6) must round-trip so the
        // slices that come later are pure additions.
        assert_eq!(client.config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert!(client.config.no_connection_reuse);
        assert_eq!(
            client
                .config
                .user_agent_override
                .as_ref()
                .unwrap()
                .to_str()
                .unwrap(),
            "custom-ua/1.0"
        );
        assert_eq!(client.config.blacklist_ips.len(), 1);
        assert_eq!(
            client.config.block_hostnames,
            vec!["*.internal".to_string()]
        );
        assert_eq!(client.config.hosts.get("svc.local").unwrap(), "127.0.0.1");
        assert_eq!(client.config.http_debug.as_deref(), Some("full"));
    }

    #[tokio::test]
    async fn request_timeout_wraps_full_send_path() {
        let base_url = start_app(Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                "late"
            }),
        ))
        .await;

        let client = HyperHttpClient::new();
        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/slow"),
                headers: Vec::new(),
                body: None,
                timeout: Some(Duration::from_millis(20)),
            })
            .await;

        let err = match result {
            Ok(_) => panic!("slow request must time out"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("timed out"),
            "timeout error should be explicit; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn no_connection_reuse_bypasses_pool() {
        let base_url = start_app(Router::new().route("/get", get(|| async { "ok" }))).await;
        let mut config = TestConfig::default();
        config.no_connection_reuse = true;
        let client = HyperHttpClient::from_config(&config).unwrap();
        let url = format!("{base_url}/get");
        let req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        let r1 = client.send(req()).await.unwrap();
        let r2 = client.send(req()).await.unwrap();

        assert!(r1.timings.connecting > 0.0, "first request must connect");
        assert!(
            r2.timings.connecting > 0.0,
            "second request must open a fresh connection when reuse is disabled"
        );
    }

    #[tokio::test]
    async fn configured_user_agent_is_sent() {
        let base_url = start_app(Router::new().route(
            "/ua",
            get(|headers: HeaderMap| async move {
                headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        ))
        .await;

        let mut config = TestConfig::default();
        config.user_agent = Some("custom-hyper-agent/1.0".to_string());
        let client = HyperHttpClient::from_config(&config).unwrap();
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/ua"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(buffered_string(&resp), "custom-hyper-agent/1.0");
    }

    #[tokio::test]
    async fn explicit_user_agent_header_overrides_configured_default() {
        let base_url = start_app(Router::new().route(
            "/ua",
            get(|headers: HeaderMap| async move {
                headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        ))
        .await;

        let mut config = TestConfig::default();
        config.user_agent = Some("configured-agent/1.0".to_string());
        let client = HyperHttpClient::from_config(&config).unwrap();
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/ua"),
                headers: vec![("User-Agent".to_string(), "explicit-agent/2.0".to_string())],
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(buffered_string(&resp), "explicit-agent/2.0");
    }

    #[test]
    fn http_debug_helpers_accept_summary_and_full_modes() {
        let mut config = TestConfig::default();
        config.http_debug = Some("summary".to_string());
        let client = HyperHttpClient::from_config(&config).unwrap();
        debug_request(&client.config, "GET", "http://example.test/", &[]);
        debug_response(&client.config, 200, "http://example.test/", &[]);

        config.http_debug = Some("full".to_string());
        let client = HyperHttpClient::from_config(&config).unwrap();
        let headers = vec![("X-Test".to_string(), "yes".to_string())];
        debug_request(&client.config, "GET", "http://example.test/", &headers);
        debug_response(&client.config, 200, "http://example.test/", &headers);
    }

    #[tokio::test]
    async fn block_hostnames_rejects_before_connect() {
        let mut config = TestConfig::default();
        config.block_hostnames = vec!["*.internal.test".to_string()];
        let client = HyperHttpClient::from_config(&config).unwrap();

        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: "http://api.internal.test/".to_string(),
                headers: Vec::new(),
                body: None,
                timeout: Some(Duration::from_millis(100)),
            })
            .await;

        let err = match result {
            Ok(_) => panic!("blocked hostname must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("blockHostnames"),
            "error should mention blockHostnames; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn blacklist_ips_rejects_literal_ip_before_connect() {
        let mut config = TestConfig::default();
        config.blacklist_ips = vec!["127.0.0.0/8".to_string()];
        let client = HyperHttpClient::from_config(&config).unwrap();

        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: "http://127.0.0.1:9/".to_string(),
                headers: Vec::new(),
                body: None,
                timeout: Some(Duration::from_millis(100)),
            })
            .await;

        let err = match result {
            Ok(_) => panic!("blacklisted literal IP must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("blacklistIPs"),
            "error should mention blacklistIPs, not connection failure; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn blacklist_ips_rejects_dns_result() {
        let mut config = TestConfig::default();
        config.blacklist_ips = vec!["127.0.0.0/8".to_string()];
        let client = HyperHttpClient::from_config(&config).unwrap();

        let result = open_connection(
            &ConnectTarget::Host("127.0.0.1:9".to_string()),
            &client.config,
        )
        .await;

        let err = match result {
            Ok(_) => panic!("blacklisted resolved IP must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("blacklistIPs"),
            "error should mention blacklistIPs; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn hosts_mapping_connects_to_ip_and_preserves_host_header() {
        let base_url = start_app(Router::new().route(
            "/host",
            get(|headers: HeaderMap| async move {
                headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        ))
        .await;
        let port = base_url.rsplit_once(':').unwrap().1;

        let mut config = TestConfig::default();
        config
            .hosts
            .insert("mapped.test".to_string(), "127.0.0.1".to_string());
        let client = HyperHttpClient::from_config(&config).unwrap();

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://mapped.test:{port}/host"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(buffered_string(&resp), format!("mapped.test:{port}"));
    }

    /// S0 — RouteKey shape is the load-bearing identity for the pool. Each
    /// axis (transport, host, port, source_ip, proxy) must independently
    /// distinguish two otherwise-identical keys, or pool reuse will leak
    /// across routes the moment S5/S7/S9/S10 start populating axes.
    #[test]
    fn route_key_equality_distinguishes_transport_host_port_source_proxy() {
        use std::collections::HashSet;

        let base = RouteKey {
            transport: Transport::Http,
            target_host: "example.com".to_string(),
            target_port: 80,
            source_ip: None,
            proxy: None,
        };

        // Same axes → equal.
        assert_eq!(base, base.clone());

        // Different host → distinct.
        let mut other_host = base.clone();
        other_host.target_host = "other.com".to_string();
        assert_ne!(base, other_host);

        // Different port → distinct.
        let mut other_port = base.clone();
        other_port.target_port = 8080;
        assert_ne!(base, other_port);

        // Different source_ip → distinct (S9 lookahead).
        let mut other_src = base.clone();
        other_src.source_ip = Some("127.0.0.2".parse().unwrap());
        assert_ne!(base, other_src);

        // Different proxy → distinct (S10 lookahead).
        let mut other_proxy = base.clone();
        other_proxy.proxy = Some(ProxyRoute { _unused: () });
        assert_ne!(base, other_proxy);

        // HashSet keying must agree with Eq — store one of each variant
        // and verify all are present.
        let set: HashSet<RouteKey> = [base, other_host, other_port, other_src, other_proxy]
            .into_iter()
            .collect();
        assert_eq!(
            set.len(),
            5,
            "five distinct RouteKey variants must hash to five distinct buckets"
        );
    }

    /// S1 — discard mode drops the body buffer but the byte counter still
    /// reflects everything that came off the wire.
    ///
    /// 100 KB body is deliberate — it exceeds hyper's internal body channel
    /// capacity, so a broken implementation that skipped the drain loop
    /// would leave bytes unread from the socket and `data_received` would
    /// fall below the body length. That makes this test regression-lock the
    /// drain invariant, not just the discard-vs-buffer choice.
    #[tokio::test]
    async fn discard_response_bodies_drops_body_keeps_byte_count() {
        let body_text = "x".repeat(100_000);
        let body_for_handler = body_text.clone();
        let app = Router::new().route(
            "/big",
            get(move || {
                let b = body_for_handler.clone();
                async move { b }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut config = TestConfig::default();
        config.discard_response_bodies = true;
        let client = HyperHttpClient::from_config(&config).unwrap();

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://127.0.0.1:{}/big", addr.port()),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert!(
            matches!(resp.body, ResponseBody::Discarded),
            "discard mode must return Discarded, not Buffered"
        );
        assert!(
            resp.data_received >= body_text.len() as u64,
            "data_received ({}) must cover full body ({}) plus headers \
             even when buffer is discarded",
            resp.data_received,
            body_text.len()
        );
    }

    /// S1 — buffer truncates at the cap but `data_received` still reflects
    /// the full wire bytes (the IO wrapper counts past the buffer cutoff).
    /// 100 KB body forces hyper to span multiple TCP reads / frames, so a
    /// broken drain that early-returned on the first frame past the cap
    /// would leave socket bytes unread and `data_received` would fall short
    /// of the body length.
    #[tokio::test]
    async fn max_response_body_size_truncates_buffer_not_count() {
        let body_text = "y".repeat(100_000);
        let body_len = body_text.len();
        let body_for_handler = body_text.clone();
        let app = Router::new().route(
            "/big",
            get(move || {
                let b = body_for_handler.clone();
                async move { b }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Build a client with a deliberately-low cap. The public ctor uses
        // the 10 MB default; override the field directly here since the cap
        // is intentionally not yet exposed through TestConfig.
        let mut client = HyperHttpClient::from_config(&TestConfig::default()).unwrap();
        client.config.max_response_body_size = 1024;

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://127.0.0.1:{}/big", addr.port()),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        let buffered_len = match &resp.body {
            ResponseBody::Buffered(b) => b.len(),
            ResponseBody::Discarded => panic!("buffered mode expected"),
        };
        assert_eq!(
            buffered_len, 1024,
            "buffer must truncate at the cap; got {buffered_len}"
        );
        assert!(
            resp.data_received >= body_len as u64,
            "data_received ({}) must reflect FULL wire bytes ({}+) regardless of cap",
            resp.data_received,
            body_len
        );
    }

    /// S0 — from_config without an explicit max-body knob must inherit the
    /// 10 MiB default. Locks the policy that the cap is currently a constant
    /// inside HyperConfig (not yet a TestConfig field), so a future move to
    /// TestConfig is a deliberate API change rather than an accidental one.
    #[test]
    fn default_max_body_unset_means_10mb_cap() {
        let client = HyperHttpClient::from_config(&TestConfig::default()).unwrap();
        assert_eq!(client.config.max_response_body_size, 10 * 1024 * 1024);
    }

    /// S1 — the central drain-after-cap invariant. Three back-to-back
    /// requests against a server that returns far more body than the buffer
    /// cap. Requests 2 and 3 must reuse the pooled connection (connecting =
    /// 0), AND every response's `data_received` must cover the full body —
    /// the second condition is the load-bearing one. A broken implementation
    /// that capped the buffer without draining the rest would either leave
    /// bytes in hyper's internal channel (and `data_received` would fall
    /// short) OR poison the next request via pooled-conn reuse. 100 KB body
    /// + 1 KB cap exercises both paths.
    #[tokio::test]
    async fn capped_body_drains_full_response_then_returns_clean_conn_to_pool() {
        let body_text = "z".repeat(100_000);
        let body_len = body_text.len() as u64;
        let body_for_handler = body_text.clone();
        let app = Router::new().route(
            "/big",
            get(move || {
                let b = body_for_handler.clone();
                async move { b }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = HyperHttpClient::from_config(&TestConfig::default()).unwrap();
        client.config.max_response_body_size = 1024;

        let url = format!("http://127.0.0.1:{}/big", addr.port());
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

        // All three responses must report identical buffer truncation AND
        // covered the full body bytes on the wire.
        for (i, r) in [&r1, &r2, &r3].iter().enumerate() {
            let buf_len = match &r.body {
                ResponseBody::Buffered(b) => b.len(),
                ResponseBody::Discarded => panic!("buffered mode expected"),
            };
            assert_eq!(buf_len, 1024, "request {i} must truncate at the cap");
            assert!(
                r.data_received >= body_len,
                "request {i} data_received ({}) must cover full body ({}) — \
                 drain must continue past the buffer cap so the IO wrapper sees \
                 every byte",
                r.data_received,
                body_len,
            );
        }

        // First request pays for connection setup; subsequent requests reuse
        // — proves the connection was clean after the drain-past-cap.
        assert!(r1.timings.connecting > 0.0, "r1 must open a fresh conn");
        assert_eq!(
            r2.timings.connecting, 0.0,
            "r2 must reuse the pool — drain-after-cap kept the conn clean"
        );
        assert_eq!(r2.timings.blocked, 0.0, "r2 must skip DNS");
        assert_eq!(r3.timings.connecting, 0.0, "r3 must reuse the pool");
    }

    /// S1 — same invariant as the capped-buffer test, but for the discard
    /// path. Three requests over a 100 KB body; r2 and r3 must reuse the
    /// pooled connection AND every response's `data_received` must cover
    /// the full body — proving the discard path drains rather than just
    /// dropping the Incoming and leaving bytes pending.
    #[tokio::test]
    async fn discard_drains_full_body_and_keeps_pooled_conn_healthy_across_requests() {
        let body_text = "w".repeat(100_000);
        let body_len = body_text.len() as u64;
        let body_for_handler = body_text.clone();
        let app = Router::new().route(
            "/big",
            get(move || {
                let b = body_for_handler.clone();
                async move { b }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut config = TestConfig::default();
        config.discard_response_bodies = true;
        let client = HyperHttpClient::from_config(&config).unwrap();

        let url = format!("http://127.0.0.1:{}/big", addr.port());
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

        for (i, r) in [&r1, &r2, &r3].iter().enumerate() {
            assert!(
                matches!(r.body, ResponseBody::Discarded),
                "response {i} must be Discarded"
            );
            assert!(
                r.data_received >= body_len,
                "response {i} data_received ({}) must cover full body ({}) — \
                 the discard path must drain to end-of-stream",
                r.data_received,
                body_len,
            );
        }
        assert!(r1.timings.connecting > 0.0);
        assert_eq!(
            r2.timings.connecting, 0.0,
            "r2 must reuse — discard path must have drained the body"
        );
        assert_eq!(r3.timings.connecting, 0.0);
    }

    /// S1 — when the drain phase itself errors (server lies about
    /// Content-Length and closes early), the poisoned PooledConn must NOT
    /// re-enter the pool. The next request must succeed via a fresh TCP
    /// connection (connecting > 0).
    #[tokio::test]
    async fn drain_error_evicts_pooled_conn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Mock server: connection 1 emits a content-length-mismatched
        // response (declares 100 bytes, sends only 4 before close).
        // Connection 2 emits a well-formed response.
        tokio::spawn(async move {
            // Connection 1: bad response.
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: keep-alive\r\n\r\nABCD",
            )
            .await
            .unwrap();
            sock.shutdown().await.ok();
            drop(sock);

            // Connection 2: good response, well-formed body, Connection: close.
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
            sock.shutdown().await.ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let client = HyperHttpClient::new();
        let url = format!("http://127.0.0.1:{}/", addr.port());
        let req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        // First request must fail at body drain.
        let r1 = client.send(req()).await;
        assert!(
            r1.is_err(),
            "request must error on content-length mismatch during drain"
        );

        // Second request must succeed against a fresh TCP connection — the
        // poisoned first-attempt conn must NOT have been re-pooled. If it
        // were, r2 would either reuse it (connecting = 0) AND return garbage
        // or fail similarly. We assert fresh setup.
        let r2 = client
            .send(req())
            .await
            .expect("second request must succeed via fresh conn");
        assert_eq!(r2.status, 200);
        assert!(
            r2.timings.connecting > 0.0,
            "r2 must NOT reuse the poisoned conn; got connecting={}",
            r2.timings.connecting
        );
    }
}
