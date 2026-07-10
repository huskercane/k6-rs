//! Phase 0.5 spike: B2 stackful-coroutine suspension (see `ASYNC_RUNTIME_PLAN.md`).
//!
//! **THROWAWAY**, gated behind the `b2-spike` cargo feature. Proves the faithful
//! port of goja's goroutine model: a **synchronous** QuickJS host fn can *yield
//! the coroutine* to an async scheduler (instead of `block_on`), which awaits a
//! tokio future and resumes the coroutine with the result. `http.get()` stays
//! synchronous in the script — no `await`, no transpile — yet the OS thread
//! yields to other VUs. That is pool-of-loops without B1's unsound await-inject.
//!
//! What it proves (the Phase 0.5 gate):
//! - **mechanism** — a sync host fn (`__host_fetch`) called from plain sync JS
//!   suspends the coroutine; the scheduler awaits and resumes; JS continues.
//! - **cross-VU concurrency on ONE thread** — two VU coroutines whose sync
//!   fetches each take 100 ms overlap to ~100 ms wall-clock, not 200 ms.
//! - **(i) QuickJS stack checks survive the non-default stack base** — deep JS
//!   recursion inside the coroutine trips QuickJS's own `RangeError`, not a
//!   segfault. Works because sync `Context::with` calls `update_stack_top()` on
//!   entry, re-anchoring the 256 KB limit onto the 1 MB coroutine stack.
//! - **(ii) panic-unwind soundness** — a panic inside the coroutine propagates
//!   through `resume` and is catchable (no abort / UB across the stack switch).
//! - **(iii) contained unsafe** — the entire `unsafe` surface is one `YielderPtr`
//!   newtype (below); nothing else.
//!
//! Run: `cargo test -p k6-js --features b2-spike b2_spike -- --nocapture`

#![allow(dead_code)]

use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use rquickjs::Function;

use crate::runtime;

/// What a suspended coroutine asks the scheduler to perform (the "blocking" op).
#[derive(Debug)]
enum HostReq {
    Sleep(Duration),
    Fetch(String),
}

/// What the scheduler injects back when it resumes the coroutine.
#[derive(Debug)]
enum HostResp {
    Start,
    Slept,
    Fetched(String),
}

type VuCoroutine = Coroutine<HostResp, HostReq, i32>;

/// The ENTIRE unsafe surface of B2: a pointer to the coroutine's `Yielder`,
/// captured by the host-fn closures so a sync host fn deep in the JS call stack
/// can reach `suspend`.
///
/// SAFETY: the pointer is only ever dereferenced on the coroutine's own thread,
/// during that coroutine's own execution (host fns run only while the coroutine
/// is running). It never actually crosses threads. The `Send`/`Sync` impls exist
/// solely because the `parallel` rquickjs feature requires host-fn closures to be
/// `Send`; capturing a raw pointer directly would otherwise be rejected. This is
/// the one contained `unsafe` B2 concentrates in exchange for B1's unbounded,
/// spread-across-every-user-script risk.
#[derive(Clone, Copy)]
struct YielderPtr(*const Yielder<HostResp, HostReq>);
unsafe impl Send for YielderPtr {}
unsafe impl Sync for YielderPtr {}

impl YielderPtr {
    fn suspend(self, req: HostReq) -> HostResp {
        // SAFETY: see the type-level note — same-thread, during coroutine run.
        unsafe { &*self.0 }.suspend(req)
    }
}

/// Build one VU as a stackful coroutine running a SYNCHRONOUS QuickJS iteration.
/// The QuickJS runtime is created ON the coroutine stack, so its stack checks
/// anchor there. `__host_fetch`/`__host_sleep` look synchronous to the script but
/// yield the coroutine to the scheduler.
fn vu_coroutine(vu_id: u32, script: String) -> VuCoroutine {
    Coroutine::new(move |yielder: &Yielder<HostResp, HostReq>, _first: HostResp| -> i32 {
        let yp = YielderPtr(yielder as *const _);

        let rt = runtime::create_runtime().expect("runtime");
        let ctx = runtime::create_context(&rt).expect("context");

        ctx.with(|ctx| -> i32 {
            let g = ctx.globals();
            g.set("__VU", vu_id).unwrap();

            // Synchronous-looking blocking fetch that actually yields the loop.
            g.set(
                "__host_fetch",
                Function::new(ctx.clone(), move |url: String| -> String {
                    match yp.suspend(HostReq::Fetch(url)) {
                        HostResp::Fetched(body) => body,
                        other => panic!("unexpected resume for fetch: {other:?}"),
                    }
                })
                .unwrap(),
            )
            .unwrap();

            g.set(
                "__host_sleep",
                Function::new(ctx.clone(), move |ms: f64| {
                    match yp.suspend(HostReq::Sleep(Duration::from_millis(ms as u64))) {
                        HostResp::Slept => {}
                        other => panic!("unexpected resume for sleep: {other:?}"),
                    }
                })
                .unwrap(),
            )
            .unwrap();

            match ctx.eval::<i32, _>(script.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[b2-spike] JS error: {e:?}");
                    -1
                }
            }
        })
    })
}

/// Drive one VU coroutine to completion on the async scheduler, awaiting each
/// yielded blocking op on tokio. THIS is where a sync `http.get` becomes a real
/// non-blocking await without the script ever seeing a promise.
async fn drive_vu(mut coro: VuCoroutine) -> i32 {
    let mut input = HostResp::Start;
    loop {
        match coro.resume(input) {
            CoroutineResult::Yield(req) => {
                input = match req {
                    HostReq::Sleep(d) => {
                        tokio::time::sleep(d).await;
                        HostResp::Slept
                    }
                    HostReq::Fetch(url) => {
                        // Stand-in for real hyper I/O: a fixed per-request delay.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        HostResp::Fetched(format!("body-of:{url}"))
                    }
                };
            }
            CoroutineResult::Return(r) => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::task::LocalSet;

    fn loop_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Mechanism: a synchronous host fn yields the coroutine; the scheduler
    /// awaits and resumes; the script uses the returned value synchronously.
    #[test]
    fn sync_host_call_yields_and_resumes() {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            // Note: NO `await` anywhere in this script — exactly like k6.
            let script = r#"
                var body = __host_fetch('http://example.com/' + __VU);
                if (body.indexOf('body-of:') !== 0) throw new Error('bad: ' + body);
                42
            "#;
            let result = drive_vu(vu_coroutine(7, script.to_string())).await;
            assert_eq!(result, 42, "sync-looking fetch must yield, resume, and return");
        });
    }

    /// Cross-VU concurrency on ONE OS thread: two VUs each do a 100 ms sync
    /// fetch. Serial would be ~200 ms; cooperative yielding lands near ~100 ms.
    #[test]
    fn two_vus_overlap_on_one_thread() {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let script = r#"
                __host_fetch('http://a/' + __VU);
                __host_fetch('http://b/' + __VU);
                __VU
            "#;
            // Two fetches per VU = ~200 ms serial per VU; two VUs overlapping
            // should still finish near ~200 ms (their fetches interleave), while
            // fully serial across VUs would be ~400 ms.
            let start = Instant::now();
            let t1 = tokio::task::spawn_local(drive_vu(vu_coroutine(1, script.to_string())));
            let t2 = tokio::task::spawn_local(drive_vu(vu_coroutine(2, script.to_string())));
            let (r1, r2) = tokio::join!(t1, t2);
            let elapsed = start.elapsed();
            assert_eq!(r1.unwrap(), 1);
            assert_eq!(r2.unwrap(), 2);
            assert!(
                elapsed < Duration::from_millis(320),
                "two VUs' sync fetches should overlap on one thread (~200 ms), got {elapsed:?}"
            );
        });
    }

    /// (i) QuickJS stack-overflow detection is intact on the coroutine stack:
    /// unbounded recursion trips a `RangeError` (caught in JS), not a segfault.
    #[test]
    fn quickjs_stack_check_intact_on_coroutine_stack() {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let script = r#"
                function rec(n) { return rec(n + 1) + 1; }
                var caught = 0;
                try { rec(0); } catch (e) { caught = 1; }
                caught
            "#;
            // If the 256 KB limit were measured against the wrong (main) stack
            // base, this would either segfault the process or never trip. A clean
            // return of 1 proves QuickJS detected the overflow on the coroutine
            // stack via update_stack_top() in Context::with.
            let result = drive_vu(vu_coroutine(0, script.to_string())).await;
            assert_eq!(result, 1, "QuickJS must catch stack overflow on the coroutine stack");
        });
    }

    /// (ii) A panic inside the coroutine propagates through `resume` and is
    /// catchable — no abort / UB across the stack switch.
    #[test]
    fn panic_unwinds_across_coroutine_switch() {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut coro: Coroutine<HostResp, HostReq, i32> =
                Coroutine::new(|_yielder, _first: HostResp| -> i32 {
                    panic!("boom inside coroutine");
                });
            coro.resume(HostResp::Start)
        }));
        assert!(
            outcome.is_err(),
            "a panic in the coroutine body must unwind catchably across the switch"
        );
    }

    // --- 1b-gate: m1/m2 — the sync-path promise substrate the unified model
    // rests on. No AsyncContext. Separate `ctx.with` blocks MODEL the borrow
    // release that a real coroutine yield produces (the QuickJS runtime state —
    // pending promises + job queue — is untouched by parking the Rust stack,
    // already shown by the stack-check test above).

    /// (m1) On the SYNC path, an `async` default fn's top-level `await` returns
    /// control to the caller with a *pending* promise (borrow released), and a
    /// scheduler-held resolver + `execute_pending_job()` settles it later.
    #[test]
    fn m1_sync_async_fn_await_returns_pending_then_settles() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        // Block 1: mint a promise we control from Rust, run an async fn that
        // awaits it. eval RETURNS here (borrow released) with the fn suspended.
        ctx.with(|ctx| {
            let (trigger, resolve, _reject) = ctx.promise().unwrap();
            ctx.globals().set("__trigger", trigger).unwrap();
            ctx.globals().set("__resolve", resolve).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__result = null;
                (async function () {
                    let v = await __trigger;
                    globalThis.__result = v + 1;
                })();
                "#,
            )
            .unwrap();
        });

        // Borrow released; the await has NOT settled yet.
        ctx.with(|ctx| {
            let pending: bool = ctx.eval("__result === null").unwrap();
            assert!(pending, "top-level await must suspend, not block");
        });

        // Block 2: the "scheduler" settles the op via the stored resolver, then
        // the driver drains microtasks — the async fn resumes to completion.
        ctx.with(|ctx| {
            let resolve: rquickjs::Function = ctx.globals().get("__resolve").unwrap();
            resolve.call::<_, ()>((41,)).unwrap();
        });
        runtime::drain_pending_jobs(&rt);

        ctx.with(|ctx| {
            let result: i32 = ctx.globals().get("__result").unwrap();
            assert_eq!(result, 42, "resolver + job drain must complete the awaited fn");
        });
    }

    /// (m2) A sync `Context` mints promises (`ctx.promise()`), hands them to JS
    /// (`Promise.all`), and scheduler-held resolvers settle them later — **out
    /// of order, with a second op still in flight** — across borrow boundaries,
    /// without corrupting the job queue.
    #[test]
    fn m2_two_inflight_promises_settle_out_of_order() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        // Register two in-flight ops behind one Promise.all.
        ctx.with(|ctx| {
            let (p0, r0, _) = ctx.promise().unwrap();
            let (p1, r1, _) = ctx.promise().unwrap();
            let g = ctx.globals();
            g.set("__p0", p0).unwrap();
            g.set("__p1", p1).unwrap();
            g.set("__r0", r0).unwrap();
            g.set("__r1", r1).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__sum = null;
                Promise.all([__p0, __p1]).then(function (vs) {
                    globalThis.__sum = vs[0] + vs[1];
                });
                "#,
            )
            .unwrap();
        });

        // Settle op1 FIRST (out of order); op0 still in flight.
        ctx.with(|ctx| {
            let r1: rquickjs::Function = ctx.globals().get("__r1").unwrap();
            r1.call::<_, ()>((20,)).unwrap();
        });
        runtime::drain_pending_jobs(&rt);
        ctx.with(|ctx| {
            let not_yet: bool = ctx.eval("__sum === null").unwrap();
            assert!(not_yet, "Promise.all must NOT resolve while an op is in flight");
        });

        // Settle op0; now Promise.all completes, order-independent.
        ctx.with(|ctx| {
            let r0: rquickjs::Function = ctx.globals().get("__r0").unwrap();
            r0.call::<_, ()>((22,)).unwrap();
        });
        runtime::drain_pending_jobs(&rt);
        ctx.with(|ctx| {
            let sum: i32 = ctx.globals().get("__sum").unwrap();
            assert_eq!(sum, 42, "both ops settled -> Promise.all resolves, job queue intact");
        });
    }
}
