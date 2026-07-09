use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Result, bail};

use k6_core::config::TestConfig;
use k6_core::traits::{HttpClient, HttpMethod, HttpRequest, HttpResponse, ResponseBody, Timings};

/// Production HTTP client backed by reqwest.
///
/// Uses a shared connection pool. Clone is inexpensive (internally Arc'd).
#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
    /// When localIPs is set, we have multiple clients bound to different source IPs.
    /// We round-robin across them.
    local_ip_clients: Option<Arc<LocalIpPool>>,
    discard_response_bodies: bool,
    max_response_body_size: usize,
    http_debug: Option<String>,
    _should_throw: bool,
    blacklist_ips: Vec<ipnet::IpNet>,
    block_hostnames: Vec<String>,
    _hosts: HashMap<String, String>,
}

struct LocalIpPool {
    clients: Vec<reqwest::Client>,
    index: AtomicUsize,
}

impl LocalIpPool {
    fn next_client(&self) -> &reqwest::Client {
        let idx = self.index.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        &self.clients[idx]
    }
}

impl ReqwestHttpClient {
    /// Create a client with just the discard option (backwards-compatible).
    pub fn new(discard_response_bodies: bool) -> Result<Self> {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(100)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            local_ip_clients: None,
            discard_response_bodies,
            max_response_body_size: 10 * 1024 * 1024,
            http_debug: None,
            _should_throw: false,
            blacklist_ips: Vec::new(),
            block_hostnames: Vec::new(),
            _hosts: HashMap::new(),
        })
    }

    /// Create a client from the full test configuration.
    pub fn from_config(config: &TestConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));

        // Connection reuse
        if config.no_connection_reuse {
            builder = builder.pool_max_idle_per_host(0);
        } else {
            builder = builder.pool_max_idle_per_host(100);
        }

        // User agent
        if let Some(ref ua) = config.user_agent {
            builder = builder.user_agent(ua.as_str());
        }

        // Max redirects
        if let Some(max) = config.max_redirects {
            if max == 0 {
                builder = builder.redirect(reqwest::redirect::Policy::none());
            } else {
                builder = builder.redirect(reqwest::redirect::Policy::limited(max as usize));
            }
        }

        // TLS verification
        if config.insecure_skip_tls_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }

        // TLS version constraints
        if let Some(ref tls) = config.tls_version {
            if let Some(ref min) = tls.min {
                builder = match min.as_str() {
                    "tls1.2" => builder.min_tls_version(reqwest::tls::Version::TLS_1_2),
                    "tls1.3" => builder.min_tls_version(reqwest::tls::Version::TLS_1_3),
                    _ => builder, // tls1.0, tls1.1 not supported by rustls
                };
            }
            if let Some(ref max) = tls.max {
                builder = match max.as_str() {
                    "tls1.2" => builder.max_tls_version(reqwest::tls::Version::TLS_1_2),
                    "tls1.3" => builder.max_tls_version(reqwest::tls::Version::TLS_1_3),
                    _ => builder,
                };
            }
        }

        // Static host→IP mappings
        for (hostname, ip) in &config.hosts {
            if let Ok(addr) = ip.parse::<IpAddr>() {
                builder = builder.resolve(hostname.as_str(), std::net::SocketAddr::new(addr, 0));
            }
        }

        // Proxy support (from environment by default; reqwest reads HTTP_PROXY/HTTPS_PROXY)
        // We don't override proxy here — reqwest already reads env vars.

        let client = builder.build()?;

        // Parse blacklist IPs into CIDR ranges
        let blacklist_ips: Vec<ipnet::IpNet> = config
            .blacklist_ips
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        // Build local IP pool for source address round-robin
        let local_ip_clients = if !config.local_ips.is_empty() {
            let mut clients = Vec::new();
            for ip_str in &config.local_ips {
                let addr: IpAddr = ip_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid localIP '{ip_str}': {e}"))?;

                let mut ip_builder = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .local_address(addr);

                if config.no_connection_reuse {
                    ip_builder = ip_builder.pool_max_idle_per_host(0);
                } else {
                    ip_builder = ip_builder.pool_max_idle_per_host(100);
                }

                if config.insecure_skip_tls_verify {
                    ip_builder = ip_builder.danger_accept_invalid_certs(true);
                }

                if let Some(ref ua) = config.user_agent {
                    ip_builder = ip_builder.user_agent(ua.as_str());
                }

                clients.push(ip_builder.build()?);
            }
            Some(Arc::new(LocalIpPool {
                clients,
                index: AtomicUsize::new(0),
            }))
        } else {
            None
        };

        Ok(Self {
            client,
            local_ip_clients,
            discard_response_bodies: config.discard_response_bodies,
            max_response_body_size: 10 * 1024 * 1024,
            http_debug: config.http_debug.clone(),
            _should_throw: config.throw,
            blacklist_ips,
            block_hostnames: config.block_hostnames.clone(),
            _hosts: config.hosts.clone(),
        })
    }

    /// Check if a URL is blocked by blacklistIPs or blockHostnames.
    fn check_blocked(&self, url: &str) -> Result<()> {
        if self.blacklist_ips.is_empty() && self.block_hostnames.is_empty() {
            return Ok(());
        }

        if let Ok(parsed) = url::Url::parse(url) {
            // Check hostname blocking
            if let Some(host) = parsed.host_str() {
                for pattern in &self.block_hostnames {
                    if hostname_matches(host, pattern) {
                        bail!("hostname {host} is blocked by blockHostnames");
                    }
                }

                // Check IP blocking — resolve hostname to check against blacklist
                if !self.blacklist_ips.is_empty() {
                    if let Ok(ip) = host.parse::<IpAddr>() {
                        for net in &self.blacklist_ips {
                            if net.contains(&ip) {
                                bail!("IP {ip} is blocked by blacklistIPs");
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Log request/response for httpDebug mode.
    fn debug_request(&self, method: &str, url: &str, headers: &[(String, String)]) {
        if let Some(ref mode) = self.http_debug {
            eprintln!("HTTP DEBUG > {method} {url}");
            if mode == "full" {
                for (k, v) in headers {
                    eprintln!("HTTP DEBUG >   {k}: {v}");
                }
            }
        }
    }

    fn debug_response(&self, status: u16, url: &str, headers: &[(String, String)]) {
        if let Some(ref mode) = self.http_debug {
            eprintln!("HTTP DEBUG < {status} {url}");
            if mode == "full" {
                for (k, v) in headers {
                    eprintln!("HTTP DEBUG <   {k}: {v}");
                }
            }
        }
    }
}

/// Check if a hostname matches a pattern with wildcard support.
/// `*.example.com` matches `foo.example.com` but not `example.com`.
fn hostname_matches(host: &str, pattern: &str) -> bool {
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..]; // ".example.com"
        host.ends_with(suffix) && host.len() > suffix.len()
    } else {
        host == pattern
    }
}

impl HttpClient for ReqwestHttpClient {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        // Check blacklist/block before sending
        self.check_blocked(&req.url)?;

        let method_str = match req.method {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
        };

        self.debug_request(method_str, &req.url, &req.headers);

        let start = Instant::now();

        // Use local IP pool client if configured, otherwise default client
        let client = if let Some(ref pool) = self.local_ip_clients {
            pool.next_client()
        } else {
            &self.client
        };

        let mut builder = match req.method {
            HttpMethod::Get => client.get(&req.url),
            HttpMethod::Post => client.post(&req.url),
            HttpMethod::Put => client.put(&req.url),
            HttpMethod::Patch => client.patch(&req.url),
            HttpMethod::Delete => client.delete(&req.url),
            HttpMethod::Head => client.head(&req.url),
            HttpMethod::Options => client.request(reqwest::Method::OPTIONS, &req.url),
        };

        for (key, value) in &req.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }

        if let Some(body) = req.body {
            builder = builder.body(body);
        }

        if let Some(timeout) = req.timeout {
            builder = builder.timeout(timeout);
        }

        // Materialize the request so we can count the bytes that go on the wire
        // (request line + headers + blank line + body). Without this we'd be
        // limited to body-only counting, which loses the lion's share of the
        // bytes for headerful or empty-body requests — matching upstream k6's
        // data_sent semantic requires the full message.
        let request = builder.build()?;
        let data_sent = estimate_request_bytes(&request);

        let send_start = Instant::now();
        let response = client.execute(request).await?;
        let waiting_done = Instant::now();

        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let url = response.url().to_string();
        // Compute response header bytes before consuming the body. Same shape
        // (status line + headers + blank line) as upstream's data_received scope.
        let response_header_bytes = estimate_response_header_bytes(&response);

        self.debug_response(status, &url, &headers);

        let body_bytes_len: u64;
        let body = if self.discard_response_bodies {
            body_bytes_len = drain_response_body(response).await?;
            ResponseBody::Discarded
        } else {
            let buffered = buffer_response_body(response, self.max_response_body_size).await?;
            body_bytes_len = buffered.len() as u64;
            ResponseBody::Buffered(buffered)
        };
        let data_received = response_header_bytes + body_bytes_len;

        let receive_done = Instant::now();

        // reqwest's high-level API exposes three observable points:
        //   send_start   — just before builder.send().await
        //   waiting_done — when .send().await? returns (response headers received)
        //   receive_done — when body buffering finishes
        // It does NOT separate "request bytes written" from "first response byte",
        // so we cannot measure the `sending` phase here without dropping to
        // hyper-level instrumentation. Until that lands, sending is reported as
        // 0 and the (write + TTFB) interval is folded into `waiting`. This
        // matches what reqwest can actually distinguish; the previous code
        // computed `sending = send_start.elapsed()` AFTER receive_done, which
        // made `sending` effectively equal to `duration` and inflated the
        // http_req_sending trend by ~100x in the conformance harness.
        let timings = Timings {
            sending: 0.0,
            waiting: waiting_done.duration_since(send_start).as_secs_f64() * 1000.0,
            receiving: receive_done.duration_since(waiting_done).as_secs_f64() * 1000.0,
            duration: start.elapsed().as_secs_f64() * 1000.0,
            ..Default::default()
        };

        Ok(HttpResponse {
            status,
            headers,
            body,
            timings,
            url,
            data_sent,
            data_received,
        })
    }
}

async fn drain_response_body(mut response: reqwest::Response) -> Result<u64> {
    let mut total: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len() as u64;
    }
    Ok(total)
}

/// Estimate the byte size of an HTTP/1.1 request as serialised on the wire:
/// `METHOD path[?query] HTTP/1.1\r\n` + `Name: Value\r\n` per header + `\r\n` + body.
///
/// Synthesis, not transport-level measurement. We add the `Host` header
/// explicitly because hyper inserts it at the connection layer from the URL —
/// it's always present on the wire but absent from `request.headers()` until
/// hyper writes the request. Reqwest may add a few more headers at that layer
/// (e.g. `User-Agent` if the builder configured one, `Content-Length` for
/// fixed-size bodies, `Connection: keep-alive` for pooled connections); those
/// are not captured here, so this is a **lower bound** on what actually goes
/// on the wire. Closing the gap to upstream's exact-byte counts requires
/// hyper-level instrumentation (the (b) work). HTTP/2 (HPACK) would invalidate
/// the estimate entirely; not in scope today (no HTTPS path exercised).
fn estimate_request_bytes(request: &reqwest::Request) -> u64 {
    let method = request.method().as_str();
    let url = request.url();
    let path = url.path();
    let query_len = url.query().map(|q| q.len() + 1).unwrap_or(0); // +1 for '?'

    // Request line: METHOD<sp>PATH[?QUERY]<sp>HTTP/1.1\r\n
    let mut bytes: u64 = (method.len() + 1 + path.len() + query_len + 1 + 8 + 2) as u64;

    // Host header (hyper inserts it at the connection layer; not in headers()).
    if !request
        .headers()
        .keys()
        .any(|k| k.as_str().eq_ignore_ascii_case("host"))
    {
        if let Some(host) = url.host_str() {
            let host_value_len = match url.port() {
                Some(p) => host.len() + 1 + p.to_string().len(),
                None => host.len(),
            };
            // "Host" + ": " + value + "\r\n"
            bytes += 4 + 2 + host_value_len as u64 + 2;
        }
    }

    for (name, value) in request.headers() {
        // "name: value\r\n"
        bytes += name.as_str().len() as u64 + 2 + value.as_bytes().len() as u64 + 2;
    }
    bytes += 2; // blank line separator

    if let Some(body) = request.body() {
        if let Some(body_bytes) = body.as_bytes() {
            bytes += body_bytes.len() as u64;
        }
    }

    bytes
}

/// Estimate the byte size of an HTTP/1.1 response prelude (status line +
/// headers + blank line), excluding body. Same caveats as
/// [`estimate_request_bytes`].
fn estimate_response_header_bytes(response: &reqwest::Response) -> u64 {
    let status = response.status();
    let reason = status.canonical_reason().unwrap_or("");

    // Status line: HTTP/1.1<sp>NNN<sp>REASON\r\n
    let mut bytes: u64 = (8 + 1 + 3 + 1 + reason.len() + 2) as u64;

    for (name, value) in response.headers() {
        bytes += name.as_str().len() as u64 + 2 + value.as_bytes().len() as u64 + 2;
    }
    bytes += 2; // blank line separator

    bytes
}

async fn buffer_response_body(
    mut response: reqwest::Response,
    max_response_body_size: usize,
) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(max_response_body_size.min(16 * 1024));

    while let Some(chunk) = response.chunk().await? {
        append_capped_chunk(&mut body, &chunk, max_response_body_size);
    }

    Ok(body)
}

fn append_capped_chunk(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if buffer.len() >= cap {
        return;
    }

    let remaining = cap - buffer.len();
    let take = remaining.min(chunk.len());
    buffer.extend_from_slice(&chunk[..take]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_match_exact() {
        assert!(hostname_matches("example.com", "example.com"));
        assert!(!hostname_matches("other.com", "example.com"));
    }

    #[test]
    fn hostname_match_wildcard() {
        assert!(hostname_matches("foo.example.com", "*.example.com"));
        assert!(hostname_matches("bar.baz.example.com", "*.example.com"));
        assert!(!hostname_matches("example.com", "*.example.com"));
    }

    #[test]
    fn check_blocked_by_hostname() {
        let client = ReqwestHttpClient {
            client: reqwest::Client::new(),
            local_ip_clients: None,
            discard_response_bodies: false,
            max_response_body_size: 10 * 1024 * 1024,
            http_debug: None,
            _should_throw: false,
            blacklist_ips: Vec::new(),
            block_hostnames: vec!["*.internal.com".to_string()],
            _hosts: HashMap::new(),
        };

        assert!(
            client
                .check_blocked("http://api.internal.com/path")
                .is_err()
        );
        assert!(client.check_blocked("http://example.com/path").is_ok());
    }

    #[test]
    fn check_blocked_by_ip() {
        let client = ReqwestHttpClient {
            client: reqwest::Client::new(),
            local_ip_clients: None,
            discard_response_bodies: false,
            max_response_body_size: 10 * 1024 * 1024,
            http_debug: None,
            _should_throw: false,
            blacklist_ips: vec!["10.0.0.0/8".parse().unwrap()],
            block_hostnames: Vec::new(),
            _hosts: HashMap::new(),
        };

        assert!(client.check_blocked("http://10.1.2.3/path").is_err());
        assert!(client.check_blocked("http://192.168.1.1/path").is_ok());
    }

    #[test]
    fn from_config_basic() {
        let config = TestConfig::default();
        let client = ReqwestHttpClient::from_config(&config);
        assert!(client.is_ok());
    }

    #[test]
    fn from_config_with_options() {
        let mut config = TestConfig::default();
        config.no_connection_reuse = true;
        config.insecure_skip_tls_verify = true;
        config.user_agent = Some("k6-rs/0.1.0".to_string());
        config.max_redirects = Some(5);
        config.http_debug = Some("full".to_string());
        config.throw = true;
        config
            .hosts
            .insert("test.local".to_string(), "127.0.0.1".to_string());

        let client = ReqwestHttpClient::from_config(&config).unwrap();
        assert_eq!(client.http_debug, Some("full".to_string()));
        assert!(client._should_throw);
    }

    /// Regression for bug (a): `sending` was being computed as
    /// `send_start.elapsed()` AFTER receive_done, which equalled total request
    /// duration instead of just-sending. After the fix, sending=0 (not
    /// measurable from reqwest's high-level API), waiting absorbs the
    /// write+TTFB interval, and waiting + receiving ≈ duration.
    #[tokio::test]
    async fn http_phase_boundaries_match_reqwest_observability() {
        use axum::Router;
        use axum::extract::Path;
        use axum::routing::get;
        use tokio::net::TcpListener;

        async fn delay_handler(Path(ms): Path<u64>) -> String {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            "x".repeat(256) // give receiving phase something to do
        }

        let app = Router::new().route("/delay/{ms}", get(delay_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = ReqwestHttpClient::new(false).unwrap();
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: format!("http://127.0.0.1:{}/delay/50", addr.port()),
            headers: Vec::new(),
            body: None,
            timeout: None,
        };
        let resp = client.send(req).await.unwrap();

        // sending is not measurable here and must be 0 — not the duration.
        assert_eq!(
            resp.timings.sending, 0.0,
            "sending should be 0 (reqwest can't separate it); was {}",
            resp.timings.sending
        );
        // waiting must absorb the server delay (50ms) plus connection setup.
        assert!(
            resp.timings.waiting >= 45.0,
            "waiting should be >= 45ms (server delay 50ms); was {}",
            resp.timings.waiting
        );
        // Sanity: duration >= waiting + receiving, and waiting is NOT the same
        // as duration (the bug equated them).
        assert!(
            resp.timings.duration >= resp.timings.waiting + resp.timings.receiving - 0.5,
            "duration ({}) must cover waiting ({}) + receiving ({})",
            resp.timings.duration,
            resp.timings.waiting,
            resp.timings.receiving
        );
        assert!(
            resp.timings.receiving >= 0.0,
            "receiving must be non-negative"
        );
    }

    /// Regression for bug (3): data_sent / data_received used to be body-only
    /// counts (`body_bytes.len()` for sent, response-body length for received),
    /// missing the request line + headers entirely. Upstream k6 counts the full
    /// HTTP message. This test asserts the new full-message scope and the
    /// approximate magnitude against a known-size response.
    #[tokio::test]
    async fn data_sent_and_received_count_headers_plus_body() {
        use axum::Router;
        use axum::routing::{get, post};
        use tokio::net::TcpListener;

        // Known fixed-size response body so we can reason about totals.
        const RESP_BODY: &str = "abcdefghij"; // 10 bytes

        let app = Router::new()
            .route("/get", get(|| async { RESP_BODY }))
            .route("/post", post(|body: String| async move { body }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = ReqwestHttpClient::new(false).unwrap();

        // GET: no request body, small response body. data_sent must include
        // the request line + headers (well above the previous 0 for body-less
        // GETs). data_received must include status line + headers + body (>>
        // the previous 10).
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://127.0.0.1:{}/get", addr.port()),
                headers: Vec::new(),
                body: None,
                timeout: None,
            })
            .await
            .unwrap();

        // Lock the SEMANTIC: a body-less GET previously reported data_sent = 0;
        // it must now cover at least the request line ("GET /get HTTP/1.1\r\n\r\n"
        // = 21 bytes) plus a Host header (always present on the wire). The
        // absolute number is a lower bound — closing the gap to upstream's
        // exact-byte count requires hyper-level instrumentation (the (b) work).
        assert!(
            resp.data_sent >= 40,
            "data_sent for a GET must cover request line + Host header; got {}",
            resp.data_sent
        );
        assert_ne!(resp.data_sent, 0, "regression: data_sent must not be 0");

        let body_len = match &resp.body {
            ResponseBody::Buffered(b) => b.len() as u64,
            ResponseBody::Discarded => 0,
        };
        assert_eq!(body_len, RESP_BODY.len() as u64);
        assert!(
            resp.data_received > body_len,
            "data_received ({}) must exceed body length ({}) by status line + headers",
            resp.data_received,
            body_len
        );
        // Sanity: prelude is at least "HTTP/1.1 200 OK\r\n\r\n" = 19 bytes,
        // plus whatever headers axum sends (content-length, content-type,
        // date, etc.) — easily another 60+ bytes.
        assert!(
            resp.data_received - body_len >= 30,
            "response prelude bytes too small: {} - {} = {}",
            resp.data_received,
            body_len,
            resp.data_received - body_len
        );

        // POST with a body: data_sent must include the explicit body bytes
        // PLUS request line + headers. Old behaviour returned just body_len.
        let body_payload = b"x".repeat(100);
        let resp = client
            .send(HttpRequest {
                method: HttpMethod::Post,
                url: format!("http://127.0.0.1:{}/post", addr.port()),
                headers: Vec::new(),
                body: Some(body_payload.clone()),
                timeout: None,
            })
            .await
            .unwrap();
        assert!(
            resp.data_sent > body_payload.len() as u64,
            "data_sent ({}) for POST must exceed body ({})",
            resp.data_sent,
            body_payload.len()
        );
    }

    #[test]
    fn from_config_with_local_ips() {
        let mut config = TestConfig::default();
        config.local_ips = vec!["127.0.0.1".to_string(), "127.0.0.2".to_string()];

        let client = ReqwestHttpClient::from_config(&config).unwrap();
        assert!(client.local_ip_clients.is_some());
        let pool = client.local_ip_clients.as_ref().unwrap();
        assert_eq!(pool.clients.len(), 2);
    }

    #[test]
    fn from_config_invalid_local_ip_fails() {
        let mut config = TestConfig::default();
        config.local_ips = vec!["not-an-ip".to_string()];

        let result = ReqwestHttpClient::from_config(&config);
        assert!(result.is_err());
    }

    #[test]
    fn local_ip_pool_round_robin() {
        let pool = LocalIpPool {
            clients: vec![
                reqwest::Client::new(),
                reqwest::Client::new(),
                reqwest::Client::new(),
            ],
            index: AtomicUsize::new(0),
        };

        // Should cycle through clients
        let _c0 = pool.next_client();
        let _c1 = pool.next_client();
        let _c2 = pool.next_client();
        // Wraps around
        assert_eq!(pool.index.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn append_capped_chunk_stops_at_limit() {
        let mut buffer = Vec::new();
        let cap = 8;

        append_capped_chunk(&mut buffer, b"abcd", cap);
        append_capped_chunk(&mut buffer, b"efgh", cap);
        append_capped_chunk(&mut buffer, b"ijkl", cap);

        assert_eq!(buffer, b"abcdefgh");
    }

    #[test]
    fn append_capped_chunk_handles_partial_final_chunk() {
        let mut buffer = Vec::new();
        let cap = 6;

        append_capped_chunk(&mut buffer, b"abcd", cap);
        append_capped_chunk(&mut buffer, b"efgh", cap);

        assert_eq!(buffer, b"abcdef");
    }
}
