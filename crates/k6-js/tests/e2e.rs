use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use k6_core::backpressure::Backpressure;
use k6_core::executor::constant_arrival_rate::ConstantArrivalRateExecutor;
use k6_core::executor::constant_vus::ConstantVusExecutor;
use k6_core::vu_pool::VuPool;
use k6_js::http_client::ReqwestHttpClient;
use k6_js::vu::{self, QuickJsVu};

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

    // Give server a moment to start
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

fn make_script(base_url: &str) -> String {
    let raw = format!(
        r#"
export default function() {{
    const res = http.get('{base_url}/api/test');
    check(res, {{
        'status is 200': (r) => r.status === 200,
    }});
}}
"#
    );
    vu::prepare_script(&raw)
}

fn create_vu(
    id: u32,
    script: &str,
    handle: tokio::runtime::Handle,
    client: Arc<ReqwestHttpClient>,
    bp: Backpressure,
) -> QuickJsVu {
    QuickJsVu::new_with_http(id, script, &[], handle, client, bp).unwrap()
}

#[tokio::test]
async fn constant_vus_against_real_server() {
    let request_count = Arc::new(AtomicU32::new(0));
    let base_url = start_test_server(request_count.clone()).await;
    let script = make_script(&base_url);

    let handle = tokio::runtime::Handle::current();
    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(10);

    // Create VUs inside spawn_blocking (QuickJS needs this)
    let vus = tokio::task::spawn_blocking({
        let script = script.clone();
        let client = client.clone();
        let bp = bp.clone();
        let handle = handle.clone();
        move || {
            (0..3)
                .map(|i| create_vu(i, &script, handle.clone(), client.clone(), bp.clone()))
                .collect::<Vec<_>>()
        }
    })
    .await
    .unwrap();

    let executor = ConstantVusExecutor::new(vus, Duration::from_millis(500));
    let summary = executor.run(CancellationToken::new()).await.unwrap();

    assert!(
        summary.iterations_completed >= 3,
        "expected >= 3 iterations, got {}",
        summary.iterations_completed
    );
    assert_eq!(summary.iterations_dropped, 0);

    let requests = request_count.load(Ordering::Relaxed);
    assert!(
        requests >= 3,
        "expected >= 3 HTTP requests to server, got {requests}"
    );
}

#[tokio::test]
async fn arrival_rate_with_slow_server_causes_drops() {
    let request_count = Arc::new(AtomicU32::new(0));
    // Server responds in 200ms
    let base_url = start_slow_server(Duration::from_millis(200), request_count.clone()).await;
    let script = make_script(&base_url);

    let handle = tokio::runtime::Handle::current();
    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(4);

    // Only 2 VUs but requesting 50/s — will definitely exhaust pool
    let vus = tokio::task::spawn_blocking({
        let script = script.clone();
        let client = client.clone();
        let bp = bp.clone();
        let handle = handle.clone();
        move || {
            (0..2)
                .map(|i| create_vu(i, &script, handle.clone(), client.clone(), bp.clone()))
                .collect::<Vec<_>>()
        }
    })
    .await
    .unwrap();

    let pool = Arc::new(VuPool::new(vus));
    let executor = ConstantArrivalRateExecutor::new(
        pool.clone(),
        50,
        Duration::from_secs(1),
        Duration::from_millis(500),
    );

    let summary = executor.run(CancellationToken::new()).await.unwrap();

    // Should have dropped some iterations (2 VUs can't sustain 50/s with 200ms response)
    assert!(
        summary.iterations_dropped > 0,
        "expected dropped iterations with slow server, got 0"
    );
    assert!(summary.iterations_completed > 0);

    // Pool capacity unchanged — memory guarantee
    assert_eq!(pool.capacity(), 2);
    // All VUs returned
    assert_eq!(pool.available_count(), 2);
}

#[tokio::test]
async fn check_results_are_correct() {
    let request_count = Arc::new(AtomicU32::new(0));
    let base_url = start_test_server(request_count.clone()).await;

    let raw = format!(
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
    );
    let script = vu::prepare_script(&raw);

    let handle = tokio::runtime::Handle::current();
    let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
    let bp = Backpressure::new(10);

    let vus = tokio::task::spawn_blocking({
        let script = script.clone();
        let client = client.clone();
        let bp = bp.clone();
        let handle = handle.clone();
        move || vec![create_vu(0, &script, handle, client, bp)]
    })
    .await
    .unwrap();

    let executor = ConstantVusExecutor::new(vus, Duration::from_millis(300));
    let summary = executor.run(CancellationToken::new()).await.unwrap();

    // All iterations should succeed (no thrown errors)
    assert!(
        summary.iterations_completed >= 1,
        "expected >= 1 successful iteration"
    );
}
