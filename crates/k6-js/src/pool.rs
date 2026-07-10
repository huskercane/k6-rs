//! Pool-of-loops spawn model (async-runtime graduation, #5 — slice 1).
//!
//! `N` loop threads (≈ cores), each a **current-thread** tokio runtime + a
//! `LocalSet` hosting **many** `!Send` coroutine VUs (one isolated
//! `AsyncRuntime`/`Context` per VU). This is the spawn model that replaces the old
//! one-`spawn_blocking`-per-VU design: the memory win is **thread stacks** — ~cores
//! of them, not one full stack per VU — which is what lets 7900 VUs fit the
//! fixed-memory budget. The per-VU heap (64 MB `Context` cap) is unchanged.
//!
//! This slice implements the `constant-vus` strategy only: every VU loops
//! iterations continuously until the duration expires (or cancellation), finishing
//! its current iteration at the clean `IterationBoundary` cancel point — k6's
//! graceful stop. The arrival-rate executors (with the single-writer idle-set
//! coordinator) land in the next slice; the force-unwind hard-deadline tier lands
//! in the cancellation slice.
//!
//! ## Lane separation (per the #5 design)
//! Each VU records its own metrics LOCALLY on its loop thread — `http_reqs`,
//! checks, `data_*`, and (added here) `iteration_duration` + the `iterations`
//! counter are all written coroutine-side into the shared `Send + Sync`
//! [`BuiltinMetrics`] registry. This module owns only the executor's own tally
//! (`iterations_completed`) for the [`RunSummary`], mirroring the sync executor's
//! independent atomic — it does NOT funnel per-iteration outcomes through a
//! coordinator.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::LocalSet;
use tokio_util::sync::CancellationToken;

use k6_core::backpressure::Backpressure;
use k6_core::executor::arrival::ArrivalCurve;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, RunSummary};

use crate::coroutine_vu::{IterationOutcome, build_coroutine_vu};
use crate::vu_sched::{IterationControl, Shared, spawn_vu};

/// Max wait for VUs to report their initial idle at arrival-rate startup before
/// the coordinator proceeds degraded. Idle reports are near-instant (a channel
/// send before any Context init), so this only ever fires when a VU has died
/// mid-construction — it converts a would-be deadlock into a logged degraded run.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Loop-thread count for `num_vus`: one per core, capped at the VU count (never
/// more threads than VUs), floored at 1 when there are VUs at all.
fn loop_thread_count(num_vus: usize) -> usize {
    if num_vus == 0 {
        return 0;
    }
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cores.min(num_vus).max(1)
}

/// Run `num_vus` `constant-vus` coroutine VUs of `script` until `duration` elapses
/// (or `cancel` fires), sharded across ≈`cores` loop threads. Blocks the calling
/// thread until every VU has stopped at an `IterationBoundary` (graceful stop);
/// call it from a blocking context (e.g. `spawn_blocking`) off the async executor.
pub fn run_constant_vus<C>(
    script: String,
    num_vus: usize,
    duration: Duration,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    cancel: CancellationToken,
) -> RunSummary
where
    C: HttpClient + 'static,
{
    run_constant_vus_on(
        loop_thread_count(num_vus),
        script,
        num_vus,
        duration,
        client,
        bp,
        metrics,
        cancel,
    )
}

/// [`run_constant_vus`] with an explicit loop-thread count. The public entry
/// derives `num_threads` from the core count; tests pin it to prove the invariant
/// property — *many `!Send` VUs sharing one loop thread* (`num_vus > num_threads`).
#[allow(clippy::too_many_arguments)]
fn run_constant_vus_on<C>(
    num_threads: usize,
    script: String,
    num_vus: usize,
    duration: Duration,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    cancel: CancellationToken,
) -> RunSummary
where
    C: HttpClient + 'static,
{
    let start = Instant::now();
    let deadline = start + duration;
    let completed = Arc::new(AtomicU64::new(0));
    let errored = Arc::new(AtomicU64::new(0));

    let mut threads = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        // Round-robin VU→thread assignment (static: VUs are `!Send`, so there is
        // no work-stealing — load balances at the dispatch layer, not by moving
        // VUs). Thread `t` owns the VUs whose id ≡ t (mod num_threads).
        let my_vus = (0..num_vus).filter(|id| id % num_threads == t).count();
        if my_vus == 0 {
            continue;
        }
        let script = script.clone();
        let client = Arc::clone(&client);
        let bp = bp.clone();
        let metrics = metrics.clone();
        let cancel = cancel.clone();
        let completed = Arc::clone(&completed);
        let errored = Arc::clone(&errored);

        threads.push(
            thread::Builder::new()
                .name(format!("k6-loop-{t}"))
                .spawn(move || {
                    run_loop_thread(my_vus, script, deadline, client, bp, metrics, cancel, completed, errored)
                })
                .expect("spawn loop thread"),
        );
    }

    for h in threads {
        let _ = h.join();
    }

    RunSummary {
        iterations_completed: completed.load(Ordering::Relaxed),
        iterations_dropped: 0, // constant-vus never drops (no arrival curve)
        iterations_errored: errored.load(Ordering::Relaxed),
        duration: start.elapsed(),
        ..Default::default()
    }
}

/// One loop thread: a current-thread runtime + `LocalSet` hosting `vu_count`
/// coroutine VUs, each driven to the `deadline`.
#[allow(clippy::too_many_arguments)]
fn run_loop_thread<C>(
    vu_count: usize,
    script: String,
    deadline: Instant,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    cancel: CancellationToken,
    completed: Arc<AtomicU64>,
    errored: Arc<AtomicU64>,
) where
    C: HttpClient + 'static,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build loop-thread runtime");

    LocalSet::new().block_on(&rt, async move {
        let mut handles = Vec::with_capacity(vu_count);
        for _ in 0..vu_count {
            let shared = Shared::new();
            // `result` is the per-VU outcome cell: the coroutine writes the last
            // iteration's typed outcome, the control hook reads it (same thread).
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(
                script.clone(),
                shared.clone(),
                Some(metrics.clone()),
                result.clone(),
            );

            let cancel = cancel.clone();
            let completed = Arc::clone(&completed);
            let errored = Arc::clone(&errored);
            // constant-vus control hook, consulted at each `IterationBoundary`
            // (and once up front at n==0). It reads the iteration that just
            // finished — published to `result` before this call — tallies it, then
            // decides RunNext (true) / Stop (false). Sync is sufficient here: no
            // coordinator, just a deadline/cancel check. (The async next_action
            // hook arrives with the arrival-rate coordinator slice.)
            let control = move |n: u32| -> bool {
                if n >= 1 {
                    // n>=1 ⇒ iteration n-1 just finished; classify it. Completed
                    // and Errored are counted in separate lanes (parity with the
                    // sync path: an Errored iteration is NOT a completion and
                    // records no iteration metric — that's coroutine-side).
                    match result.borrow().as_ref() {
                        Some(IterationOutcome::Completed { .. }) => {
                            completed.fetch_add(1, Ordering::Relaxed);
                        }
                        Some(IterationOutcome::Errored { .. }) => {
                            errored.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {}
                    }
                }
                !cancel.is_cancelled() && Instant::now() < deadline
            };

            handles.push(spawn_vu(
                coro,
                shared,
                Arc::clone(&client),
                bp.clone(),
                control,
            ));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

// ---------------------------------------------------------------------------
// Arrival-rate: the coordinator + its VU control port.
// ---------------------------------------------------------------------------

/// The arrival-rate control port: at each `IterationBoundary` (and once up front)
/// the VU tallies the iteration it just finished, signals itself idle to the
/// coordinator, then parks awaiting its per-VU dispatch (`RunNext`). The VU is a
/// member of the coordinator's idle set **iff** parked here (= available); it is
/// removed the instant the coordinator dispatches it (= busy) and re-enters only
/// on its next boundary. The VU never touches the idle queue — it only SENDS its
/// id — so the coordinator is the single writer.
struct ArrivalControl {
    my_id: usize,
    idle_tx: UnboundedSender<usize>,
    run_next_rx: UnboundedReceiver<()>,
    result: Rc<RefCell<Option<IterationOutcome>>>,
    completed: Arc<AtomicU64>,
    errored: Arc<AtomicU64>,
}

impl IterationControl for ArrivalControl {
    async fn next(&mut self, completed_iters: u32) -> bool {
        if completed_iters >= 1 {
            // Classify the finished iteration into its lane. A dispatched
            // iteration that threw is `errored`, NOT a completion and NOT a drop —
            // it consumed an arrival slot and ran, so conservation requires it be
            // counted here (else completed+dropped silently under-sums the
            // integral by the error count).
            match self.result.borrow().as_ref() {
                Some(IterationOutcome::Completed { .. }) => {
                    self.completed.fetch_add(1, Ordering::Relaxed);
                }
                Some(IterationOutcome::Errored { .. }) => {
                    self.errored.fetch_add(1, Ordering::Relaxed);
                }
                None => {}
            }
        }
        // Signal idle, then await dispatch. A send error (coordinator gone) or a
        // closed channel (senders dropped at end-of-run) ⇒ graceful Stop.
        if self.idle_tx.send(self.my_id).is_err() {
            return false;
        }
        self.run_next_rx.recv().await.is_some()
    }
}

/// Run `num_vus` arrival-rate coroutine VUs of `script` driven by `curve` (constant
/// OR ramping — `ArrivalCurve::constant`/`::new`), sharded across `num_threads`
/// loop threads and paced by a single global coordinator. Blocks until the run
/// completes. Drops (an arrival with no idle VU) ARE the load-test result.
#[allow(clippy::too_many_arguments)]
pub fn run_arrival_rate<C>(
    script: String,
    num_vus: usize,
    curve: ArrivalCurve,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    cancel: CancellationToken,
) -> RunSummary
where
    C: HttpClient + 'static,
{
    run_arrival_rate_on(
        loop_thread_count(num_vus),
        script,
        num_vus,
        curve,
        client,
        bp,
        metrics,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_arrival_rate_on<C>(
    num_threads: usize,
    script: String,
    num_vus: usize,
    curve: ArrivalCurve,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    cancel: CancellationToken,
) -> RunSummary
where
    C: HttpClient + 'static,
{
    if num_vus == 0 || num_threads == 0 {
        return RunSummary::default();
    }

    let completed = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let errored = Arc::new(AtomicU64::new(0));

    // Per-VU dispatch channels: coordinator holds the senders (by id), each VU its
    // receiver. Idle channel: every VU clones `idle_tx` → coordinator's `idle_rx`.
    let (idle_tx, idle_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let mut run_next_tx: Vec<UnboundedSender<()>> = Vec::with_capacity(num_vus);
    let mut per_thread_rx: Vec<Vec<(usize, UnboundedReceiver<()>)>> =
        (0..num_threads).map(|_| Vec::new()).collect();
    for id in 0..num_vus {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        run_next_tx.push(tx);
        per_thread_rx[id % num_threads].push((id, rx));
    }

    // Spawn loop threads FIRST so their VUs park and report idle; the coordinator's
    // startup phase waits for all N reports before starting the clock.
    let mut loop_threads = Vec::with_capacity(num_threads);
    for (t, my_vus) in per_thread_rx.into_iter().enumerate() {
        if my_vus.is_empty() {
            continue;
        }
        let script = script.clone();
        let client = Arc::clone(&client);
        let bp = bp.clone();
        let metrics = metrics.clone();
        let idle_tx = idle_tx.clone();
        let completed = Arc::clone(&completed);
        let errored = Arc::clone(&errored);
        loop_threads.push(
            thread::Builder::new()
                .name(format!("k6-loop-{t}"))
                .spawn(move || {
                    run_arrival_loop_thread(my_vus, script, client, bp, metrics, idle_tx, completed, errored)
                })
                .expect("spawn loop thread"),
        );
    }
    // Drop the main-thread idle_tx clone; only the VUs' clones keep it open.
    drop(idle_tx);

    // Coordinator on its own thread — it owns the idle set and the global arrival
    // view. Returns the tight execution window (post-startup → exit).
    let coordinator = {
        let dropped = Arc::clone(&dropped);
        thread::Builder::new()
            .name("k6-coordinator".into())
            .spawn(move || run_coordinator(idle_rx, run_next_tx, curve, num_vus, cancel, dropped))
            .expect("spawn coordinator")
    };

    // Join coordinator first (it drops the dispatch senders on exit), then the loop
    // threads (VUs see the closed channel and stop at their next boundary).
    let duration = coordinator.join().unwrap_or(Duration::ZERO);
    for h in loop_threads {
        let _ = h.join();
    }

    RunSummary {
        iterations_completed: completed.load(Ordering::Relaxed),
        iterations_dropped: dropped.load(Ordering::Relaxed),
        iterations_errored: errored.load(Ordering::Relaxed),
        duration,
        ..Default::default()
    }
}

/// One arrival-rate loop thread: a current-thread runtime + `LocalSet` hosting its
/// VUs, each with an [`ArrivalControl`] wired to its own dispatch receiver.
#[allow(clippy::too_many_arguments)]
fn run_arrival_loop_thread<C>(
    vus: Vec<(usize, UnboundedReceiver<()>)>,
    script: String,
    client: Arc<C>,
    bp: Backpressure,
    metrics: BuiltinMetrics,
    idle_tx: UnboundedSender<usize>,
    completed: Arc<AtomicU64>,
    errored: Arc<AtomicU64>,
) where
    C: HttpClient + 'static,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build loop-thread runtime");

    LocalSet::new().block_on(&rt, async move {
        let mut handles = Vec::with_capacity(vus.len());
        for (id, run_next_rx) in vus {
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(
                script.clone(),
                shared.clone(),
                Some(metrics.clone()),
                result.clone(),
            );
            let control = ArrivalControl {
                my_id: id,
                idle_tx: idle_tx.clone(),
                run_next_rx,
                result,
                completed: Arc::clone(&completed),
                errored: Arc::clone(&errored),
            };
            handles.push(spawn_vu(coro, shared, Arc::clone(&client), bp.clone(), control));
        }
        for h in handles {
            let _ = h.await;
        }
    });
}

/// The single global coordinator. Sole owner of the idle set; drives arrivals off
/// the shared [`ArrivalCurve`] integral. Returns the execution-window duration.
fn run_coordinator(
    mut idle_rx: UnboundedReceiver<usize>,
    run_next_tx: Vec<UnboundedSender<()>>,
    curve: ArrivalCurve,
    num_vus: usize,
    cancel: CancellationToken,
    dropped: Arc<AtomicU64>,
) -> Duration {
    let mut idle: VecDeque<usize> = VecDeque::with_capacity(num_vus);

    // Startup: collect all N VUs' initial idle reports (the "pool full at t=0"
    // equivalence — no startup skew) BEFORE starting the clock. Cancel- and
    // timeout-aware by design: a VU that dies before its first report (e.g. a
    // coroutine-stack alloc OOM at 7900 VUs — the soak condition) must NOT hang
    // the whole run. On cancel or the startup deadline we proceed DEGRADED with
    // whoever reported (logged); the un-reported VUs simply aren't in the idle set.
    let startup_deadline = Instant::now() + STARTUP_TIMEOUT;
    while idle.len() < num_vus {
        if cancel.is_cancelled() {
            break;
        }
        match idle_rx.try_recv() {
            Ok(id) => idle.push_back(id),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if Instant::now() >= startup_deadline {
                    eprintln!(
                        "warning: only {}/{num_vus} VUs ready at the startup deadline; \
                         proceeding degraded (the rest failed to initialize)",
                        idle.len()
                    );
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            // Every VU dropped its idle sender before we got N reports — nothing
            // left to run.
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    if idle.is_empty() {
        // No VU ever became ready (all died, or cancelled during startup). Drop the
        // dispatch senders so any late survivor stops, and report a zero window.
        drop(run_next_tx);
        return Duration::ZERO;
    }

    let start = Instant::now();
    let total = curve.total_duration();
    let mut dispatched: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            break;
        }
        // Drain new idle reports into the queue. SINGLE WRITER: this is the only
        // place the idle set is mutated.
        while let Ok(id) = idle_rx.try_recv() {
            idle.push_back(id);
        }

        let elapsed = start.elapsed();
        let clamped = elapsed.min(total);
        let target = curve.expected_arrivals(clamped);

        // Fire every arrival whose scheduled position we've passed. Structurally
        // identical to the sync executor's `while … { try_acquire → run |
        // record_dropped }`: `idle.pop_front() == None` ⟺ pool exhausted ⟺ DROP,
        // at the same integral instants. That correspondence IS the equivalence.
        while (dispatched as f64) + 1.0 <= target {
            dispatched += 1;
            match idle.pop_front() {
                // Dispatch to an idle VU. A closed VU channel (raced shutdown) is
                // treated as a drop — the arrival slot is still consumed.
                Some(id) => {
                    if run_next_tx[id].send(()).is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if elapsed >= total {
            break;
        }

        // Sleep ≈ next arrival (same prediction as the sync executor; the catch-up
        // loop guarantees the count regardless of granularity). Cancellation is
        // polled at the loop top, so worst-case stop latency is one sleep.
        let inst_rate = curve.interpolate_rate(clamped);
        let sleep = if inst_rate > 0.1 {
            let deficit = (dispatched as f64 + 1.0 - target).max(0.0);
            Duration::from_secs_f64((deficit / inst_rate).clamp(0.0005, 0.05))
        } else {
            Duration::from_millis(50)
        };
        thread::sleep(sleep);
    }

    let elapsed = start.elapsed();
    // Drop the dispatch senders → every VU's `recv()` returns None → graceful Stop
    // at its next boundary.
    drop(run_next_tx);
    elapsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    use k6_core::traits::{HttpRequest, HttpResponse, ResponseBody, Timings};

    /// Send+Sync mock (crosses loop threads): every request → 200 with a tiny
    /// JSON body, no artificial delay.
    struct Mock200;
    impl HttpClient for Mock200 {
        fn send(&self, _req: HttpRequest) -> impl Future<Output = anyhow::Result<HttpResponse>> + Send {
            async {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: ResponseBody::Buffered(br#"{"ok":true}"#.to_vec()),
                    timings: Timings { duration: 1.0, ..Default::default() },
                    url: "http://x/".into(),
                    data_sent: 20,
                    data_received: 30,
                })
            }
        }
    }

    /// Slice-1 headline: the spawn model runs coroutine VUs on loop threads to a
    /// graceful stop, with **many `!Send` VUs sharing one loop thread**
    /// (`num_vus=6` pinned to `num_threads=2` ⇒ 3 VUs per thread — the crux of the
    /// pool-of-loops memory model, impossible under one-thread-per-VU). Asserts:
    /// (a) iterations were counted across threads; (b) graceful stop (returns near
    /// the deadline, not far past it); (c) executor tally == the `iterations`
    /// metric counter == `http_reqs` (each iteration does exactly one `http.get`)
    /// — the three lanes agree.
    #[test]
    fn constant_vus_spawn_model_runs_on_loop_threads_and_counts_locally() {
        let metrics = BuiltinMetrics::new();
        let script = r#"
            export default function () {
                const r = http.get('http://x/');
                if (r.json().ok !== true) throw new Error('bad');
            }
        "#;

        let summary = run_constant_vus_on(
            2, // pinned: 6 VUs / 2 threads ⇒ 3 !Send VUs per loop thread
            script.to_string(),
            6,
            Duration::from_millis(200),
            Arc::new(Mock200),
            Backpressure::new(32),
            metrics.clone(),
            CancellationToken::new(),
        );

        // (a) Real work happened — with 6 VUs and near-instant http over 200 ms
        // this is comfortably in the hundreds; assert a floor well above the VU
        // count so "each VU ran many iterations" is proven, not just "ran once".
        assert!(
            summary.iterations_completed >= 60,
            "expected many iterations across 6 VUs, got {}",
            summary.iterations_completed
        );
        assert_eq!(summary.iterations_dropped, 0, "constant-vus never drops");

        // (b) Graceful stop: finished near the 200 ms deadline (each VU completes
        // its in-flight iteration then stops at the boundary), not far past it.
        assert!(
            summary.duration >= Duration::from_millis(190)
                && summary.duration < Duration::from_millis(800),
            "expected graceful stop near the 200 ms deadline, got {:?}",
            summary.duration
        );

        // (c) The three lanes agree: the executor's own tally, the coroutine-side
        // `iterations` counter, and `http_reqs` (one GET per iteration) are equal.
        let iterations_metric = metrics.registry.counter_get("iterations");
        let http_reqs =
            metrics.registry.counter_get("http_reqs{expected_response:true,method:GET,status:200}");
        assert_eq!(
            iterations_metric, summary.iterations_completed,
            "iterations metric (coroutine lane) must equal the executor tally"
        );
        assert_eq!(
            http_reqs, summary.iterations_completed,
            "one http.get per iteration ⇒ http_reqs == iterations_completed"
        );
        // iteration_duration trend was recorded too (parity with the sync VU).
        assert!(
            metrics.registry.trend_stats("iteration_duration").is_some(),
            "iteration_duration trend must be recorded on the coroutine path"
        );
    }

    /// Discriminating outcome test (finding #2): a script that does `http.get`
    /// and THEN throws every 3rd iteration. Because the request is recorded
    /// coroutine-side before the throw, `http_reqs` counts EVERY attempt while the
    /// pool tally + the `iterations` metric count only *completed* ones. So they
    /// must diverge: `iterations_completed < http_reqs` (≈ 2/3 of it). If the tally
    /// wrongly counted `Errored` too, the two would be equal — this is the only
    /// test that exercises the count-only-Completed branch (pool.rs).
    #[test]
    fn errored_iterations_are_not_counted_but_their_requests_are() {
        let metrics = BuiltinMetrics::new();
        // Per-VU `globalThis.__n` persists across iterations (long-lived VU).
        let script = r#"
            export default function () {
                globalThis.__n = (globalThis.__n || 0) + 1;
                http.get('http://x/');           // recorded BEFORE any throw
                if (globalThis.__n % 3 === 0) throw new Error('every third');
            }
        "#;

        let summary = run_constant_vus_on(
            2,
            script.to_string(),
            4,
            Duration::from_millis(200),
            Arc::new(Mock200),
            Backpressure::new(16),
            metrics.clone(),
            CancellationToken::new(),
        );

        let iterations_metric = metrics.registry.counter_get("iterations");
        let http_reqs =
            metrics.registry.counter_get("http_reqs{expected_response:true,method:GET,status:200}");

        assert!(summary.iterations_completed > 0, "some iterations completed");
        // Every iteration (incl. thrown ones) issued exactly one GET, so http_reqs
        // == total attempts > completed. This is the discriminating assertion.
        assert!(
            summary.iterations_completed < http_reqs,
            "thrown iterations must NOT be counted: completed {} should be < http_reqs {}",
            summary.iterations_completed,
            http_reqs
        );
        // ~2/3 complete (every 3rd throws) — comfortably above half.
        assert!(
            summary.iterations_completed >= http_reqs / 2,
            "≈2/3 of attempts complete: completed {} vs http_reqs {}",
            summary.iterations_completed,
            http_reqs
        );
        // Three lanes still agree on the completed-only count.
        assert_eq!(
            iterations_metric, summary.iterations_completed,
            "iterations metric (coroutine lane) counts completed only, == executor tally"
        );
        // Conservation for constant-vus: every attempt did exactly one http.get and
        // ended completed or errored (nothing dropped/interrupted), so the two lanes
        // sum to http_reqs. This is where the errored lane earns its keep.
        assert!(summary.iterations_errored > 0, "every 3rd iteration throws");
        assert_eq!(
            summary.iterations_completed + summary.iterations_errored,
            http_reqs,
            "completed {} + errored {} should equal http_reqs {http_reqs}",
            summary.iterations_completed,
            summary.iterations_errored
        );
    }

    /// Cancellation (graceful, at the boundary): a token cancelled early stops the
    /// run well before the nominal duration. VUs stop at the next
    /// `IterationBoundary` — no force-unwind (that hard tier is a later slice).
    #[test]
    fn cancel_stops_the_run_at_the_next_boundary() {
        let metrics = BuiltinMetrics::new();
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        // Cancel after 60 ms from another thread.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            cancel2.cancel();
        });

        let summary = run_constant_vus_on(
            2,
            "export default function () { http.get('http://x/'); }".to_string(),
            4,
            Duration::from_secs(30), // long nominal duration; cancel cuts it short
            Arc::new(Mock200),
            Backpressure::new(16),
            metrics,
            cancel,
        );

        assert!(
            summary.duration < Duration::from_secs(2),
            "cancel should stop the run well before the 30 s duration, got {:?}",
            summary.duration
        );
        assert!(summary.iterations_completed > 0, "some iterations ran before cancel");
    }

    /// Degenerate input: zero VUs ⇒ no loop threads, an immediate empty summary.
    #[test]
    fn zero_vus_is_an_immediate_empty_run() {
        let summary = run_constant_vus_on(
            0,
            "export default function () {}".to_string(),
            0,
            Duration::from_millis(50),
            Arc::new(Mock200),
            Backpressure::new(4),
            BuiltinMetrics::new(),
            CancellationToken::new(),
        );
        assert_eq!(summary.iterations_completed, 0);
        assert_eq!(summary.iterations_dropped, 0);
    }

    // --- arrival-rate coordinator ------------------------------------------

    /// Drop-accounting bar (no-drop half): with an ample pool of fast VUs the
    /// coordinator never finds the idle set empty at an arrival, so ZERO drops, and
    /// the completed count lands on the curve integral (50/s × 0.4 s ≈ 20). Fast
    /// http (instant mock) ⇒ each dispatched VU is back in the idle set long before
    /// the next arrival.
    #[test]
    fn arrival_rate_ample_pool_no_drops_and_hits_integral() {
        let curve = ArrivalCurve::constant(50, Duration::from_secs(1), Duration::from_millis(400));
        let integral = curve.expected_arrivals(Duration::from_millis(400)); // ≈ 20
        let summary = run_arrival_rate_on(
            2,
            "export default function () { http.get('http://x/'); }".to_string(),
            20, // ample
            curve,
            Arc::new(Mock200),
            Backpressure::new(64),
            BuiltinMetrics::new(),
            CancellationToken::new(),
        );
        assert_eq!(summary.iterations_dropped, 0, "ample fast pool must not drop");
        // completed ≈ integral (a small tail either way for the final tick).
        let lo = (integral as u64).saturating_sub(6);
        let hi = integral as u64 + 3;
        assert!(
            (lo..=hi).contains(&summary.iterations_completed),
            "completed {} should land near the integral {integral:.1}",
            summary.iterations_completed
        );
    }

    /// Drop-accounting bar (the crux) + integral-total equivalence: 2 VUs each
    /// running a 50 ms iteration cannot sustain 100/s, so the coordinator finds the
    /// idle set empty at many arrivals ⇒ real drops. AND every arrival slot is
    /// accounted: completed + dropped ≈ the curve integral (100/s × 0.3 s ≈ 30) —
    /// idle-empty ⟺ pool-exhausted, nothing lost. This is the drop-accounting
    /// equivalence the coordinator exists to preserve; completed/dropped pull
    /// apart here as they never did in slice 1.
    #[test]
    fn arrival_rate_saturated_drops_and_total_equals_integral() {
        let curve = ArrivalCurve::constant(100, Duration::from_secs(1), Duration::from_millis(300));
        let integral = curve.expected_arrivals(Duration::from_millis(300)); // ≈ 30
        let summary = run_arrival_rate_on(
            2,
            "export default function () { sleep(0.05); }".to_string(), // 50 ms/iter
            2, // saturated: 2 VUs vs 100/s
            curve,
            Arc::new(Mock200),
            Backpressure::new(16),
            BuiltinMetrics::new(),
            CancellationToken::new(),
        );

        assert!(summary.iterations_completed > 0, "some iterations completed");
        assert!(
            summary.iterations_dropped > 0,
            "2 slow VUs vs 100/s must drop, got 0 (completed {})",
            summary.iterations_completed
        );
        // No throw in this script ⇒ the errored bucket is empty, and graceful stop
        // interrupts nothing.
        assert_eq!(summary.iterations_errored, 0);
        assert_eq!(summary.iterations_interrupted, 0);
        // Every arrival is accounted for: completed + dropped ≈ integral. Small
        // tail for the boundary tick.
        let total = summary.iterations_completed + summary.iterations_dropped;
        let lo = (integral as u64).saturating_sub(3);
        let hi = integral as u64 + 3;
        assert!(
            (lo..=hi).contains(&total),
            "completed {} + dropped {} = {total} should equal the integral {integral:.1} \
             (idle-empty ⟺ pool-exhausted — every arrival dispatched or dropped)",
            summary.iterations_completed,
            summary.iterations_dropped
        );
    }

    /// Three-way conservation (slice-2 finding 1): a script that is BOTH slow
    /// (drops) AND throws every 3rd iteration (errors). All three buckets are
    /// non-empty and they conserve: completed + dropped + errored ≈ the integral.
    /// If errored iterations fell out of the accounting (the gap this closes),
    /// completed + dropped would under-sum the integral by the error count.
    /// interrupted stays 0 — graceful stop force-unwinds nothing.
    #[test]
    fn arrival_rate_mixed_outcomes_conserve_three_ways() {
        let curve = ArrivalCurve::constant(100, Duration::from_secs(1), Duration::from_millis(300));
        let integral = curve.expected_arrivals(Duration::from_millis(300)); // ≈ 30
        let script = r#"
            export default function () {
                globalThis.__n = (globalThis.__n || 0) + 1;
                sleep(0.03);                       // 30 ms ⇒ 2 VUs can't sustain 100/s
                if (globalThis.__n % 3 === 0) throw new Error('every third');
            }
        "#;
        let summary = run_arrival_rate_on(
            2,
            script.to_string(),
            2, // saturated
            curve,
            Arc::new(Mock200),
            Backpressure::new(16),
            BuiltinMetrics::new(),
            CancellationToken::new(),
        );

        assert!(summary.iterations_completed > 0, "some completed: {summary:?}");
        assert!(summary.iterations_dropped > 0, "some dropped: {summary:?}");
        assert!(summary.iterations_errored > 0, "some errored: {summary:?}");
        assert_eq!(summary.iterations_interrupted, 0, "graceful stop interrupts nothing");

        let total = summary.iterations_completed
            + summary.iterations_dropped
            + summary.iterations_errored;
        let lo = (integral as u64).saturating_sub(3);
        let hi = integral as u64 + 3;
        assert!(
            (lo..=hi).contains(&total),
            "completed {} + dropped {} + errored {} = {total} should conserve to the \
             integral {integral:.1}",
            summary.iterations_completed,
            summary.iterations_dropped,
            summary.iterations_errored
        );
    }

    /// Startup robustness (slice-2 finding 3): a token already cancelled when the
    /// coordinator reaches its startup collection must break out and return
    /// cleanly — NOT block forever in the wait-for-N-idle loop. Same exit path a
    /// VU dying before its first idle report takes (the deadlock this closes). The
    /// run terminates; it does not hang.
    #[test]
    fn arrival_startup_cancelled_returns_without_hang() {
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled
        let summary = run_arrival_rate_on(
            2,
            "export default function () { http.get('http://x/'); }".to_string(),
            4,
            ArrivalCurve::constant(50, Duration::from_secs(1), Duration::from_secs(10)),
            Arc::new(Mock200),
            Backpressure::new(16),
            BuiltinMetrics::new(),
            cancel,
        );
        // Cancelled before the clock started ⇒ no arrivals, an empty summary, and
        // (the point) the call RETURNED rather than deadlocking.
        assert_eq!(summary.iterations_completed, 0);
        assert_eq!(summary.iterations_dropped, 0);
        assert!(summary.duration < Duration::from_secs(1));
    }

    /// Ramp integral parity (mirrors the sync `ramp_matches_integral_count`): a
    /// 0→100/s ramp over 0.3 s integrates to ≈15; with ample fast VUs the completed
    /// count lands there — the coroutine coordinator honors the SAME `ArrivalCurve`
    /// integral as the sync executor, so arrival instants match by construction.
    #[test]
    fn arrival_rate_ramp_completed_lands_on_integral() {
        use k6_core::config::Stage;
        let curve = ArrivalCurve::new(
            0.0,
            &[Stage { duration: Duration::from_millis(300), target: 100 }],
            Duration::from_secs(1),
        );
        let integral = curve.expected_arrivals(Duration::from_millis(300)); // ≈ 15
        let summary = run_arrival_rate_on(
            2,
            "export default function () { http.get('http://x/'); }".to_string(),
            30, // ample
            curve,
            Arc::new(Mock200),
            Backpressure::new(64),
            BuiltinMetrics::new(),
            CancellationToken::new(),
        );
        assert_eq!(summary.iterations_dropped, 0, "ample pool ⇒ no drops on the ramp");
        let lo = (integral as u64).saturating_sub(6);
        let hi = integral as u64 + 3;
        assert!(
            (lo..=hi).contains(&summary.iterations_completed),
            "ramp completed {} should land near the integral {integral:.1}",
            summary.iterations_completed
        );
    }
}
