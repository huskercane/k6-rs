//! End-to-end: real HTTP server + the coroutine pool executors. Exercises
//! http.get + check through the production path (`k6_js::pool`) against a live
//! axum server.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use k6_core::backpressure::Backpressure;
use k6_core::executor::arrival::ArrivalCurve;
use k6_core::metrics::BuiltinMetrics;
use k6_js::coroutine_vu::VuSpec;
use k6_js::http_client::ReqwestHttpClient;
use k6_js::pool;

/// Start a test HTTP server, returns the base URL.
async fn start_test_server(request_count: Arc<AtomicU32>) -> String {
    let app = Router::new().route(
        "/api/test",
        get(move || {
            let count = request_count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                axum::Json(serde_json::json!({ "status": "ok" }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    url
}

/// Start a slow test server (adds delay per request).
async fn start_slow_server(delay: Duration, request_count: Arc<AtomicU32>) -> String {
    let app = Router::new().route(
        "/api/test",
        get(move || {
            let count = request_count.clone();
            let delay = delay;
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(delay).await;
                axum::Json(serde_json::json!({ "status": "ok" }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://127.0.0.1:{}", addr.port());

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    url
}

/// RAW k6 script (the coroutine VU prepares it) that GETs + checks the endpoint.
fn make_script(base_url: &str) -> String {
    format!(
        r#"
export default function() {{
    const res = http.get('{base_url}/api/test');
    check(res, {{
        'status is 200': (r) => r.status === 200,
    }});
}}
"#
    )
}

#[tokio::test]
async fn constant_vus_against_real_server() {
    let request_count = Arc::new(AtomicU32::new(0));
    let base_url = start_test_server(request_count.clone()).await;
    let spec = VuSpec::script(make_script(&base_url));

    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(10);
    let metrics = BuiltinMetrics::new();

    // The pool owns loop threads + blocks; run it off the async executor.
    let summary = tokio::task::spawn_blocking(move || {
        pool::run_constant_vus(
            spec,
            3,
            Duration::from_millis(500),
            client,
            bp,
            metrics,
            CancellationToken::new(),
            Duration::from_millis(200),
        )
    })
    .await
    .unwrap();

    assert!(
        summary.iterations_completed >= 3,
        "expected >= 3 iterations, got {}",
        summary.iterations_completed
    );
    assert_eq!(summary.iterations_dropped, 0);

    let requests = request_count.load(Ordering::Relaxed);
    assert!(requests >= 3, "expected >= 3 HTTP requests to server, got {requests}");
}

#[tokio::test]
async fn arrival_rate_with_slow_server_causes_drops() {
    let request_count = Arc::new(AtomicU32::new(0));
    // Server responds in 200ms.
    let base_url = start_slow_server(Duration::from_millis(200), request_count.clone()).await;
    let spec = VuSpec::script(make_script(&base_url));

    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(4);
    let metrics = BuiltinMetrics::new();

    // Only 2 VUs but 50/s against a 200ms server — the idle set empties ⇒ drops.
    let curve = ArrivalCurve::constant(50, Duration::from_secs(1), Duration::from_millis(500));
    let summary = tokio::task::spawn_blocking(move || {
        pool::run_arrival_rate(
            spec,
            2,
            curve,
            client,
            bp,
            metrics,
            CancellationToken::new(),
            Duration::from_millis(300),
        )
    })
    .await
    .unwrap();

    assert!(
        summary.iterations_dropped > 0,
        "expected dropped iterations with a slow server, got 0 ({summary:?})"
    );
    assert!(summary.iterations_completed > 0);
}

#[tokio::test]
async fn check_results_are_correct() {
    let request_count = Arc::new(AtomicU32::new(0));
    let base_url = start_test_server(request_count.clone()).await;

    let spec = VuSpec::script(format!(
        r#"
export default function() {{
    const res = http.get('{base_url}/api/test');
    const passed = check(res, {{
        'status is 200': (r) => r.status === 200,
        'body has status': (r) => JSON.parse(r.body).status === 'ok',
    }});
    if (!passed) {{
        throw new Error('checks failed');
    }}
}}
"#
    ));

    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(10);
    let metrics = BuiltinMetrics::new();

    let summary = tokio::task::spawn_blocking(move || {
        pool::run_constant_vus(
            spec,
            1,
            Duration::from_millis(300),
            client,
            bp,
            metrics,
            CancellationToken::new(),
            Duration::from_millis(200),
        )
    })
    .await
    .unwrap();

    // A thrown check-failure would land in `errored`, not `completed`.
    assert!(
        summary.iterations_completed >= 1,
        "expected >= 1 successful iteration ({summary:?})"
    );
    assert_eq!(summary.iterations_errored, 0, "checks should pass ({summary:?})");
}
