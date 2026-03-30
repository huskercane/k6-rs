use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::traits::{RunSummary, VirtualUser};
use crate::vu_pool::VuPool;

/// An executor whose VU count is controlled at runtime via a REST API.
///
/// Starts with `initial_vus` VUs running. A REST API on `localhost:6565` allows
/// adjusting VUs at runtime:
/// - `GET  /v1/status` — returns current status JSON
/// - `PATCH /v1/status` — set `{"vus": N}` to adjust active VU count
///
/// Runs until duration expires (if set), `PATCH {"stopped": true}`, or Ctrl+C.
pub struct ExternallyControlledExecutor<V: VirtualUser + 'static> {
    pool: Arc<VuPool<V>>,
    initial_vus: u32,
    max_vus: u32,
    duration: Duration,
}

impl<V: VirtualUser + 'static> ExternallyControlledExecutor<V> {
    pub fn new(pool: Arc<VuPool<V>>, initial_vus: u32, max_vus: u32, duration: Duration) -> Self {
        Self {
            pool,
            initial_vus,
            max_vus,
            duration,
        }
    }

    pub async fn run(self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let total_iterations = Arc::new(AtomicU64::new(0));
        let active_vus = Arc::new(AtomicU32::new(self.initial_vus));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let max_vus = self.max_vus;

        // Spawn the REST API server
        let api_active = Arc::clone(&active_vus);
        let api_stopped = Arc::clone(&stopped);
        let api_cancel = cancel.clone();
        let api_iters = Arc::clone(&total_iterations);

        let api_handle = tokio::spawn(async move {
            if let Err(e) = run_api_server(api_active, api_stopped, api_cancel, api_iters, max_vus).await {
                eprintln!("  externally-controlled API error: {e}");
            }
        });

        // Spawn VU worker tasks — each grabs from pool when active
        let mut handles = Vec::new();
        for vu_idx in 0..self.max_vus {
            let pool = Arc::clone(&self.pool);
            let iterations = Arc::clone(&total_iterations);
            let active = Arc::clone(&active_vus);
            let cancel = cancel.clone();
            let stopped = Arc::clone(&stopped);
            let deadline = if self.duration.is_zero() {
                None
            } else {
                Some(start + self.duration)
            };

            let handle = tokio::task::spawn_blocking(move || {
                loop {
                    if cancel.is_cancelled() || stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(d) = deadline {
                        if Instant::now() >= d {
                            break;
                        }
                    }

                    // Only run if this VU index is within the active count
                    if vu_idx >= active.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }

                    if let Some(mut guard) = pool.try_acquire_owned() {
                        let vu = guard.vu_mut();
                        match vu.run_iteration() {
                            Ok(_) => {
                                iterations.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                eprintln!("VU iteration error: {e}");
                            }
                        }
                        vu.reset();
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all VU tasks
        for handle in handles {
            let _ = handle.await;
        }

        // Cancel API server
        api_handle.abort();

        let elapsed = start.elapsed();

        Ok(RunSummary {
            iterations_completed: total_iterations.load(Ordering::Relaxed),
            iterations_dropped: 0,
            duration: elapsed,
        })
    }
}

/// Simple REST API for controlling the executor.
///
/// Listens on `localhost:6565`:
/// - `GET  /v1/status` → `{"vus": N, "vus-max": M, "stopped": false, "running": true, "tainted": false}`
/// - `PATCH /v1/status` → body `{"vus": N}` or `{"stopped": true}`
async fn run_api_server(
    active_vus: Arc<AtomicU32>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
    iterations: Arc<AtomicU64>,
    max_vus: u32,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:6565").await?;
    eprintln!("  externally-controlled API listening on http://127.0.0.1:6565");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accept = listener.accept() => {
                let (mut stream, _addr) = accept?;
                let active = Arc::clone(&active_vus);
                let stopped = Arc::clone(&stopped);
                let cancel = cancel.clone();
                let iters = Arc::clone(&iterations);

                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 { return; }

                    let request = String::from_utf8_lossy(&buf[..n]);
                    let (method, path) = parse_http_request(&request);

                    let response = match (method, path) {
                        ("GET", "/v1/status") => {
                            let vus = active.load(Ordering::Relaxed);
                            let iters = iters.load(Ordering::Relaxed);
                            let body = serde_json::json!({
                                "vus": vus,
                                "vus-max": max_vus,
                                "stopped": stopped.load(Ordering::Relaxed),
                                "running": true,
                                "tainted": false,
                                "iterations": iters,
                            });
                            http_response(200, &body.to_string())
                        }
                        ("PATCH", "/v1/status") => {
                            // Extract JSON body from request
                            if let Some(body) = extract_body(&request) {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
                                    if let Some(new_vus) = val.get("vus").and_then(|v| v.as_u64()) {
                                        let new_vus = (new_vus as u32).min(max_vus);
                                        active.store(new_vus, Ordering::Relaxed);
                                    }
                                    if let Some(true) = val.get("stopped").and_then(|v| v.as_bool()) {
                                        stopped.store(true, Ordering::Relaxed);
                                        cancel.cancel();
                                    }
                                    let body = serde_json::json!({
                                        "vus": active.load(Ordering::Relaxed),
                                        "vus-max": max_vus,
                                        "stopped": stopped.load(Ordering::Relaxed),
                                    });
                                    http_response(200, &body.to_string())
                                } else {
                                    http_response(400, r#"{"error":"invalid JSON"}"#)
                                }
                            } else {
                                http_response(400, r#"{"error":"missing body"}"#)
                            }
                        }
                        _ => http_response(404, r#"{"error":"not found"}"#),
                    };

                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        }
    }

    Ok(())
}

fn parse_http_request(request: &str) -> (&str, &str) {
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        (parts[0], parts[1])
    } else {
        ("", "")
    }
}

fn extract_body(request: &str) -> Option<&str> {
    request.find("\r\n\r\n").map(|idx| &request[idx + 4..])
}

fn http_response(status: u16, body: &str) -> String {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Unknown",
    };
    format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::IterationResult;

    struct MockVu;
    impl VirtualUser for MockVu {
        fn run_iteration(&mut self) -> Result<IterationResult> {
            std::thread::sleep(Duration::from_millis(10));
            Ok(IterationResult { duration: Duration::from_millis(10) })
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn parse_http_request_line() {
        let (method, path) = parse_http_request("GET /v1/status HTTP/1.1\r\nHost: localhost\r\n");
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/status");
    }

    #[test]
    fn extract_body_from_request() {
        let req = "PATCH /v1/status HTTP/1.1\r\nContent-Length: 10\r\n\r\n{\"vus\": 5}";
        let body = extract_body(req);
        assert_eq!(body, Some("{\"vus\": 5}"));
    }

    #[test]
    fn http_response_format() {
        let resp = http_response(200, r#"{"ok":true}"#);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Type: application/json"));
        assert!(resp.ends_with(r#"{"ok":true}"#));
    }

    #[tokio::test]
    async fn externally_controlled_basic() {
        let vus: Vec<MockVu> = (0..4).map(|_| MockVu).collect();
        let pool = Arc::new(VuPool::new(vus));

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Cancel after 300ms
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel_clone.cancel();
        });

        let executor = ExternallyControlledExecutor::new(
            pool,
            2, // initial 2 VUs
            4, // max 4
            Duration::ZERO, // no duration limit
        );

        let summary = executor.run(cancel).await.unwrap();
        assert!(summary.iterations_completed > 0);
        assert!(summary.duration < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn externally_controlled_with_duration() {
        let vus: Vec<MockVu> = (0..2).map(|_| MockVu).collect();
        let pool = Arc::new(VuPool::new(vus));

        let executor = ExternallyControlledExecutor::new(
            pool,
            1,
            2,
            Duration::from_millis(200),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();
        assert!(summary.iterations_completed > 0);
        assert!(summary.duration >= Duration::from_millis(180));
    }
}
