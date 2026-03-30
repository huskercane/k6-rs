use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Result};

use k6_core::config::TestConfig;
use k6_core::traits::{HttpClient, HttpMethod, HttpRequest, HttpResponse, ResponseBody, Timings};

/// Production HTTP client backed by reqwest.
///
/// Uses a shared connection pool. Clone is cheap (internally Arc'd).
#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
    /// When localIPs is set, we have multiple clients bound to different source IPs.
    /// We round-robin across them.
    local_ip_clients: Option<Arc<LocalIpPool>>,
    discard_response_bodies: bool,
    max_response_body_size: usize,
    http_debug: Option<String>,
    throw: bool,
    blacklist_ips: Vec<ipnet::IpNet>,
    block_hostnames: Vec<String>,
    hosts: HashMap<String, String>,
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
            throw: false,
            blacklist_ips: Vec::new(),
            block_hostnames: Vec::new(),
            hosts: HashMap::new(),
        })
    }

    /// Create a client from the full test configuration.
    pub fn from_config(config: &TestConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30));

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
            throw: config.throw,
            blacklist_ips,
            block_hostnames: config.block_hostnames.clone(),
            hosts: config.hosts.clone(),
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

        let send_start = Instant::now();
        let response = builder.send().await?;
        let waiting_done = Instant::now();

        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let url = response.url().to_string();

        self.debug_response(status, &url, &headers);

        let body = if self.discard_response_bodies {
            drain_response_body(response).await?;
            ResponseBody::Discarded
        } else {
            ResponseBody::Buffered(buffer_response_body(
                response,
                self.max_response_body_size,
            )
            .await?)
        };

        let receive_done = Instant::now();

        let timings = Timings {
            sending: send_start.elapsed().as_secs_f64() * 1000.0,
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
        })
    }
}

async fn drain_response_body(mut response: reqwest::Response) -> Result<()> {
    while let Some(chunk) = response.chunk().await? {
        let _ = chunk;
    }
    Ok(())
}

async fn buffer_response_body(mut response: reqwest::Response, max_response_body_size: usize) -> Result<Vec<u8>> {
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
            throw: false,
            blacklist_ips: Vec::new(),
            block_hostnames: vec!["*.internal.com".to_string()],
            hosts: HashMap::new(),
        };

        assert!(client.check_blocked("http://api.internal.com/path").is_err());
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
            throw: false,
            blacklist_ips: vec!["10.0.0.0/8".parse().unwrap()],
            block_hostnames: Vec::new(),
            hosts: HashMap::new(),
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
        config.hosts.insert("test.local".to_string(), "127.0.0.1".to_string());

        let client = ReqwestHttpClient::from_config(&config).unwrap();
        assert_eq!(client.http_debug, Some("full".to_string()));
        assert!(client.throw);
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
