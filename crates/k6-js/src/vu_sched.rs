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

// TRANSITIONAL: the whole graduation stack (this module → register_yielding_http
// → coroutine_vu) is reachable only from tests until #5 wires the coroutine VU
// into the executors. REMOVE this allow at #5 — then any genuinely dead scheduler
// item surfaces instead of being masked.
#![allow(dead_code)]

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

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
    /// A protocol-specific streaming op (ws/grpc): the op **carries its own
    /// future + I/O resource** (e.g. a `Receiver<WsEvent>`), so `run_op` just
    /// awaits it and NEVER learns the protocol or grows a registry param — the
    /// hard invariant. Output is type-erased; the consumer host fn downcasts. No
    /// backpressure permit (only `Http` gates on HTTP concurrency). Keeps
    /// `vu_sched` a generic yield engine with no compile edge on `api::ws`/`grpc`.
    Streaming(Pin<Box<dyn Future<Output = Box<dyn Any>>>>),
}

/// The owned outcome of a `HostOp`. Carries `Result` so a transport failure
/// reaches `finish_http_response`'s Err path (status:0 / classify_error /
/// failure-tagged metric) — the bucket that silently rots if only the happy path
/// is tested.
pub(crate) enum OpDone {
    Http(anyhow::Result<HttpResponse>),
    Slept,
    /// Type-erased streaming result (ws/grpc); the consumer downcasts to its own
    /// event/message type. Matching is inherently consumer-side.
    Stream(Box<dyn Any>),
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
    /// Sync `http.batch`: park, run ALL these ops CONCURRENTLY, resume with all
    /// results in input order. Neither AwaitOne (serializes) nor asyncRequest
    /// (returns a promise) — a synchronous wait-for-all.
    AwaitAll(Vec<HostOp>),
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
    /// Results of an `AwaitAll`, in input order.
    All(Vec<OpDone>),
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

    /// `http.batch` consumer side: yield the coroutine to run all `ops`
    /// concurrently, resume with all results in input order.
    pub(crate) fn await_all(self, ops: Vec<HostOp>) -> Vec<OpDone> {
        match self.suspend(Yield::AwaitAll(ops)) {
            Resume::All(v) => v,
            _ => panic!("driver returned a non-All resume for AwaitAll"),
        }
    }
}

pub(crate) type VuCoroutine = Coroutine<Resume, Yield, ()>;

/// Wraps the VU coroutine so a drop DURING an active panic does NOT force_unwind
/// it. `Coroutine::drop` force_unwinds a suspended coroutine; doing that while a
/// panic is already unwinding is a panic-during-panic → non-unwinding process
/// ABORT (taking every other VU on the loop thread). On the rare VU-panic path
/// (`drive_vu` unwinding a host-fn/client panic drops its local `coro`), we instead
/// LEAK the coroutine — its stack + Context — so the panic can reach the
/// task-boundary `catch_unwind` cleanly and isolate to just that VU. The leak is
/// one 512 KB stack on a bug path (reclaimed at process exit); the loop thread and
/// its other VUs survive. A DELIBERATE hard-cancel force_unwind (not during a
/// panic) goes through [`Self::force_unwind`] and is the normal validated path.
///
/// SCOPE: this handles the ASYNC-DRIVER unwind — a panic that originates in
/// `drive_vu` (a client/host-fn future panicking while the coroutine is parked at a
/// yield), a real Rust unwind. A *synchronous* host-fn panic inside `coro.resume()`
/// is a different surface: rquickjs traps panics at the callback boundary and turns
/// them into a JS exception, so they land in the `Errored` lane and never become a
/// Rust unwind through the C frames. If that containment is ever violated (a host
/// fn panicking through the C boundary), it is the SAME force_unwind-through-C
/// surface that #12's `unwind-safety` CI gate guards.
struct PanicSafeCoro(Option<VuCoroutine>);

impl PanicSafeCoro {
    fn resume(&mut self, input: Resume) -> CoroutineResult<Yield, ()> {
        self.0.as_mut().expect("coroutine present").resume(input)
    }
    fn force_unwind(&mut self) {
        if let Some(c) = self.0.as_mut() {
            c.force_unwind();
        }
    }
}

impl Drop for PanicSafeCoro {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Leak instead of force_unwinding mid-panic (which would abort).
            std::mem::forget(self.0.take());
        }
        // Otherwise the inner `Option<VuCoroutine>` drops normally → clean
        // force_unwind (the validated unwind-through-QuickJS-C path).
    }
}

/// The RunNext/Stop decision at each `IterationBoundary` (and once up front) — the
/// executor's control port. `next` MAY await: constant-vus returns a ready bool
/// (deadline/cancel check), while the arrival-rate coordinator path parks here
/// awaiting its per-VU dispatch channel (hence `&mut self` across `.await`, which
/// a bare `FnMut(u32) -> impl Future` signature cannot express).
///
/// A blanket impl covers every `FnMut(u32) -> bool` (constant-vus + tests), so
/// only the coordinator supplies a real struct.
pub(crate) trait IterationControl {
    /// `completed_iters` is the number of iterations finished so far (0 on the
    /// first, up-front call). Return `true` to run the next iteration, `false` to
    /// stop the VU.
    fn next(&mut self, completed_iters: u32) -> impl Future<Output = bool>;
}

impl<F: FnMut(u32) -> bool> IterationControl for F {
    async fn next(&mut self, completed_iters: u32) -> bool {
        self(completed_iters)
    }
}

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
        // The op carries its own future + resource; the scheduler just awaits it.
        // No backpressure (only Http gates HTTP concurrency).
        HostOp::Streaming(fut) => OpDone::Stream(fut.await),
    }
}

/// The hard-cancellation tier (the second of the two-tier stop). When `token`
/// fires — the executor raises it once the graceful-stop deadline (`gracefulStop`,
/// default 30s) expires — a VU still parked mid-op is `force_unwind`-ed and its
/// in-flight iteration counted as **interrupted** (a shutdown artifact, distinct
/// from `errored`). Graceful stop (at an `IterationBoundary`) never touches this;
/// it's only for VUs that can't reach a boundary because their I/O is hung.
#[derive(Clone)]
pub(crate) struct HardStop {
    pub(crate) token: CancellationToken,
    pub(crate) interrupted: Arc<AtomicU64>,
}

impl HardStop {
    /// A `HardStop` that never fires — for callers with no hard deadline (tests,
    /// the harness). Its interrupted counter is discarded.
    pub(crate) fn never() -> Self {
        Self {
            token: CancellationToken::new(),
            interrupted: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// The ONLY sanctioned way to run a VU (I3): `spawn_local`. The `unsafe Send` on
/// [`Shared`] would let `tokio::spawn` compile — and be UB. Graceful-only variant
/// (no hard deadline); production paths use [`spawn_vu_hard`].
pub(crate) fn spawn_vu<C, K>(
    coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
    control: K,
) -> tokio::task::JoinHandle<()>
where
    C: HttpClient + 'static,
    K: IterationControl + 'static,
{
    spawn_vu_hard(coro, shared, client, bp, control, HardStop::never())
}

/// [`spawn_vu`] with the hard-cancellation tier wired in.
///
/// This is the ASYNC driver (R1): it — not the sync per-iteration body inside the
/// coroutine — awaits tokio futures and services I/O yields.
pub(crate) fn spawn_vu_hard<C, K>(
    coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
    control: K,
    hard: HardStop,
) -> tokio::task::JoinHandle<()>
where
    C: HttpClient + 'static,
    K: IterationControl + 'static,
{
    use futures_util::FutureExt;
    tokio::task::spawn_local(async move {
        // Fault isolation (#9): catch a panic in the VU's own execution (a host-fn
        // or client bug) at the task boundary. CRUCIAL — if the panic instead
        // unwound the task, the parked `coro` would be dropped DURING the active
        // panic, and `Coroutine::drop` force_unwinds a suspended coroutine → a
        // force_unwind-during-panic aborts the process (taking every other VU on
        // the loop thread). Catching here stops the panic first; `coro` then drops
        // AFTER, so its force_unwind runs cleanly (the validated path). One VU's
        // bug degrades to one lost VU + a loud log, not a whole-run abort.
        let driven = std::panic::AssertUnwindSafe(drive_vu(
            PanicSafeCoro(Some(coro)), shared, client, bp, control, hard,
        ))
        .catch_unwind()
        .await;
        if driven.is_err() {
            eprintln!(
                "error: a VU panicked mid-run and was isolated; run is DEGRADED \
                 (iteration counts may be undercounted)."
            );
        }
    })
}

/// Drive a VU coroutine's iteration loop. Owns the VU's futures; never touches
/// its `Context` (I1); metrics-free. Runs each op via [`run_op`].
///
/// `control` (an [`IterationControl`]) is the RunNext/Stop hook, consulted at each
/// `IterationBoundary` (and once up front). A *port*, not a baked count: #5's
/// constant-vus supplies a deadline check, the arrival-rate coordinator parks here
/// awaiting dispatch, and the two-tier cancellation plugs in — prefer stopping at
/// the boundary, reserving `force_unwind` mid-op for a hard deadline.
pub(crate) async fn drive_vu<C, K>(
    mut coro: PanicSafeCoro,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
    mut control: K,
    hard: HardStop,
) where
    C: HttpClient + 'static,
    K: IterationControl,
{
    type PendingFut = Pin<Box<dyn std::future::Future<Output = (OpId, OpDone)>>>;
    let mut pending: FuturesUnordered<PendingFut> = FuturesUnordered::new();
    // First resume runs iteration 0 unless control declines it up front.
    let mut resume = if control.next(0).await { Resume::RunNext } else { Resume::Stop };
    let mut done_iters = 0u32;
    let home = std::thread::current().id();

    // Hard-cancel handler: the coroutine is suspended at a yield (we're in a select
    // between resumes), so `force_unwind` is safe — it unwinds the parked stack,
    // dropping the in-flight iteration + the Context cleanly (validated through the
    // QuickJS C frames on the soak platform). The interrupted iteration is counted
    // as interrupted, NOT errored. Returns `true` when it fired (caller returns).
    macro_rules! on_hard_cancel {
        () => {{
            coro.force_unwind();
            hard.interrupted.fetch_add(1, Ordering::Relaxed);
            return;
        }};
    }

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
                        _ = hard.token.cancelled() => on_hard_cancel!(),
                    }
                };
                resume = Resume::One(done);
            }
            Yield::AwaitAll(ops) => {
                // Run all batch ops CONCURRENTLY (indexed to preserve input
                // order). While parked, keep draining pending async completions
                // into `completed` (I2) — same as AwaitOne.
                let n = ops.len();
                let mut batch: FuturesUnordered<Pin<Box<dyn std::future::Future<Output = (usize, OpDone)>>>> =
                    FuturesUnordered::new();
                for (i, op) in ops.into_iter().enumerate() {
                    let client = Arc::clone(&client);
                    let bp = bp.clone();
                    batch.push(Box::pin(async move { (i, run_op(op, &client, &bp).await) }));
                }
                let mut results: Vec<Option<OpDone>> = (0..n).map(|_| None).collect();
                let mut remaining = n;
                while remaining > 0 {
                    tokio::select! {
                        Some((i, done)) = batch.next(), if remaining > 0 => {
                            results[i] = Some(done);
                            remaining -= 1;
                        }
                        Some((op, res)) = pending.next(), if !pending.is_empty() => {
                            shared.0.borrow_mut().completed.push_back((op, res));
                        }
                        _ = hard.token.cancelled() => on_hard_cancel!(),
                    }
                }
                resume = Resume::All(results.into_iter().map(|r| r.expect("all batch ops done")).collect());
            }
            Yield::AwaitPending => {
                debug_assert!(
                    !pending.is_empty(),
                    "AwaitPending with no in-flight futures — outstanding desynced from queues"
                );
                tokio::select! {
                    Some((op, res)) = pending.next() => {
                        shared.0.borrow_mut().completed.push_back((op, res));
                    }
                    _ = hard.token.cancelled() => on_hard_cancel!(),
                }
                resume = Resume::Progressed;
            }
            Yield::IterationBoundary => {
                // Iteration complete + event loop drained — the clean cancel
                // point (#5 prefers cancelling here over force_unwind).
                done_iters += 1;
                resume = if control.next(done_iters).await {
                    Resume::RunNext
                } else {
                    Resume::Stop
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k6_core::traits::{HttpClient, HttpRequest, HttpResponse, ResponseBody, Timings};

    struct Dummy;
    impl HttpClient for Dummy {
        fn send(
            &self,
            _r: HttpRequest,
        ) -> impl Future<Output = anyhow::Result<HttpResponse>> + Send {
            async {
                Ok(HttpResponse {
                    status: 0,
                    headers: vec![],
                    body: ResponseBody::Buffered(vec![]),
                    timings: Timings::default(),
                    url: String::new(),
                    data_sent: 0,
                    data_received: 0,
                })
            }
        }
    }

    /// The generic Streaming primitive (ws/grpc): the op carries its OWN future +
    /// I/O resource (here a channel, standing in for a ws `Receiver<WsEvent>`);
    /// `run_op` just awaits it — no registry param (the hard invariant) and no
    /// backpressure permit. The result is type-erased; the consumer downcasts.
    #[tokio::test]
    async fn run_op_streaming_carries_its_own_resource_and_type_erases_result() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tx.send("hello".to_string()).unwrap();
        let fut: Pin<Box<dyn Future<Output = Box<dyn Any>>>> = Box::pin(async move {
            Box::new(rx.recv().await.unwrap()) as Box<dyn Any>
        });
        let client = Arc::new(Dummy);
        let bp = Backpressure::new(4);
        match run_op(HostOp::Streaming(fut), &client, &bp).await {
            OpDone::Stream(b) => {
                assert_eq!(*b.downcast::<String>().expect("downcast"), "hello")
            }
            _ => panic!("expected Stream"),
        }
    }
}
