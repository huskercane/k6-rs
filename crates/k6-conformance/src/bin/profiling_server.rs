//! Zero-latency profiling target: the standalone twin of the in-process axum
//! server the (deleted, #6) `http_bridge` bench used. One route, tiny JSON
//! body, so a `k6-rs run profiling/bench.js` profile isolates fixed
//! per-iteration overhead — not target-server or network cost.
//!
//! Run with: `cargo run --release -p k6-conformance --bin profiling_server [port]`

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(8877);

    let app = Router::new().route("/", get(|| async { r#"{"ok":true}"# }));
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("profiling target on http://{}/", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
