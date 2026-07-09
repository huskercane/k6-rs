//! Per-request bridge + HTTP client micro-benchmark.
//!
//! Locks the fixed per-iteration overhead that dominates on low-latency /
//! high-RPS-per-VU targets. Against the zero-latency in-process server below,
//! `run_iteration` cost is essentially: the QuickJS default-function call, the
//! JS->Rust request bridge (headers/tags marshalling, per-request
//! allocations), the HTTP client round-trip, and metric recording. Network
//! latency is ~0, so this isolates the *fixed overhead* the HTTP perf work
//! targets (see HTTP_PERF_PLAN.md).
//!
//! Two backends are benchmarked so the hyper-vs-reqwest A/B stays
//! regression-locked: `hyper` is the production default; `reqwest` is the
//! `K6RS_HTTP_CLIENT=reqwest` fallback. Measured 2026-07-09 (release), hyper
//! carried ~20µs/req less `http_req_duration` than reqwest on localhost.
//!
//! Run with: `cargo bench -p k6-js --bench http_bridge`
//!
//! NOTE: absolute numbers here are not directly comparable to the end-to-end
//! `k6-rs run` figures — there is no executor scheduling, output plumbing, or
//! process startup. The value is the *delta* between runs and between the two
//! clients, which is what a regression gate needs.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::net::TcpListener;

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::VirtualUser;
use k6_js::http_client::ReqwestHttpClient;
use k6_js::hyper_client::HyperHttpClient;
use k6_js::vu::{self, QuickJsVu};

/// Spawn a zero-latency in-process server that returns a tiny JSON body,
/// mirroring the `srv.go` repro in HTTP_PERF_PLAN.md.
async fn start_server() -> String {
    let app = Router::new().route("/", get(|| async { r#"{"ok":true}"# }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the server a moment to accept connections.
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://127.0.0.1:{}/", addr.port())
}

fn bench_script(base_url: &str) -> String {
    vu::prepare_script(&format!(
        "export default function() {{ http.get('{base_url}'); }}"
    ))
}

fn bench_http_bridge(c: &mut Criterion) {
    // A dedicated multi-thread runtime hosts the server and drives the client
    // futures. The benchmark closure runs on criterion's calling thread (NOT a
    // runtime worker), so the VU's internal `Handle::block_on` is a legal
    // block-from-outside-the-runtime — exactly how the executors call it via
    // `spawn_blocking`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    let base_url = rt.block_on(start_server());
    let src = bench_script(&base_url);
    // Generous permits: backpressure is not what we are measuring here.
    let bp = Backpressure::new(64);

    let mut group = c.benchmark_group("http_get_iteration");

    {
        let client = Arc::new(HyperHttpClient::new());
        let mut hyper_vu = QuickJsVu::new_with_http_and_metrics(
            0,
            &src,
            &[],
            handle.clone(),
            client,
            bp.clone(),
            Some(BuiltinMetrics::new()),
        )
        .unwrap();
        // Warm the connection pool so we measure steady-state reuse, not the
        // first-request DNS/TCP handshake.
        hyper_vu.run_iteration().unwrap();
        group.bench_function("hyper", |b| {
            b.iter(|| hyper_vu.run_iteration().unwrap())
        });
    }

    {
        let client = Arc::new(ReqwestHttpClient::new(false).unwrap());
        let mut reqwest_vu = QuickJsVu::new_with_http_and_metrics(
            0,
            &src,
            &[],
            handle.clone(),
            client,
            bp.clone(),
            Some(BuiltinMetrics::new()),
        )
        .unwrap();
        reqwest_vu.run_iteration().unwrap();
        group.bench_function("reqwest", |b| {
            b.iter(|| reqwest_vu.run_iteration().unwrap())
        });
    }

    group.finish();
}

criterion_group!(benches, bench_http_bridge);
criterion_main!(benches);
