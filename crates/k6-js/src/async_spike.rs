//! Phase 0 spike for the async-runtime migration (see `ASYNC_RUNTIME_PLAN.md`).
//!
//! **THROWAWAY.** This module exists only to de-risk the epic's core
//! assumptions before Phase 1 touches production code. It is gated behind the
//! `async-spike` cargo feature and is deleted once Phase 1 lands. Nothing here
//! is wired into the VU/executor path.
//!
//! It proves three things the plan gates Phase 0 on:
//!
//! - **(a) mechanism** — a Rust `async` host fn resolves a JS `Promise` from an
//!   awaited future (`asyncDouble` below).
//! - **(b) pool-of-loops + isolation** — two VUs, *each with its own isolated
//!   `AsyncRuntime` + 64 MB memory limit*, run on **one OS thread** (one
//!   `LocalSet`, one current-thread runtime) and make **overlapping** async
//!   calls. Wall-clock proves the I/O overlaps; a cross-runtime global check
//!   proves the heaps are isolated (collapsing to one runtime per thread would
//!   break this — the rejected design).
//! - **(c) the driver** — named explicitly: one `rt.drive()` future per VU,
//!   `spawn_local`'d onto the shared `LocalSet`. That future is what advances
//!   each runtime's spawned host futures + JS microtasks while the VU awaits.
//!
//! Run: `cargo test -p k6-js --features async-spike async_spike -- --nocapture`

// Throwaway spike: the exhibits below are exercised by the module's own tests.
#![allow(dead_code)]

use std::time::Duration;

use anyhow::{Context, Result};
use rquickjs::prelude::Async;
use rquickjs::{AsyncContext, AsyncRuntime, Function, Promise};

/// Per-VU memory limit, identical to the sync runtime today (`runtime.rs`).
/// The whole point of pool-of-loops is that this stays *per VU* even though
/// many VUs share a thread.
const VU_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Build one isolated VU runtime: its own `AsyncRuntime` (own heap), its own
/// 64 MB cap, and the async host fns registered. This is the per-VU unit that
/// pool-of-loops hosts many of, on few threads.
async fn build_vu_runtime() -> Result<(AsyncRuntime, AsyncContext)> {
    let rt = AsyncRuntime::new().context("failed to create AsyncRuntime")?;
    rt.set_memory_limit(VU_MEMORY_LIMIT).await;
    rt.set_max_stack_size(256 * 1024).await;

    let ctx = AsyncContext::full(&rt)
        .await
        .context("failed to create AsyncContext")?;

    ctx.with(|ctx| -> Result<()> {
        let globals = ctx.globals();

        // (a) An async host fn: awaits a real Rust future, resolves a JS promise.
        // `Async` adapts an `FnMut -> Future` into a JS function returning a
        // Promise; the future is spawned onto the runtime and driven by (c).
        let double = Function::new(
            ctx.clone(),
            Async(|n: f64| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                n * 2.0
            }),
        )?
        .with_name("asyncDouble")?;
        globals.set("asyncDouble", double)?;

        // A no-arg async delay, used to show intra-VU Promise.all concurrency.
        let delay = Function::new(
            ctx.clone(),
            Async(|| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }),
        )?
        .with_name("asyncDelay")?;
        globals.set("asyncDelay", delay)?;

        Ok(())
    })
    .await?;

    Ok((rt, ctx))
}

/// Run one VU "iteration": evaluate a script that awaits async host fns, and
/// return the numeric result the promise resolves to.
///
/// The awaited promise only makes progress because a `drive()` task for this
/// runtime is live on the same `LocalSet` (the named driver, (c)).
async fn run_iteration(ctx: &AsyncContext, script: &str) -> Result<f64> {
    let script = script.to_string();
    ctx.async_with(async move |ctx| -> Result<f64> {
        let promise: Promise = ctx
            .eval(script)
            .map_err(|e| anyhow::anyhow!("eval error: {e:?}"))?;
        let value: f64 = promise
            .into_future()
            .await
            .map_err(|e| anyhow::anyhow!("promise rejected: {e:?}"))?;
        Ok(value)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use tokio::task::LocalSet;

    /// Build the single-threaded current-thread runtime that a pool-of-loops
    /// executor *thread* would own. Everything below runs on ONE such thread.
    fn loop_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// (a) An async host fn resolves a JS promise from an awaited Rust future.
    #[test]
    fn async_host_fn_resolves_promise() {
        let rt = loop_runtime();
        let local = LocalSet::new();
        local.block_on(&rt, async {
            let (qjs_rt, ctx) = build_vu_runtime().await.unwrap();
            // (c) the driver — advances spawned host futures + microtasks.
            let driver = tokio::task::spawn_local(qjs_rt.drive());

            let result = run_iteration(&ctx, "asyncDouble(21)").await.unwrap();
            assert_eq!(result, 42.0, "awaited async host fn must resolve the promise");

            // Intra-VU concurrency: Promise.all of two 100ms delays must overlap.
            let start = Instant::now();
            run_iteration(
                &ctx,
                "Promise.all([asyncDelay(), asyncDelay(), asyncDelay()]).then(() => 7)",
            )
            .await
            .unwrap();
            let elapsed = start.elapsed();
            assert!(
                elapsed < Duration::from_millis(250),
                "3x100ms delays should overlap (~100ms), got {elapsed:?} — not concurrent"
            );

            driver.abort();
        });
    }

    /// (b) Two VUs, each an isolated `AsyncRuntime` + 64 MB cap, on ONE thread,
    /// making overlapping async calls — and heaps stay isolated.
    #[test]
    fn two_isolated_vus_one_thread_overlap() {
        let rt = loop_runtime();
        let local = LocalSet::new();
        local.block_on(&rt, async {
            let (rt1, ctx1) = build_vu_runtime().await.unwrap();
            let (rt2, ctx2) = build_vu_runtime().await.unwrap();

            // (c) one driver per VU runtime, both on this single LocalSet/thread.
            let d1 = tokio::task::spawn_local(rt1.drive());
            let d2 = tokio::task::spawn_local(rt2.drive());

            // Heap isolation: a global set in VU1 must NOT be visible in VU2.
            // This is the property a shared runtime would destroy.
            ctx1.with(|ctx| ctx.eval::<(), _>("globalThis.leak = 'vu1'").unwrap())
                .await;
            let leaked_into_vu2: bool = ctx2
                .with(|ctx| ctx.eval::<bool, _>("typeof globalThis.leak !== 'undefined'").unwrap())
                .await;
            assert!(
                !leaked_into_vu2,
                "per-VU runtimes must be isolated — VU2 saw VU1's global"
            );

            // Overlap across VUs: each VU awaits a 100ms async call. Run both
            // iterations concurrently on the one thread. Serial would be ~200ms;
            // cooperative multiplexing should land near ~100ms.
            let start = Instant::now();
            let t1 = {
                let ctx1 = ctx1.clone();
                tokio::task::spawn_local(async move {
                    run_iteration(&ctx1, "asyncDouble(1)").await.unwrap()
                })
            };
            let t2 = {
                let ctx2 = ctx2.clone();
                tokio::task::spawn_local(async move {
                    run_iteration(&ctx2, "asyncDouble(2)").await.unwrap()
                })
            };
            let (r1, r2) = tokio::join!(t1, t2);
            let elapsed = start.elapsed();
            assert_eq!(r1.unwrap(), 2.0);
            assert_eq!(r2.unwrap(), 4.0);
            assert!(
                elapsed < Duration::from_millis(180),
                "two VUs' 100ms calls should overlap on one thread (~100ms), got {elapsed:?}"
            );

            // Bare-interpreter heap of these two runtimes — deliberately NOT
            // reported as a VU floor: they have bootstrapped nothing but two
            // trivial host fns. The representative number (full API bootstrap)
            // is measured in `bootstrapped_vu_heap_floor` below.
            let m1 = rt1.memory_usage().await;
            let m2 = rt2.memory_usage().await;
            let bare = (m1.malloc_size + m2.malloc_size) / 2;
            eprintln!(
                "[async-spike] bare-interpreter heap ≈ {bare} bytes ({:.2} MB) — \
                 NOT a VU; see bootstrapped_vu_heap_floor for the real floor",
                bare as f64 / 1e6,
            );

            d1.abort();
            d2.abort();
        });
    }

    /// Corrected per-VU heap floor: measure a runtime that has run the SAME
    /// dependency-free API bootstrap `vu.rs` runs (console/encoding/crypto/
    /// execution/html/secrets/csv/fs/streams/webcrypto) plus a representative
    /// script — not the bare interpreter the overlap test happens to leave.
    ///
    /// Still a *partial* floor: it excludes the http/ws/grpc/metrics handlers
    /// (which need a client/handle/registry to wire) and any real user script.
    /// The point is to stop quoting an empty-interpreter number as "the VU
    /// heap" — the bootstrap alone is multiples of it.
    #[test]
    fn bootstrapped_vu_heap_floor() {
        let rt = loop_runtime();
        let local = LocalSet::new();
        local.block_on(&rt, async {
            let qjs_rt = AsyncRuntime::new().unwrap();
            qjs_rt.set_memory_limit(VU_MEMORY_LIMIT).await;
            let ctx = AsyncContext::full(&qjs_rt).await.unwrap();

            let bare = qjs_rt.memory_usage().await.malloc_size;

            ctx.with(|ctx| -> Result<()> {
                // Exactly the dependency-free set from vu.rs's bootstrap.
                crate::api::encoding::register(&ctx)?;
                crate::api::crypto::register(&ctx)?;
                crate::api::execution::register(&ctx)?;
                crate::api::html::register(&ctx)?;
                crate::api::secrets::register(&ctx)?;
                crate::api::csv::register(&ctx)?;
                crate::api::fs::register(&ctx)?;
                crate::api::streams::register(&ctx)?;
                crate::api::webcrypto::register(&ctx)?;
                // A small but non-trivial user script (closures + retained state).
                ctx.eval::<(), _>(
                    r#"
                    globalThis.__state = { seen: [], n: 0 };
                    globalThis.__k6_default = function () {
                        __state.n++;
                        __state.seen.push(crypto.sha256('x' + __state.n));
                        if (__state.seen.length > 100) __state.seen.shift();
                    };
                    "#,
                )
                .map_err(|e| anyhow::anyhow!("bootstrap script error: {e:?}"))?;
                Ok(())
            })
            .await
            .unwrap();

            qjs_rt.run_gc().await;
            let bootstrapped = qjs_rt.memory_usage().await.malloc_size;
            PER_VU_HEAP.store(bootstrapped as u64, Ordering::Relaxed);

            eprintln!(
                "[async-spike] bootstrapped VU heap ≈ {bootstrapped} bytes ({:.2} MB), \
                 {:.1}x the bare interpreter ({bare} bytes). Partial JS-heap floor at \
                 7900 VUs ≈ {:.1} MB — EXCLUDES http/ws/grpc/metrics handlers + real \
                 user scripts, so a lower bound, not a budget. Real RSS is Phase 2's gate.",
                bootstrapped as f64 / 1e6,
                bootstrapped as f64 / bare.max(1) as f64,
                (bootstrapped as f64 * 7900.0) / 1e6,
            );

            assert!(
                bootstrapped > bare,
                "bootstrap must grow the heap; got {bootstrapped} <= {bare}"
            );
        });
    }

    static PER_VU_HEAP: AtomicU64 = AtomicU64::new(0);

    /// The linchpin of Phase 1's big-bang rationale, proven rather than
    /// asserted: once a VU runs on a tokio runtime, the `handle.block_on(...)`
    /// that http/ws/grpc/sleep use TODAY panics — there is no sync-shim escape,
    /// so those host fns MUST convert to async in the same step as VU-on-loop.
    /// (`block_in_place` is not an out either: it needs a multi-thread runtime.)
    #[test]
    #[should_panic(expected = "within a runtime")]
    fn handle_block_on_panics_once_vu_runs_on_a_runtime() {
        let rt = loop_runtime();
        rt.block_on(async {
            let handle = tokio::runtime::Handle::current();
            // Exactly the shape of api/http.rs today, but now from *within* the
            // loop the VU would run on:
            handle.block_on(async { 1 + 1 });
        });
    }

    /// Isolation of the memory *limit*, not just the heap: exhausting VU1's
    /// allocation must not affect VU2 (proves the 64 MB cap is per-runtime, the
    /// property pool-of-loops must keep vs. a per-thread-group budget).
    #[test]
    fn per_vu_memory_limit_is_isolated() {
        let rt = loop_runtime();
        let local = LocalSet::new();
        local.block_on(&rt, async {
            // Small caps so we can trip one cheaply.
            let rt1 = AsyncRuntime::new().unwrap();
            rt1.set_memory_limit(2 * 1024 * 1024).await;
            let ctx1 = AsyncContext::full(&rt1).await.unwrap();

            let rt2 = AsyncRuntime::new().unwrap();
            rt2.set_memory_limit(64 * 1024 * 1024).await;
            let ctx2 = AsyncContext::full(&rt2).await.unwrap();

            // VU1 tries to allocate way past its 2 MB cap → must error/throw.
            let vu1_hit_limit: bool = ctx1
                .with(|ctx| {
                    ctx.eval::<(), _>("let a = []; for (let i=0;i<5_000_000;i++) a.push(i);")
                        .is_err()
                })
                .await;
            assert!(vu1_hit_limit, "VU1 should have hit its own 2 MB limit");

            // VU2, with its own 64 MB heap, is unaffected by VU1's exhaustion.
            let vu2_ok: i32 = ctx2
                .with(|ctx| ctx.eval::<i32, _>("let a=[]; for(let i=0;i<1000;i++) a.push(i); a.length").unwrap())
                .await;
            assert_eq!(vu2_ok, 1000, "VU2's heap must be unaffected by VU1");
        });
    }
}
