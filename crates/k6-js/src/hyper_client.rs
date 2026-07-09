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
//! Scope through Phase 4:
//!   - HTTP/1.1 plus HTTPS HTTP/2 via ALPN.
//!   - Pooling by [`RouteKey`] — same as the spike's per-authority pool, but
//!     keyed on a type whose shape is frozen now (transport + origin host +
//!     port + source-IP bind + proxy route) so HTTPS/local_ips/proxy slices
//!     don't have to mutate pool identity.
//!   - Request timeout, `no_connection_reuse`, user-agent override, and
//!     `http_debug` are wired.
//!   - `blockHostnames`, `blacklistIPs`, and static `hosts` mappings are
//!     wired.
//!   - Redirects, `Connection: close` pooling behavior, and `localIPs`
//!     source binding are wired.
//!   - Plain HTTP proxy routing and HTTPS CONNECT tunneling are wired.
//!   - Body reader uses a frame loop with cap-then-drain semantics: buffer
//!     truncates at [`HyperConfig::max_response_body_size`] but the remainder
//!     of the response is ALWAYS drained to end-of-stream before the pooled
//!     conn is released, so a capped or discarded body never poisons the
//!     pool. Drain failures evict the conn rather than re-pooling it.

use std::collections::HashMap;
use std::env;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::{http1, http2};
use hyper::header::{CONNECTION, HOST, HeaderValue, LOCATION, USER_AGENT};
use hyper::{Method, Request, Response, Uri};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

use k6_core::config::{TestConfig, TlsVersionConfig};
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

/// Reqwest follows up to 10 redirects by default when no explicit
/// `maxRedirects` option is set.
const DEFAULT_MAX_REDIRECTS: u32 = 10;

/// Identity of a reusable connection.
///
/// Keying the pool on this struct (rather than a raw `"host:port"` string)
/// means subsequent slices can extend connection identity without overloading
/// existing fields' semantics:
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
    Https,
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(crate) struct ProxyRoute {
    scheme: String,
    host: String,
    port: u16,
}

impl ProxyRoute {
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

enum ConnectTarget {
    Host(String),
    Ip(SocketAddr),
}

#[derive(Clone)]
struct HyperRequest {
    method: HttpMethod,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

impl From<HttpRequest> for HyperRequest {
    fn from(req: HttpRequest) -> Self {
        Self {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        }
    }
}

/// Snapshot of every HTTP-shaping knob the hyper client cares about, captured
/// at [`HyperHttpClient::from_config`] time.
///
/// Fields actively read today: `discard_response_bodies`,
/// `max_response_body_size`, `request_timeout`, `no_connection_reuse`,
/// `user_agent_override`, `blacklist_ips`, `block_hostnames`, `hosts`,
/// `local_ips`, and `http_debug`. Other fields are stored so later slices'
/// edits are pure additions; each is annotated with the slice that will
/// consume it.
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

    // Phase 3 — maximum redirects to follow. `0` disables redirects.
    max_redirects: u32,

    // Phase 4 — source-IP round-robin binding.
    local_ips: Vec<IpAddr>,

    // Phase 4 — proxy routing from HTTP_PROXY/http_proxy and
    // HTTPS_PROXY/https_proxy. HTTPS proxies use HTTP CONNECT.
    http_proxy: Option<ProxyRoute>,
    https_proxy: Option<ProxyRoute>,
    no_proxy: Vec<String>,

    // Phase 4 — direct HTTPS/TLS.
    insecure_skip_tls_verify: bool,
    tls_version: Option<TlsVersionConfig>,
    tls_client_config: Arc<rustls::ClientConfig>,

    // Phase 1 — request/response logging to stderr.
    http_debug: Option<String>,
}

/// Hyper-backed HTTP client. See module docs.
pub struct HyperHttpClient {
    config: HyperConfig,
    user_agent: HeaderValue,
    local_ip_index: AtomicUsize,
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

        let local_ips = config
            .local_ips
            .iter()
            .map(|ip| {
                ip.parse::<IpAddr>()
                    .with_context(|| format!("invalid localIP '{ip}'"))
            })
            .collect::<Result<Vec<_>>>()?;

        let hcfg = HyperConfig {
            discard_response_bodies: config.discard_response_bodies,
            max_response_body_size: DEFAULT_MAX_RESPONSE_BODY_SIZE,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            no_connection_reuse: config.no_connection_reuse,
            user_agent_override,
            blacklist_ips,
            block_hostnames: config.block_hostnames.clone(),
            hosts: config.hosts.clone(),
            max_redirects: config.max_redirects.unwrap_or(DEFAULT_MAX_REDIRECTS),
            local_ips,
            http_proxy: proxy_from_env("HTTP_PROXY", "http_proxy")?,
            https_proxy: proxy_from_env("HTTPS_PROXY", "https_proxy")?,
            no_proxy: no_proxy_from_env(),
            insecure_skip_tls_verify: config.insecure_skip_tls_verify,
            tls_version: config.tls_version.clone(),
            tls_client_config: build_tls_client_config(config)?,
            http_debug: config.http_debug.clone(),
        };

        let user_agent = hcfg
            .user_agent_override
            .clone()
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_USER_AGENT));

        Ok(Self {
            config: hcfg,
            user_agent,
            local_ip_index: AtomicUsize::new(0),
            pool: Mutex::new(HashMap::new()),
        })
    }

    fn next_source_ip(&self) -> Option<IpAddr> {
        if self.config.local_ips.is_empty() {
            return None;
        }
        let idx = self.local_ip_index.fetch_add(1, Ordering::Relaxed) % self.config.local_ips.len();
        Some(self.config.local_ips[idx])
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

/// One pooled HTTP connection: a hyper SendRequest paired with the IO-level
/// metrics for that physical TCP stream, plus the task driving the connection
/// future. Dropping this aborts the conn task so the FD is released promptly.
struct PooledConn {
    sender: PooledSender,
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

enum PooledSender {
    Http1(http1::SendRequest<Full<Bytes>>),
    Http2(http2::SendRequest<Full<Bytes>>),
}

impl PooledSender {
    fn protocol(&self) -> ConnectionProtocol {
        match self {
            Self::Http1(_) => ConnectionProtocol::Http1,
            Self::Http2(_) => ConnectionProtocol::Http2,
        }
    }

    async fn send_request(
        &mut self,
        request: Request<Full<Bytes>>,
    ) -> hyper::Result<Response<Incoming>> {
        match self {
            Self::Http1(sender) => sender.send_request(request).await,
            Self::Http2(sender) => sender.send_request(request).await,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionProtocol {
    Http1,
    Http2,
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
struct CountingStream<S> {
    inner: S,
    metrics: WireMetrics,
}

impl<S> AsyncWrite for CountingStream<S>
where
    S: AsyncWrite + Unpin,
{
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

impl<S> AsyncRead for CountingStream<S>
where
    S: AsyncRead + Unpin,
{
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
    let mut current = HyperRequest::from(req);
    let mut redirects_followed = 0;

    loop {
        let response = send_once(client, current.clone(), user_agent.clone()).await?;
        let Some(next_url) = redirect_target(&response, &current.url)? else {
            return Ok(response);
        };

        if client.config.max_redirects == 0 {
            return Ok(response);
        }
        if redirects_followed >= client.config.max_redirects {
            anyhow::bail!(
                "too many redirects: exceeded maxRedirects={}",
                client.config.max_redirects
            );
        }

        redirects_followed += 1;
        current = redirected_request(current, next_url, response.status);
    }
}

async fn send_once(
    client: &HyperHttpClient,
    req: HyperRequest,
    user_agent: HeaderValue,
) -> Result<HttpResponse> {
    let start = Instant::now();

    let uri: Uri = req.url.parse().context("invalid URL")?;
    let scheme = uri.scheme_str().unwrap_or("http");
    let transport = transport_for_scheme(scheme)?;
    let host = uri.host().context("URL missing host")?.to_string();
    let port = uri.port_u16().unwrap_or(match transport {
        Transport::Http => 80,
        Transport::Https => 443,
    });
    let origin_authority = format!("{host}:{port}");

    check_blocked_hostname(&client.config, &host)?;
    check_literal_ip_blacklist(&client.config, &host)?;

    let proxy = proxy_for(&client.config, scheme, &host);
    let (target_host, connect_target) = if let Some(proxy) = &proxy {
        (host.clone(), ConnectTarget::Host(proxy.authority()))
    } else {
        resolve_connect_target(&client.config, &host, port)?
    };
    let source_ip = client.next_source_ip();

    // RouteKey identifies the pool slot for this connection. In S0 every axis
    // except `transport`/`target_host`/`target_port` is fixed (no local-IP
    // bind, no proxy, plain HTTP). The shape is frozen so S5/S7/S9/S10 each
    // populate their own axis without overloading existing semantics — see
    // the type's doc-comment.
    let route_key = RouteKey {
        transport,
        target_host: target_host.clone(),
        target_port: port,
        source_ip,
        proxy: proxy.clone(),
    };

    // Try the pool first. On a hit, `blocked` and `connecting` are 0 — the
    // request reuses an existing TCP connection. On a miss, do the DNS+TCP
    // dance and record real timings for those phases.
    let (mut pooled, blocked_ms, connecting_ms, tls_handshaking_ms) =
        if !client.config.no_connection_reuse {
            if let Some(p) = client.try_acquire(&route_key) {
                (p, 0.0, 0.0, 0.0)
            } else {
                open_connection(
                    &connect_target,
                    source_ip,
                    transport,
                    &host,
                    proxy
                        .as_ref()
                        .filter(|_| transport == Transport::Https)
                        .map(|_| origin_authority.as_str()),
                    &client.config,
                )
                .await?
            }
        } else {
            open_connection(
                &connect_target,
                source_ip,
                transport,
                &host,
                proxy
                    .as_ref()
                    .filter(|_| transport == Transport::Https)
                    .map(|_| origin_authority.as_str()),
                &client.config,
            )
            .await?
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
    let request_uri = if pooled.sender.protocol() == ConnectionProtocol::Http2 {
        req.url.clone()
    } else if proxy.is_some() {
        req.url.clone()
    } else {
        uri.path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".to_string())
    };
    let has_user_agent = req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
    let mut builder = Request::builder()
        .method(method)
        .uri(request_uri)
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
    let should_close = connection_close_requested(&headers);
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
        tls_handshaking: tls_handshaking_ms,
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
    if client.config.no_connection_reuse || should_close {
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

fn redirect_target(response: &HttpResponse, base_url: &str) -> Result<Option<String>> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }

    let Some(location) = header_value(&response.headers, LOCATION.as_str()) else {
        return Ok(None);
    };
    let base = url::Url::parse(base_url).context("parsing redirect base URL")?;
    let next = base
        .join(location)
        .with_context(|| format!("invalid redirect Location {location:?}"))?;
    Ok(Some(next.to_string()))
}

fn redirected_request(mut req: HyperRequest, next_url: String, status: u16) -> HyperRequest {
    if matches!(status, 301 | 302 | 303) && matches!(req.method, HttpMethod::Post) {
        req.method = HttpMethod::Get;
        req.body = None;
        req.headers.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("content-length") && !k.eq_ignore_ascii_case("content-type")
        });
    }
    req.url = next_url;
    req
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn connection_close_requested(headers: &[(String, String)]) -> bool {
    header_value(headers, CONNECTION.as_str())
        .map(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("close"))
        })
        .unwrap_or(false)
}

fn proxy_from_env(upper: &str, lower: &str) -> Result<Option<ProxyRoute>> {
    let Some(raw) = env::var(upper).ok().or_else(|| env::var(lower).ok()) else {
        return Ok(None);
    };

    parse_http_proxy(&raw).map(Some)
}

fn parse_http_proxy(raw: &str) -> Result<ProxyRoute> {
    let proxy_url = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let parsed =
        url::Url::parse(&proxy_url).with_context(|| format!("invalid HTTP proxy URL {raw:?}"))?;
    anyhow::ensure!(
        parsed.scheme() == "http",
        "unsupported proxy scheme {:?}; plain HTTP proxy support requires http://",
        parsed.scheme()
    );
    let host = parsed
        .host_str()
        .context("HTTP proxy URL missing host")?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .context("HTTP proxy URL missing port")?;
    Ok(ProxyRoute {
        scheme: parsed.scheme().to_string(),
        host,
        port,
    })
}

fn no_proxy_from_env() -> Vec<String> {
    env::var("NO_PROXY")
        .ok()
        .or_else(|| env::var("no_proxy").ok())
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn proxy_for(config: &HyperConfig, scheme: &str, host: &str) -> Option<ProxyRoute> {
    if no_proxy_matches(&config.no_proxy, host) {
        return None;
    }
    match scheme {
        "http" => config.http_proxy.clone(),
        "https" => config.https_proxy.clone(),
        _ => None,
    }
}

fn transport_for_scheme(scheme: &str) -> Result<Transport> {
    match scheme {
        "http" => Ok(Transport::Http),
        "https" => Ok(Transport::Https),
        _ => anyhow::bail!("unsupported URL scheme `{scheme}`"),
    }
}

fn no_proxy_matches(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|pattern| {
        let p = pattern.trim();
        if p == "*" {
            return true;
        }
        let p = p
            .strip_prefix("http://")
            .or_else(|| p.strip_prefix("https://"))
            .unwrap_or(p);
        let p = p.split(':').next().unwrap_or(p).trim_start_matches('.');
        host == p || host.ends_with(&format!(".{p}"))
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
    source_ip: Option<IpAddr>,
    transport: Transport,
    tls_server_name: &str,
    proxy_connect_authority: Option<&str>,
    config: &HyperConfig,
) -> Result<(PooledConn, f64, f64, f64)> {
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
    let tcp = connect_tcp(addr, source_ip).await?;
    let connect_done = Instant::now();

    let wire = WireMetrics::default();
    let mut stream = CountingStream {
        inner: tcp,
        metrics: wire.clone(),
    };
    if let Some(authority) = proxy_connect_authority {
        establish_connect_tunnel(&mut stream, authority).await?;
    }
    let (sender, conn_task, tls_handshaking) = match transport {
        Transport::Http => {
            let (sender, connection) = http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .context("hyper http1 handshake failed")?;
            let conn_task = tokio::spawn(async move {
                let _ = connection.await;
            });
            (PooledSender::Http1(sender), conn_task, 0.0)
        }
        Transport::Https => {
            let tls_start = Instant::now();
            let server_name = ServerName::try_from(tls_server_name.to_string())
                .with_context(|| format!("invalid TLS server name {tls_server_name:?}"))?;
            let tls_stream = TlsConnector::from(config.tls_client_config.clone())
                .connect(server_name, stream)
                .await
                .context("TLS handshake failed")?;
            let tls_handshaking = tls_start.elapsed().as_secs_f64() * 1000.0;
            let is_h2 = tls_stream
                .get_ref()
                .1
                .alpn_protocol()
                .is_some_and(|protocol| protocol == b"h2");
            if is_h2 {
                let (sender, connection) = http2::Builder::new(TokioExecutor::new())
                    .handshake::<_, Full<Bytes>>(TokioIo::new(tls_stream))
                    .await
                    .context("hyper http2 handshake failed")?;
                let conn_task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                (PooledSender::Http2(sender), conn_task, tls_handshaking)
            } else {
                let (sender, connection) =
                    http1::handshake::<_, Full<Bytes>>(TokioIo::new(tls_stream))
                        .await
                        .context("hyper http1 handshake failed")?;
                let conn_task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                (PooledSender::Http1(sender), conn_task, tls_handshaking)
            }
        }
    };

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
        tls_handshaking,
    ))
}

async fn establish_connect_tunnel<S>(stream: &mut S, authority: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing CONNECT request to proxy")?;
    stream
        .flush()
        .await
        .context("flushing CONNECT request to proxy")?;

    let mut response = Vec::new();
    let mut buf = [0_u8; 512];
    while !response.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream
            .read(&mut buf)
            .await
            .context("reading CONNECT response from proxy")?;
        anyhow::ensure!(n != 0, "proxy closed connection before CONNECT response");
        response.extend_from_slice(&buf[..n]);
        anyhow::ensure!(
            response.len() <= 8192,
            "proxy CONNECT response exceeded 8192 bytes"
        );
    }

    let first_line_end = response
        .windows(2)
        .position(|w| w == b"\r\n")
        .context("proxy CONNECT response missing status line")?;
    let first_line = String::from_utf8_lossy(&response[..first_line_end]);
    anyhow::ensure!(
        first_line.starts_with("HTTP/1.1 200") || first_line.starts_with("HTTP/1.0 200"),
        "proxy CONNECT failed: {first_line}"
    );
    Ok(())
}

fn build_tls_client_config(config: &TestConfig) -> Result<Arc<rustls::ClientConfig>> {
    let versions = tls_protocol_versions(config.tls_version.as_ref())?;
    let version_slice = versions.as_deref().unwrap_or(rustls::DEFAULT_VERSIONS);
    let builder = rustls::ClientConfig::builder_with_provider(rustls_provider())
        .with_protocol_versions(version_slice)
        .context("building TLS client protocol versions")?;

    let mut client_config = if config.insecure_skip_tls_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    client_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(client_config))
}

fn rustls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn tls_protocol_versions(
    tls: Option<&TlsVersionConfig>,
) -> Result<Option<Vec<&'static rustls::SupportedProtocolVersion>>> {
    let Some(tls) = tls else {
        return Ok(None);
    };

    let min = tls.min.as_deref().and_then(supported_tls_version_rank);
    let max = tls.max.as_deref().and_then(supported_tls_version_rank);

    if let (Some(min), Some(max)) = (min, max) {
        anyhow::ensure!(
            min <= max,
            "invalid tlsVersion range: min {:?} is greater than max {:?}",
            tls.min,
            tls.max
        );
    }

    let versions: Vec<&'static rustls::SupportedProtocolVersion> = [
        (3_u8, &rustls::version::TLS13),
        (2_u8, &rustls::version::TLS12),
    ]
    .into_iter()
    .filter(|(rank, _)| min.is_none_or(|m| *rank >= m) && max.is_none_or(|m| *rank <= m))
    .map(|(_, version)| version)
    .collect();

    if versions.is_empty() {
        Ok(None)
    } else {
        Ok(Some(versions))
    }
}

fn supported_tls_version_rank(version: &str) -> Option<u8> {
    match version {
        "tls1.2" => Some(2),
        "tls1.3" => Some(3),
        _ => None,
    }
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(rustls_provider()))
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn connect_tcp(addr: SocketAddr, source_ip: Option<IpAddr>) -> Result<TcpStream> {
    let Some(source_ip) = source_ip else {
        return TcpStream::connect(addr).await.context("TCP connect failed");
    };

    if source_ip.is_ipv4() != addr.is_ipv4() {
        anyhow::bail!("localIP {source_ip} address family does not match remote {addr}");
    }

    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }
    .context("creating TCP socket")?;

    socket
        .bind(SocketAddr::new(source_ip, 0))
        .with_context(|| format!("binding localIP {source_ip}"))?;
    socket.connect(addr).await.context("TCP connect failed")
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
    use std::convert::Infallible;

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use base64::Engine as _;
    use hyper::Version;
    use hyper::service::service_fn;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    const TEST_TLS_CERT_DER_B64: &str = "MIIDJTCCAg2gAwIBAgIUaPczj/t0/C0gODXNAiU8TaDSAgIwDQYJKoZIhvcNAQELBQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwOTA1MDQzM1oXDTM2MDcwNjA1MDQzM1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEArrZWpRB7AesYDV40L/hSV5tNSxZ7GxxcO8hhjsy+9UYka4BnVRCdmkG6J5bt81LARql/ZQAkQ3hb0mosflgqmm6Ht4GGFt32Am31CjR+jVTjCyM5Is4rm+3oTomXl/UKe9KaJkidxtk3VJ58mIt9dP0F/IsP00bxoeAvTGroOGFVorPj2OypxRaU9qaFcvK4/I+eDCJmxT9MV866//AwOiBNth7W+nJkwCEl5wIJkkw5nRQkMWrJyocKPS2+9dhRKp+2CFJ3T8oEkEqDIxr1eLi8n1sV8byH4jhZZp0l4+9MV4AVcnngBFv4Q0SqfKVbs8A4gVge+TDgZmdgDlxGBQIDAQABo28wbTAdBgNVHQ4EFgQUYudNX1WRv3OAB6nPtk+37fVmGjYwHwYDVR0jBBgwFoAUYudNX1WRv3OAB6nPtk+37fVmGjYwDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwDQYJKoZIhvcNAQELBQADggEBADxaHHJkMN2mMQTokoScJkqk2sbg8eqW+PKcJA7RgjwZbjtp2bg82ctLa1k+5/wH7cqnvh1LOBMb7rtHh5WkE/v/Q1f4oVleV8wSEOjLWbNfvi/OxemTzSBJAzxPzhOGhhzyXiRAAsN3PV/zDTSq3Yb9XBKOtwDOIN70LzLElNnDXqyEFGOp7W7NQWJAUW2GyBc9rhWyxiPQ4oeRCxOZh+CB86jTuVY2NZSkeegF4TALfP6Oo4fznPRDUys0fVgO8Jv+A1+ZqUi7h9QfXbZzgrsPKbVzQkr8HHCkBKXnynyMKbIMGzE2Nt1l/+qwgka/pa0ouB0OpuyEMU+cS5NHXnE=";
    const TEST_TLS_KEY_DER_B64: &str = "MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQCutlalEHsB6xgNXjQv+FJXm01LFnsbHFw7yGGOzL71RiRrgGdVEJ2aQbonlu3zUsBGqX9lACRDeFvSaix+WCqaboe3gYYW3fYCbfUKNH6NVOMLIzkiziub7ehOiZeX9Qp70pomSJ3G2TdUnnyYi310/QX8iw/TRvGh4C9Maug4YVWis+PY7KnFFpT2poVy8rj8j54MImbFP0xXzrr/8DA6IE22Htb6cmTAISXnAgmSTDmdFCQxasnKhwo9Lb712FEqn7YIUndPygSQSoMjGvV4uLyfWxXxvIfiOFlmnSXj70xXgBVyeeAEW/hDRKp8pVuzwDiBWB75MOBmZ2AOXEYFAgMBAAECggEAB4Bwy/mfLn/nsns/BmhFMNnMQdMfShS3qSF7fuQvttxiJ/OFfFOQUNVNpvGGGhKNivswKygMZpE+cBR7AJnMioEAdtKq7URukcAi62NBo9PnQ80pYOM1YCag+O5TggTVhGeQkuA/VhBxncKIWwxyQJm0rhlSfqHnMiosHb3hZrpE+m8E+b9nvCTlKSo/VdYSHE0CnNA5OVXb5bHJR/0eBrWdYN6g9MynHLrQQaP5BDgbRaOpoZ8p2AM+hc9O2c5kAIVTyBA51RmV64QaheuUuIVhDFWWVRGrwy84NQx9v1xdYx+iJCD3HU6k+HRgrPEpQ5ptsZZjNOZKP9C1dwWpaQKBgQDkDdkv01NPkqldI7RSMrFHJJula1Xy9cQ0NvOI3vrixZVEl02KXh7IP4teZbMqb949zujbwsp4oQU58QU30zgIGyKxKWBtygGan8ovJBa3PmDUqLA80ibnjEaa9FUzIAQAf+dzcT24EUX6jg71kDQiSa3OjaZ3dHDXNxFqDS6rDwKBgQDEHyJxuSV/uBnEdsCqPKu08sd+MY1+xHcpRaNuX7Gd5EqeRe+Es9VZDlQwQ/iRm4EiZrzoPz9jP/g2dL9772LN+yvXzajnYLfyHir9wDw5fO1S+YVOa3TM1cOVqnBzO8/vpDG3rZ7XI96nXWN0ZOgEZepEeCTv0QgqG5kqP3vNqwKBgQDDqhIO04ymOBoxvGGJKM8rUABu1AHhO/YEKqWWaGHvUUC5oes4bXqRqtuDuVQYc/TFKRJnAuC+0MBwLxegBwwLAGUqhWqjp+7qYHCTM659t/pSWw0ikdgpUBR//GRhQfXNC/Bj/uPKWp+k0l+JVxkz1e1Wy/fog7IRJME/MWI6BwKBgQCRAZQuEX6wWCZ1JHh/ZixutbLakzjTKeARG/Qif46L92dUbtERhQWRuw50QU1gG2H3VY8HCPyNHZcgbGHH+M9NDRD1lpHzwYc/9R5EUAY3Wy790o/F052gdc0Os95A1VCBFx3LeQugdl0B0gLe5FzII7J6vXpR9nPa7lzo59dZ0QKBgQDj+4zksm7lmp9bapd4RXqr3FX6V/tStpHR687r08kLBxmjTC0eDeifnDhqly1IeStcVW3jC8MFa4nUtwAcRwiqFtP185tq8hu7ESL9OuDePve6C7EhDveKup3xXkGqS0N20f/4vvTHo3bFVOCxi/X91N6DqrXOzoZvya8NWnIvMw==";

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

    fn test_tls_server_config(alpn_protocols: Vec<Vec<u8>>) -> rustls::ServerConfig {
        let cert = CertificateDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(TEST_TLS_CERT_DER_B64)
                .unwrap(),
        );
        let key = rustls_pki_types::PrivateKeyDer::try_from(
            base64::engine::general_purpose::STANDARD
                .decode(TEST_TLS_KEY_DER_B64)
                .unwrap(),
        )
        .unwrap();
        let mut server_config = rustls::ServerConfig::builder_with_provider(rustls_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        server_config.alpn_protocols = alpn_protocols;
        server_config
    }

    async fn start_tls_fixture() -> String {
        let server_config = test_tls_server_config(Vec::new());
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut buf = [0_u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\ntls-good",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        format!("https://127.0.0.1:{}", addr.port())
    }

    async fn start_h2_tls_fixture() -> String {
        let server_config = test_tls_server_config(vec![b"h2".to_vec()]);
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let service = service_fn(|req: Request<Incoming>| async move {
                        assert_eq!(req.version(), Version::HTTP_2);
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            b"h2-good",
                        ))))
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        format!("https://127.0.0.1:{}", addr.port())
    }

    async fn start_connect_proxy() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 512];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = inbound.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        if request.len() > 8192 {
                            return;
                        }
                    }
                    let first_line_end = request
                        .windows(2)
                        .position(|w| w == b"\r\n")
                        .unwrap_or(request.len());
                    let first_line = String::from_utf8_lossy(&request[..first_line_end]);
                    let Some(authority) = first_line
                        .strip_prefix("CONNECT ")
                        .and_then(|rest| rest.split_whitespace().next())
                    else {
                        let _ = inbound
                            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        return;
                    };

                    let Ok(mut upstream) = tokio::net::TcpStream::connect(authority).await else {
                        let _ = inbound
                            .write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        return;
                    };
                    if inbound
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .is_ok()
                    {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await;
                    }
                });
            }
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
    async fn https_rejects_self_signed_cert_by_default() {
        let base_url = start_tls_fixture().await;
        let client = HyperHttpClient::new();
        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/secure"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await;
        let err = match result {
            Ok(_) => panic!("self-signed HTTPS fixture must fail with default verification"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("TLS handshake failed"),
            "error should retain TLS context; got: {msg}"
        );
    }

    #[tokio::test]
    async fn https_insecure_skip_verify_succeeds_and_records_tls_timing() {
        let base_url = start_tls_fixture().await;
        let mut config = TestConfig::default();
        config.insecure_skip_tls_verify = true;
        let client = HyperHttpClient::from_config(&config).unwrap();

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/secure"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(buffered_string(&resp), "tls-good");
        assert!(
            resp.timings.tls_handshaking > 0.0,
            "HTTPS must measure TLS handshaking; got {}",
            resp.timings.tls_handshaking
        );
        assert!(
            resp.data_sent > 0,
            "TLS request must count encrypted wire bytes sent"
        );
        assert!(
            resp.data_received > "tls-good".len() as u64,
            "TLS response must count encrypted response bytes plus headers"
        );
    }

    #[tokio::test]
    async fn https_proxy_connect_tunnels_tls_to_origin() {
        let base_url = start_tls_fixture().await;
        let proxy_url = start_connect_proxy().await;
        let mut config = TestConfig::default();
        config.insecure_skip_tls_verify = true;
        let mut client = HyperHttpClient::from_config(&config).unwrap();
        client.config.https_proxy = Some(parse_http_proxy(&proxy_url).unwrap());

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/secure"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(buffered_string(&resp), "tls-good");
        assert!(
            resp.timings.tls_handshaking > 0.0,
            "proxied HTTPS must still measure origin TLS handshaking"
        );
    }

    #[tokio::test]
    async fn https_negotiates_http2_when_server_selects_h2() {
        let base_url = start_h2_tls_fixture().await;
        let mut config = TestConfig::default();
        config.insecure_skip_tls_verify = true;
        let client = HyperHttpClient::from_config(&config).unwrap();

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/h2"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(buffered_string(&resp), "h2-good");
        assert!(resp.timings.tls_handshaking > 0.0);
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
        config.max_redirects = Some(3);
        config.local_ips = vec!["127.0.0.1".to_string(), "127.0.0.2".to_string()];
        config.insecure_skip_tls_verify = true;
        config.tls_version = Some(TlsVersionConfig {
            min: Some("tls1.2".to_string()),
            max: Some("tls1.3".to_string()),
        });

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
        assert_eq!(client.config.max_redirects, 3);
        assert_eq!(client.config.local_ips.len(), 2);
        assert!(client.config.insecure_skip_tls_verify);
        assert_eq!(
            client.config.tls_version.as_ref().unwrap().min.as_deref(),
            Some("tls1.2")
        );
        assert_eq!(client.config.http_debug.as_deref(), Some("full"));
    }

    #[test]
    fn from_config_rejects_invalid_supported_tls_version_range() {
        let mut config = TestConfig::default();
        config.tls_version = Some(TlsVersionConfig {
            min: Some("tls1.3".to_string()),
            max: Some("tls1.2".to_string()),
        });

        let err = match HyperHttpClient::from_config(&config) {
            Ok(_) => panic!("invalid supported TLS version range must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("invalid tlsVersion range"),
            "error should mention invalid tlsVersion range; got: {err:#}"
        );
    }

    #[test]
    fn tls_version_ignores_rustls_unsupported_legacy_versions_like_reqwest() {
        let mut config = TestConfig::default();
        config.tls_version = Some(TlsVersionConfig {
            min: Some("tls1.0".to_string()),
            max: Some("tls1.1".to_string()),
        });

        let client = HyperHttpClient::from_config(&config).unwrap();
        assert_eq!(
            client.config.tls_version.as_ref().unwrap().min.as_deref(),
            Some("tls1.0")
        );
    }

    #[test]
    fn from_config_invalid_local_ip_fails() {
        let mut config = TestConfig::default();
        config.local_ips = vec!["not-an-ip".to_string()];

        let err = match HyperHttpClient::from_config(&config) {
            Ok(_) => panic!("invalid localIP must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("invalid localIP"),
            "error should mention invalid localIP; got: {err:#}"
        );
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
            None,
            Transport::Http,
            "127.0.0.1",
            None,
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

    #[tokio::test]
    async fn redirects_follow_location_and_return_final_url() {
        let base_url = start_app(
            Router::new()
                .route(
                    "/start",
                    get(|| async {
                        (
                            StatusCode::FOUND,
                            [(LOCATION.as_str(), "/final")],
                            "redirecting",
                        )
                    }),
                )
                .route("/final", get(|| async { "done" })),
        )
        .await;

        let client = HyperHttpClient::new();
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/start"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(resp.url, format!("{base_url}/final"));
        assert_eq!(buffered_string(&resp), "done");
    }

    #[tokio::test]
    async fn max_redirects_zero_returns_redirect_response() {
        let base_url = start_app(Router::new().route(
            "/start",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(LOCATION.as_str(), "/final")],
                    "redirecting",
                )
            }),
        ))
        .await;

        let mut config = TestConfig::default();
        config.max_redirects = Some(0);
        let client = HyperHttpClient::from_config(&config).unwrap();
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/start"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.status, 302);
        assert_eq!(resp.url, format!("{base_url}/start"));
        assert_eq!(buffered_string(&resp), "redirecting");
    }

    #[tokio::test]
    async fn too_many_redirects_errors() {
        let base_url = start_app(Router::new().route(
            "/loop",
            get(|| async { (StatusCode::FOUND, [(LOCATION.as_str(), "/loop")], "loop") }),
        ))
        .await;

        let mut config = TestConfig::default();
        config.max_redirects = Some(1);
        let client = HyperHttpClient::from_config(&config).unwrap();
        let result = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("{base_url}/loop"),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await;

        let err = match result {
            Ok(_) => panic!("redirect loop must fail"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("too many redirects"),
            "error should mention redirect limit; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn connection_close_response_is_not_repooled() {
        let base_url = start_app(Router::new().route(
            "/close",
            get(|| async { (StatusCode::OK, [(CONNECTION.as_str(), "close")], "ok") }),
        ))
        .await;

        let client = HyperHttpClient::new();
        let url = format!("{base_url}/close");
        let req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        let r1 = client.send(req()).await.unwrap();
        let r2 = client.send(req()).await.unwrap();

        assert_eq!(r1.status, 200);
        assert!(
            r2.timings.connecting > 0.0,
            "second request must open a new connection after Connection: close"
        );
    }

    #[tokio::test]
    async fn local_ips_round_robin_source_binding() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(2);

        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, peer) = listener.accept().await.unwrap();
                tx.send(peer.ip()).await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                )
                .await
                .unwrap();
                sock.shutdown().await.ok();
            }
        });

        let mut config = TestConfig::default();
        config.local_ips = vec!["127.0.0.1".to_string(), "127.0.0.2".to_string()];
        let client = HyperHttpClient::from_config(&config).unwrap();
        let url = format!("http://127.0.0.1:{}/", addr.port());
        let req = || HttpRequest {
            method: HttpMethod::Get,
            url: url.clone(),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };

        let r1 = client.send(req()).await.unwrap();
        let r2 = client.send(req()).await.unwrap();
        let p1 = rx.recv().await.unwrap();
        let p2 = rx.recv().await.unwrap();

        assert_eq!(r1.status, 200);
        assert_eq!(r2.status, 200);
        assert_eq!(p1, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(p2, "127.0.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn parses_http_proxy_url_variants() {
        let explicit = parse_http_proxy("http://127.0.0.1:8080").unwrap();
        assert_eq!(explicit.scheme, "http");
        assert_eq!(explicit.host, "127.0.0.1");
        assert_eq!(explicit.port, 8080);

        let implicit = parse_http_proxy("proxy.local:3128").unwrap();
        assert_eq!(implicit.scheme, "http");
        assert_eq!(implicit.host, "proxy.local");
        assert_eq!(implicit.port, 3128);
    }

    #[test]
    fn no_proxy_patterns_bypass_proxy() {
        let patterns = vec![
            "localhost".to_string(),
            ".internal.test".to_string(),
            "api.example.com:8080".to_string(),
        ];

        assert!(no_proxy_matches(&patterns, "localhost"));
        assert!(no_proxy_matches(&patterns, "svc.internal.test"));
        assert!(no_proxy_matches(&patterns, "api.example.com"));
        assert!(!no_proxy_matches(&patterns, "public.example.com"));
    }

    #[tokio::test]
    async fn http_proxy_receives_absolute_form_request_and_origin_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::oneshot;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            let request_text = String::from_utf8_lossy(&buf[..n]).to_string();
            tx.send(request_text).ok();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
            sock.shutdown().await.ok();
        });

        let mut client = HyperHttpClient::from_config(&TestConfig::default()).unwrap();
        client.config.http_proxy = Some(ProxyRoute {
            scheme: "http".to_string(),
            host: proxy_addr.ip().to_string(),
            port: proxy_addr.port(),
        });

        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: "http://origin.test/proxy-path?q=1".to_string(),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        let request_text = rx.await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            request_text.starts_with("GET http://origin.test/proxy-path?q=1 HTTP/1.1\r\n"),
            "proxy request must use absolute-form URI; got:\n{request_text}"
        );
        assert!(
            request_text
                .lines()
                .any(|line| line.eq_ignore_ascii_case("host: origin.test:80")),
            "proxy request must preserve origin Host header; got:\n{request_text}"
        );
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
        other_proxy.proxy = Some(ProxyRoute {
            scheme: "http".to_string(),
            host: "proxy.local".to_string(),
            port: 8080,
        });
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
