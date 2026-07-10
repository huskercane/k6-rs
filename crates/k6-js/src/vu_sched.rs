//! The VU scheduler + yield primitive (production, un-gated). Extracted from the
//! `vu_loop` harness once its shape was proven, and now consumed for real by the
//! yielding http host fns (`api::http`) and the coroutine VU.
//!
//! ## Invariants (proven in the `vu_loop` harness, enforced here by structure)
//! - **I1** — [`drive_vu`] owns only `(coroutines, futures)` + a client; it never
//!   borrows a `Context`, and is metrics-free ([`run_op`] returns an owned
//!   `Result<HttpResponse>`; all metric recording is coroutine-side).
//! - **I2 (queue-don't-resolve)** — a future completing while the VU is parked
//!   mid sync `http.get` (borrow held) only QUEUES its result in
//!   [`VuShared::completed`]; the coroutine's driver loop resolves promises.
//! - **I3 (spawn_local-only)** — VU futures are thread-pinned; the `unsafe Send`
//!   on [`Shared`]/[`YielderPtr`] is an FFI lie (the `parallel` rquickjs feature
//!   needs host-fn closures `Send`), not a license to move threads. Spawn only
//!   via [`spawn_vu`]; a `debug_assert` backstops a stray `tokio::spawn`.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use futures_util::stream::{FuturesUnordered, StreamExt};

use k6_core::backpressure::Backpressure;
use k6_core::traits::{HttpClient, HttpRequest, HttpResponse};

use crate::api::http::ResponseCallback;

pub(crate) type OpId = u64;

/// Per-request info an async http op needs at *resolution* time (driver-loop
/// side), carried in a side-map keyed by op id so the scheduler stays ignorant of
/// it (I1) — it only moves the owned request/response.
pub(crate) struct AsyncMeta {
    pub(crate) method: String,
    pub(crate) user_tags: Vec<(String, String)>,
    pub(crate) response_callback: ResponseCallback,
}

/// A blocking op the scheduler runs on the VU's behalf. The scheduler is the ONLY
/// place futures are created (I1).
pub(crate) enum HostOp {
    Http(HttpRequest),
    Sleep(Duration),
}

/// The owned outcome of a `HostOp`. Carries `Result` so a transport failure
/// reaches `finish_http_response`'s Err path (status:0 / classify_error /
/// failure-tagged metric) — the bucket that silently rots if only the happy path
/// is tested.
pub(crate) enum OpDone {
    Http(anyhow::Result<HttpResponse>),
    Slept,
}

#[derive(Default)]
pub(crate) struct VuShared {
    pub(crate) next_op: OpId,
    /// Async ops registered but not yet resolved — see the fire-and-forget /
    /// event-loop-drained iteration-end rule in the coroutine driver loop.
    pub(crate) outstanding: u64,
    /// Async ops awaiting the scheduler to make futures.
    pub(crate) registered: Vec<(OpId, HostOp)>,
    /// Completed async results — drained ONLY by the driver loop (I2).
    pub(crate) completed: VecDeque<(OpId, OpDone)>,
    /// Resolution-time metadata for async http ops (see [`AsyncMeta`]).
    pub(crate) async_meta: HashMap<OpId, AsyncMeta>,
}

/// `Send` newtype over the per-VU `Rc` (I3 FFI lie; single-threaded in practice).
#[derive(Clone, Default)]
pub(crate) struct Shared(pub(crate) Rc<RefCell<VuShared>>);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register an async op (the host-fn consumer side): allocate an id, count it
    /// outstanding, queue it for the scheduler, and stash its resolution meta.
    /// Returns the op id (the JS side keys its resolver by it).
    pub(crate) fn register_async(&self, op: HostOp, meta: Option<AsyncMeta>) -> OpId {
        let mut s = self.0.borrow_mut();
        let id = s.next_op;
        s.next_op += 1;
        s.outstanding += 1;
        s.registered.push((id, op));
        if let Some(m) = meta {
            s.async_meta.insert(id, m);
        }
        id
    }
}

pub(crate) enum Yield {
    /// Sync `http.get`/`sleep`: park (borrow held), run this op, resume directly.
    AwaitOne(HostOp),
    /// Driver loop: wait for a registered async op to complete.
    AwaitPending,
    /// One iteration finished + event loop drained. The long-lived coroutine
    /// parks here between iterations — a clean cancel point (no borrow held, no
    /// I/O in flight). The async driver resumes with `RunNext` or `Stop`.
    IterationBoundary,
}

pub(crate) enum Resume {
    Start,
    One(OpDone),
    Progressed,
    /// Run the next iteration (from the `IterationBoundary` park).
    RunNext,
    /// Tear down the VU (stop the iteration loop).
    Stop,
}

/// `Send` newtype over the `Yielder` pointer captured by host-fn closures.
#[derive(Clone, Copy)]
pub(crate) struct YielderPtr(pub(crate) *const Yielder<Resume, Yield>);
unsafe impl Send for YielderPtr {}
unsafe impl Sync for YielderPtr {}
impl YielderPtr {
    pub(crate) fn new(y: &Yielder<Resume, Yield>) -> Self {
        Self(y as *const _)
    }

    pub(crate) fn suspend(self, y: Yield) -> Resume {
        // SAFETY: same-thread, during this coroutine's own execution.
        unsafe { &*self.0 }.suspend(y)
    }

    /// The sync-blocking host-fn consumer side: yield the coroutine to run one op
    /// and resume with its owned result. Panics on a protocol mismatch (a bug in
    /// the driver, not a runtime condition).
    pub(crate) fn await_one(self, op: HostOp) -> OpDone {
        match self.suspend(Yield::AwaitOne(op)) {
            Resume::One(done) => done,
            _ => panic!("driver returned a non-One resume for AwaitOne"),
        }
    }
}

pub(crate) type VuCoroutine = Coroutine<Resume, Yield, ()>;

/// Run one op to its owned result. The ENTIRE I/O surface of the scheduler, and
/// the only place metrics MUST NOT appear (I1).
pub(crate) async fn run_op<C: HttpClient + 'static>(
    op: HostOp,
    client: &Arc<C>,
    bp: &Backpressure,
) -> OpDone {
    match op {
        HostOp::Http(req) => {
            let _permit = bp.acquire().await;
            OpDone::Http(client.send(req).await)
        }
        HostOp::Sleep(d) => {
            tokio::time::sleep(d).await;
            OpDone::Slept
        }
    }
}

/// The ONLY sanctioned way to run a VU (I3): `spawn_local`. The `unsafe Send` on
/// [`Shared`] would let `tokio::spawn` compile — and be UB.
///
/// This is the ASYNC driver (R1): it — not the sync per-iteration body inside the
/// coroutine — awaits tokio futures and services I/O yields.
pub(crate) fn spawn_vu<C, F>(
    coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
    control: F,
) -> tokio::task::JoinHandle<()>
where
    C: HttpClient + 'static,
    F: FnMut(u32) -> bool + 'static,
{
    tokio::task::spawn_local(drive_vu(coro, shared, client, bp, control))
}

/// Drive a VU coroutine's iteration loop. Owns the VU's futures; never touches
/// its `Context` (I1); metrics-free. Runs each op via [`run_op`].
///
/// `control(completed_iters) -> bool` is the RunNext/Stop hook, consulted at each
/// `IterationBoundary` (and once up front). A *hook*, not a baked count: #5's
/// executor supplies arrival/duration/graceful-stop, and the two-tier
/// cancellation plugs in here — prefer stopping at the boundary, reserving
/// `force_unwind` mid-op for a hard deadline.
pub(crate) async fn drive_vu<C, F>(
    mut coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
    mut control: F,
) where
    C: HttpClient + 'static,
    F: FnMut(u32) -> bool,
{
    type PendingFut = Pin<Box<dyn std::future::Future<Output = (OpId, OpDone)>>>;
    let mut pending: FuturesUnordered<PendingFut> = FuturesUnordered::new();
    // First resume runs iteration 0 unless control declines it up front.
    let mut resume = if control(0) { Resume::RunNext } else { Resume::Stop };
    let mut done_iters = 0u32;
    let home = std::thread::current().id();

    loop {
        debug_assert_eq!(
            std::thread::current().id(),
            home,
            "VU driver migrated threads — VUs are spawn_local-only (I3)"
        );
        let y = match coro.resume(resume) {
            CoroutineResult::Return(()) => return,
            CoroutineResult::Yield(y) => y,
        };

        // Pull newly-registered async ops into our own FuturesUnordered.
        {
            let mut s = shared.0.borrow_mut();
            let ops: Vec<(OpId, HostOp)> = s.registered.drain(..).collect();
            drop(s);
            for (op, hop) in ops {
                let client = Arc::clone(&client);
                let bp = bp.clone();
                pending.push(Box::pin(async move { (op, run_op(hop, &client, &bp).await) }));
            }
        }

        match y {
            Yield::AwaitOne(op) => {
                // Park on the sync op. While parked, KEEP draining async
                // completions — but only QUEUE them (I2), never resolve.
                //
                // #5 cancellation seam: this stays a `select!` loop precisely so
                // a `_ = cancel.cancelled() => { coro.force_unwind(); return }`
                // arm threads in here without a retrofit — the sync op future is
                // simply dropped, and the coroutine unwinds cleanly.
                let sync = run_op(op, &client, &bp);
                tokio::pin!(sync);
                let done = loop {
                    tokio::select! {
                        r = &mut sync => break r,
                        Some((op, res)) = pending.next(), if !pending.is_empty() => {
                            shared.0.borrow_mut().completed.push_back((op, res));
                        }
                    }
                };
                resume = Resume::One(done);
            }
            Yield::AwaitPending => {
                debug_assert!(
                    !pending.is_empty(),
                    "AwaitPending with no in-flight futures — outstanding desynced from queues"
                );
                if let Some((op, res)) = pending.next().await {
                    shared.0.borrow_mut().completed.push_back((op, res));
                }
                resume = Resume::Progressed;
            }
            Yield::IterationBoundary => {
                // Iteration complete + event loop drained — the clean cancel
                // point (#5 prefers cancelling here over force_unwind).
                done_iters += 1;
                resume = if control(done_iters) {
                    Resume::RunNext
                } else {
                    Resume::Stop
                };
            }
        }
    }
}
