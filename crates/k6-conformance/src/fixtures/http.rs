//! HTTP fixture: deterministic local server scripts hit via `K6_TEST_URL`.
//!
//! Spike scope: `/get` (returns 200 + JSON), `/status/:code`, `/delay/:ms`.
//! Fresh state per binary invocation — the runner calls `start()` once per
//! script-side, never sharing an instance between the two runners.

use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct HttpFixture {
    pub base_url: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl HttpFixture {
    pub async fn start() -> Result<Self> {
        let app = Router::new()
            .route("/get", get(get_handler))
            .route("/status/{code}", get(status_handler))
            .route("/delay/{ms}", get(delay_handler));

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        let handle = tokio::spawn(async move {
            let shutdown = async move { cancel_child.cancelled().await };
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
        });

        // Tiny settle so the listener is accept()-ready before scripts hit it.
        tokio::time::sleep(Duration::from_millis(20)).await;

        Ok(Self {
            base_url,
            cancel,
            handle,
        })
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.handle.await;
    }
}

async fn get_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

async fn status_handler(Path(code): Path<u16>) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST)
}

async fn delay_handler(Path(ms): Path<u64>) -> &'static str {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_serves_get_with_json_body() {
        let f = HttpFixture::start().await.unwrap();
        let resp = reqwest::get(format!("{}/get", f.base_url))
            .await
            .expect("GET /get");
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.expect("body text");
        let body: serde_json::Value = serde_json::from_str(&text).expect("JSON body");
        assert_eq!(body, serde_json::json!({"status": "ok"}));
        f.stop().await;
    }

    #[tokio::test]
    async fn fixture_returns_requested_status() {
        let f = HttpFixture::start().await.unwrap();
        for code in [200u16, 204, 404, 500] {
            let resp = reqwest::get(format!("{}/status/{code}", f.base_url))
                .await
                .expect("GET /status");
            assert_eq!(resp.status().as_u16(), code);
        }
        f.stop().await;
    }

    #[tokio::test]
    async fn fixture_delay_returns_after_sleep() {
        let f = HttpFixture::start().await.unwrap();
        let start = std::time::Instant::now();
        let resp = reqwest::get(format!("{}/delay/50", f.base_url))
            .await
            .expect("GET /delay");
        assert_eq!(resp.status(), 200);
        assert!(start.elapsed() >= Duration::from_millis(45));
        f.stop().await;
    }

    #[tokio::test]
    async fn each_start_yields_independent_instance() {
        let a = HttpFixture::start().await.unwrap();
        let b = HttpFixture::start().await.unwrap();
        assert_ne!(a.base_url, b.base_url, "ports must differ");
        // Both serve concurrently.
        let ra = reqwest::get(format!("{}/get", a.base_url)).await.unwrap();
        let rb = reqwest::get(format!("{}/get", b.base_url)).await.unwrap();
        assert_eq!(ra.status(), 200);
        assert_eq!(rb.status(), 200);
        a.stop().await;
        b.stop().await;
    }
}
