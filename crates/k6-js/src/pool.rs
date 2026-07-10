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
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tokio::task::LocalSet;
use tokio_util::sync::CancellationToken;

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, RunSummary};

use crate::coroutine_vu::{IterationOutcome, build_coroutine_vu};
use crate::vu_sched::{Shared, spawn_vu};

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

        threads.push(
            thread::Builder::new()
                .name(format!("k6-loop-{t}"))
                .spawn(move || {
                    run_loop_thread(my_vus, script, deadline, client, bp, metrics, cancel, completed)
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
        duration: start.elapsed(),
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
            // constant-vus control hook, consulted at each `IterationBoundary`
            // (and once up front at n==0). It reads the iteration that just
            // finished — published to `result` before this call — tallies it, then
            // decides RunNext (true) / Stop (false). Sync is sufficient here: no
            // coordinator, just a deadline/cancel check. (The async next_action
            // hook arrives with the arrival-rate coordinator slice.)
            let control = move |n: u32| -> bool {
                if n >= 1 {
                    // n>=1 ⇒ iteration n-1 just completed; count only Completed
                    // (an Errored iteration matches the sync path: not counted,
                    // no iteration metric — recorded coroutine-side).
                    if let Some(IterationOutcome::Completed { .. }) = result.borrow().as_ref() {
                        completed.fetch_add(1, Ordering::Relaxed);
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
}
