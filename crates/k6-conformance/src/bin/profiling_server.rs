//! Zero-latency profiling target: the standalone twin of the in-process axum
//! server the (deleted, #6) `http_bridge` bench used. One route, tiny JSON
//! body, so a `k6-rs run profiling/bench.js` profile isolates fixed
//! per-iteration overhead — not target-server or network cost.
//!
//! Run with: `cargo run --release -p k6-conformance --bin profiling_server [port]`
//!
//! **High-VU tuning.** The handler is trivial, so the only thing that limits how
//! many VUs this can serve is *connection admission*, not request work. Two knobs
//! matter under a 1000+-VU stampede (all VUs connecting at once, no think time):
//!
//!  1. **Listen backlog.** tokio's `TcpListener::bind` uses a backlog of 1024;
//!     1000 VUs connecting simultaneously overflow it and the kernel silently
//!     drops SYNs → clients see `status:0` (a saturated-target artifact, NOT an
//!     engine result). We build the socket by hand and request a large backlog.
//!     NOTE: the kernel caps the effective backlog at `net.core.somaxconn`
//!     (often 4096 on modern Linux); raise it if you still see `status:0`:
//!       `sudo sysctl -w net.core.somaxconn=16384`
//!  2. **Worker threads.** A single accept/serve thread bottlenecks the accept
//!     loop; we pin the runtime to all cores so accept + keep-alive serving fan
//!     out. (`#[tokio::main]` already defaults to this, but we make it explicit
//!     alongside the socket tuning so the intent is one place.)
//!
//! Even so: for a CLEAN concurrency flamegraph, drive a VU count the target can
//! actually keep at `status:200` (verify `http_req_failed ~0%` first). A run that
//! is 97% `status:0` profiles connection-error handling, not the http.get path.

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use tokio::net::TcpSocket;

/// Requested listen backlog. Capped by `net.core.somaxconn` at the kernel; large
/// enough to absorb a full 1000+-VU connect stampede when somaxconn allows.
const LISTEN_BACKLOG: u32 = 16_384;

fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::args().nth(1).map(|p| p.parse()).transpose()?.unwrap_or(8877);

    // Explicit multi-threaded runtime across all cores so the accept loop and
    // keep-alive request serving don't bottleneck on one thread under high VU
    // counts (this is the `#[tokio::main]` default, made explicit next to the
    // socket tuning it pairs with).
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = Router::new().route("/", get(|| async { r#"{"ok":true}"# }));

        // Build the listening socket by hand so we can set a large backlog +
        // SO_REUSEADDR — `TcpListener::bind` hard-codes a 1024 backlog that a
        // 1000-VU connect stampede overflows into `status:0` on the client.
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let socket = TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.bind(addr)?;
        let listener = socket.listen(LISTEN_BACKLOG)?;

        println!(
            "profiling target on http://{}/ (backlog req={}, capped at net.core.somaxconn)",
            listener.local_addr()?,
            LISTEN_BACKLOG
        );
        axum::serve(listener, app).await?;
        anyhow::Ok(())
    })
}
