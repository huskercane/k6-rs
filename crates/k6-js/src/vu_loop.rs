//! Phase 1b construction (#2): the yield primitive + async scheduler + per-VU
//! driver loop, proven as a *composition* (see `ASYNC_RUNTIME_PLAN.md`).
//!
//! **Still gated behind `b2-spike`** (depends on `corosensei`, not yet wired to
//! production `QuickJsVu`/executors — that's #3/#5). This is where the unified
//! model stops being modeled and becomes real: a live coroutine yields to the
//! scheduler, a real tokio future completes there, the coroutine resumes, and
//! the driver loop resolves promises + drains jobs — in one run.
//!
//! ## Two invariants
//!
//! - **I1 (inherited):** the scheduler owns only `(coroutines, futures)`. It
//!   never borrows a VU's `Context`.
//! - **I2 (queue-don't-resolve):** a future can complete while its VU is parked
//!   deep inside a *different* sync `http.get` (the `ctx.with` borrow is held).
//!   Resolving its promise then would re-enter that `Context` = double-borrow UB.
//!   So completion only **queues** the result (Rust-side, per-VU); the
//!   coroutine's own driver loop drains the queue and calls resolvers, inside its
//!   own borrow. Two delivery modes over one yield primitive:
//!   - **sync `http.get`** — coroutine parked holding the borrow → resumed
//!     directly with the value.
//!   - **`asyncRequest`** — does not yield; registers a future, returns a pending
//!     promise; completion is stashed for the driver loop to apply later.
//!
//! Run: `cargo test -p k6-js --features b2-spike vu_loop -- --nocapture`

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use futures_util::stream::{FuturesUnordered, StreamExt};
use rquickjs::Function;

use crate::runtime;

type OpId = u64;

/// Per-VU state shared between the coroutine (host fns + driver loop) and the
/// scheduler. Single-threaded: the coroutine and the scheduler never run at the
/// same instant (cooperative), so no borrow is ever held across a suspend/resume.
#[derive(Default)]
struct VuShared {
    next_op: OpId,
    /// Async ops registered by a host fn, awaiting the scheduler to make futures.
    registered: Vec<(OpId, Duration)>,
    /// Completed async results — drained ONLY by the driver loop (I2).
    completed: VecDeque<(OpId, String)>,
}

/// `Send` newtype over the per-VU `Rc`. SAFETY: single-threaded — the VU, its
/// scheduler task, and its host fns all live on one loop thread and never send
/// this across threads. The bound exists only because the `parallel` rquickjs
/// feature requires host-fn closures to be `Send`. Contained-unsafe, same
/// discipline as the B2 spike's `YielderPtr`.
#[derive(Clone)]
struct Shared(Rc<RefCell<VuShared>>);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// What a suspended coroutine asks the scheduler for.
enum Yield {
    /// Sync `http.get`: park (borrow held), await this op, resume with its value.
    AwaitOne(Duration),
    /// Driver loop: nothing to run until a registered async op completes.
    AwaitPending,
}

/// What the scheduler injects on resume.
enum Resume {
    Start,
    SyncDone(String),
    Progressed,
}

/// `Send` newtype over the `Yielder` pointer captured by host-fn closures. Same
/// contained-unsafe rationale as `Shared`.
#[derive(Clone, Copy)]
struct YielderPtr(*const Yielder<Resume, Yield>);
unsafe impl Send for YielderPtr {}
unsafe impl Sync for YielderPtr {}
impl YielderPtr {
    fn suspend(self, y: Yield) -> Resume {
        // SAFETY: same-thread, during this coroutine's own execution.
        unsafe { &*self.0 }.suspend(y)
    }
}

type VuCoroutine = Coroutine<Resume, Yield, ()>;

/// Build one VU coroutine: a sync QuickJS context running `main` (an async fn),
/// with `syncFetch` (yields) and `asyncFetch` (registers + returns a promise),
/// wrapped in the driver loop that IS this VU's event loop. The final `__ret` is
/// written into `result` (the coroutine owns the `Context`, so the caller reads
/// the outcome through this slot).
fn vu_coroutine(script: String, shared: Shared, result: Rc<RefCell<String>>) -> VuCoroutine {
    Coroutine::new(move |yielder: &Yielder<Resume, Yield>, _start: Resume| {
        let yp = YielderPtr(yielder as *const _);
        let rt = runtime::create_runtime().expect("rt");
        let ctx = runtime::create_context(&rt).expect("ctx");

        // --- host fns + JS glue (one borrow) ---
        ctx.with(|ctx| {
            let g = ctx.globals();

            // sync http.get analog: yields the coroutine, resumes with the value.
            g.set(
                "__sync_fetch",
                Function::new(ctx.clone(), move |ms: f64| -> String {
                    match yp.suspend(Yield::AwaitOne(Duration::from_millis(ms as u64))) {
                        Resume::SyncDone(v) => v,
                        _ => panic!("bad resume for sync fetch"),
                    }
                })
                .unwrap(),
            )
            .unwrap();

            // asyncRequest analog: registers a future, returns an op id. Does NOT
            // yield — the promise is minted in JS and its resolver stashed.
            let sh = shared.clone();
            g.set(
                "__register_async",
                Function::new(ctx.clone(), move |ms: f64| -> f64 {
                    let mut s = sh.0.borrow_mut();
                    let id = s.next_op;
                    s.next_op += 1;
                    s.registered.push((id, Duration::from_millis(ms as u64)));
                    id as f64
                })
                .unwrap(),
            )
            .unwrap();

            ctx.eval::<(), _>(
                r#"
                globalThis.__resolvers = {};
                globalThis.syncFetch = function (ms) { return __sync_fetch(ms); };
                globalThis.asyncFetch = function (ms) {
                    var id = __register_async(ms);
                    return new Promise(function (resolve) { globalThis.__resolvers[id] = resolve; });
                };
                globalThis.__done = false;
                globalThis.__ret = "";
                "#,
            )
            .unwrap();
        });

        // --- run main to its first suspension point (borrow spans any sync
        // fetch's yield: the coroutine parks HOLDING this borrow) ---
        ctx.with(|ctx| {
            let wrapped = format!(
                r#"Promise.resolve((async function () {{ {script} }})())
                       .then(function (v) {{ globalThis.__ret = String(v); globalThis.__done = true; }});"#
            );
            if let Err(e) = ctx.eval::<(), _>(wrapped.as_bytes()) {
                eprintln!("[vu_loop] main error: {e:?}");
            }
        });

        // --- driver loop: settle completed ops + drain jobs (inside our own
        // borrows), then yield to let async ops progress. Resolvers are called
        // ONLY here (I2), never by the scheduler. ---
        loop {
            let done = ctx.with(|ctx| -> bool {
                let drained: Vec<(OpId, String)> =
                    shared.0.borrow_mut().completed.drain(..).collect();
                if !drained.is_empty() {
                    let resolvers: rquickjs::Object =
                        ctx.globals().get("__resolvers").unwrap();
                    for (op, val) in drained {
                        let key = op.to_string();
                        if let Ok(f) = resolvers.get::<_, Function>(key.as_str()) {
                            let _ = f.call::<_, ()>((val,));
                        }
                        let _ = resolvers.remove(key.as_str());
                    }
                }
                ctx.globals().get::<_, bool>("__done").unwrap_or(false)
            });

            runtime::drain_pending_jobs(&rt);

            let done = done
                || ctx.with(|ctx| ctx.globals().get::<_, bool>("__done").unwrap_or(false));
            if done {
                break;
            }

            // Nothing more to run synchronously; wait for an async op.
            let _ = yp.suspend(Yield::AwaitPending);
        }

        // Publish the outcome for the caller (the coroutine owns the Context).
        ctx.with(|ctx| {
            let r: String = ctx.globals().get("__ret").unwrap_or_default();
            *result.borrow_mut() = r;
        });
    })
}

/// Drive one VU coroutine to completion on the scheduler. Owns the VU's futures
/// (`pending`); never touches its `Context` (I1). On completion it only pushes
/// results to the shared queue (I2) — the driver loop resolves them.
async fn drive_vu(mut coro: VuCoroutine, shared: Shared) {
    type PendingFut = Pin<Box<dyn std::future::Future<Output = (OpId, String)>>>;
    let mut pending: FuturesUnordered<PendingFut> = FuturesUnordered::new();
    let mut resume = Resume::Start;

    loop {
        let y = match coro.resume(resume) {
            CoroutineResult::Return(()) => return,
            CoroutineResult::Yield(y) => y,
        };

        // Pull newly-registered async ops into our own FuturesUnordered.
        {
            let mut s = shared.0.borrow_mut();
            for (op, dur) in s.registered.drain(..) {
                pending.push(Box::pin(async move {
                    tokio::time::sleep(dur).await;
                    (op, format!("a{}", dur.as_millis()))
                }));
            }
        }

        match y {
            Yield::AwaitOne(dur) => {
                // Await the sync op. While parked, KEEP draining async
                // completions — but only QUEUE them (I2), never resolve.
                let sync = tokio::time::sleep(dur);
                tokio::pin!(sync);
                let val = loop {
                    tokio::select! {
                        _ = &mut sync => break format!("s{}", dur.as_millis()),
                        Some((op, res)) = pending.next(), if !pending.is_empty() => {
                            shared.0.borrow_mut().completed.push_back((op, res));
                        }
                    }
                };
                resume = Resume::SyncDone(val);
            }
            Yield::AwaitPending => {
                if let Some((op, res)) = pending.next().await {
                    shared.0.borrow_mut().completed.push_back((op, res));
                }
                resume = Resume::Progressed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::LocalSet;

    fn loop_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Run a VU coroutine to completion on a single-thread loop and return its
    /// `__ret`.
    fn run_vu_capturing(script: &str) -> String {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let shared = Shared(Rc::new(RefCell::new(VuShared::default())));
            let result = Rc::new(RefCell::new(String::new()));
            let coro = vu_coroutine(script.to_string(), shared.clone(), result.clone());
            drive_vu(coro, shared.clone()).await;
            let out = result.borrow().clone();
            out
        })
    }

    /// Composition + the queue-don't-resolve HAZARD in one run: `main` registers
    /// an async op (10 ms) then parks in a sync fetch (50 ms). The async op
    /// completes *during the park* (borrow held) — it must be QUEUED, not
    /// resolved (else double-borrow UB) — and resolved only after the sync fetch
    /// returns and the driver loop turns. A live coroutine, a real tokio future,
    /// resume, resolve, drain — all for real.
    #[test]
    fn composition_async_completes_during_sync_park() {
        // main awaits the async promise, so __ret reflects BOTH results and the
        // ordering the invariant produces.
        let script = r#"
            var asyncVal = null;
            var p = asyncFetch(10).then(function (v) { asyncVal = v; });
            var s = syncFetch(50);   // parks HOLDING the borrow; async(10ms) completes here
            await p;                 // resolved only by the driver loop, after the park
            return s + '|' + asyncVal;
        "#;
        let out = run_vu_capturing(script);
        assert_eq!(
            out, "s50|a10",
            "sync fetch resumes with its value; the async op queued during the park \
             resolves afterwards — no double-borrow"
        );
    }

    /// Pure sync path (north-star degenerate case): one sync fetch, no promises.
    #[test]
    fn composition_sync_only() {
        let out = run_vu_capturing("var s = syncFetch(30); return s;");
        assert_eq!(out, "s30");
    }

    /// Two async ops behind Promise.all overlap (multi-op case).
    #[test]
    fn composition_promise_all_overlap() {
        let script = r#"
            var vs = await Promise.all([asyncFetch(40), asyncFetch(40)]);
            return vs[0] + ',' + vs[1];
        "#;
        let out = run_vu_capturing(script);
        assert_eq!(out, "a40,a40");
    }
}
