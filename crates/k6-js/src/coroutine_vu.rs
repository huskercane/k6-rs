//! Production coroutine VU (async-runtime graduation, step 3). A long-lived
//! per-VU coroutine — bootstrap the real k6 API ONCE, then loop iterations,
//! yielding for I/O (`vu_sched`) and parking at `IterationBoundary` between them.
//! The sync per-iteration body runs INSIDE the coroutine via coroutine yields;
//! the async `drive_vu` (executor-spawned, #5) services I/O — that's the R1 seam.
//!
//! This module is the first *real consumer* of `vu_sched` + `register_yielding_http`
//! — it validates the scheduler API against production use (not the harness). It
//! is not yet wired into the executors (#5); today it's exercised by its tests.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use anyhow::Result;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, Yielder};

use k6_core::metrics::BuiltinMetrics;

use crate::api::http::register_yielding_http;
use crate::runtime::{self, VU_MAX_STACK};
use crate::vu::prepare_script_with_dir;
use crate::vu_sched::{OpDone, Resume, Shared, VuCoroutine, Yield, YielderPtr};

/// Per-VU coroutine stack size — **the fixed-memory blast radius**: this × maxVUs
/// (7900 at the soak target) is a first-order term in a fixed-memory tool.
///
/// It MUST exceed `VU_MAX_STACK` (the 256 KB QuickJS *JS* recursion limit) plus
/// the deepest *native* frame beneath it (QuickJS C recursion + host-fn Rust
/// frames on the coroutine stack — note `client.send` runs on the SCHEDULER, not
/// here), so QuickJS trips its own `RangeError` BEFORE the native stack
/// guard-page `SIGSEGV`s. corosensei defaults to 1 MiB; we set it explicitly and
/// smaller. `quickjs_range_error_trips_before_native_overflow` +
/// `fat_frame_recursion_traps_rangeerror_before_native_overflow` guard the coupling
/// at this size.
///
/// **MEASURED (#14, `measure_fat_frame_c_stack_highwater`):** worst-case C-stack
/// high-water = **~252 KB** — a deep recursion with a native-heavy host fn
/// (`crypto.sha256`) at EVERY frame, far more stressful than the flat http loops
/// real load-test scripts run. It lands right at `VU_MAX_STACK` because QuickJS's
/// anchored `js_check_stack_overflow` caps JS recursion at that C-stack budget
/// regardless of frame fatness (the heavy I/O future runs on the SCHEDULER, off
/// this stack, per I1). So 512 KB carries the deepest JS+native frame plus the
/// coroutine/driver frames beneath it with >2× headroom. At 7900 VUs that's
/// ~4 GB of stacks — the fixed-memory budget this migration was designed around.
const COROUTINE_STACK_SIZE: usize = VU_MAX_STACK + 256 * 1024; // 256 KB JS + 256 KB native headroom

/// Typed per-iteration outcome. #5's executor reads this to count completed vs
/// failed iterations and drive thresholds — a thrown iteration must be
/// *type-distinct* from a script that legitimately returns a string, not a
/// `"ERR:"` sentinel that a return value could collide with (bar b).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IterationOutcome {
    Completed { value: String },
    Errored { message: String },
    /// The iteration was cut short by the hard-stop (the CPU-interrupt handler
    /// broke a runaway JS loop, or the event loop was abandoned at shutdown). A
    /// shutdown ARTIFACT — counted `interrupted`, NOT `errored`, so a graceful stop
    /// can't trip an error threshold in the run's final moment. Semantically
    /// identical to a `force_unwind` interrupt (which the VU can't reach a boundary
    /// to publish — that path is counted in `drive_vu`).
    Interrupted,
}

/// Everything a coroutine VU needs to run a user script beyond the k6 API: the
/// source plus the per-run script environment (`__ENV`), the `setup()` result
/// (`__k6_setup_data`), and which exported function to call each iteration
/// (`exec`, default `__k6_default`). Cloned per VU across loop threads (all `Send`).
#[derive(Clone, Default)]
pub struct VuSpec {
    /// RAW script source (NOT pre-transformed) — the builder runs `prepare_script`
    /// with `script_dir` so local imports resolve.
    pub script: String,
    /// Directory the script lives in, for resolving `./`/`../` imports + `open()`.
    pub script_dir: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    /// JSON-serialized `setup()` return value, or `None`.
    pub setup_data: Option<String>,
    /// Named exported function to run each iteration; `None` = the default export.
    pub exec_fn: Option<String>,
}

impl VuSpec {
    /// A spec with just a script — empty env, no setup data, default export.
    pub fn script(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            ..Default::default()
        }
    }
}

impl From<String> for VuSpec {
    fn from(script: String) -> Self {
        Self::script(script)
    }
}

impl From<&str> for VuSpec {
    fn from(script: &str) -> Self {
        Self::script(script)
    }
}

/// Bootstrap the k6 API surface into the coroutine's context — ONCE per VU. Only
/// modules that fit the yield model (or need no I/O) go here: the dependency-free
/// APIs (all `&Ctx`, no `block_on`), yielding http (jar + `__wrap_response`),
/// check, and custom metrics.
///
/// Deliberately NOT here: `sleep`/`ws`/`grpc`/timers — they `block_on` a client
/// at call time, which panics on the loop; each gets its own yield conversion.
fn bootstrap_api(
    ctx: &rquickjs::Ctx<'_>,
    yp: YielderPtr,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
    spec: &VuSpec,
    vu_id: usize,
) -> Result<()> {
    // Execution-context globals the user script reads: __VU (this VU's id), __ITER
    // (updated per iteration), __ENV (the run environment), and __k6_setup_data
    // (the setup() result). Parity with the sync VU (`vu.rs`).
    let globals = ctx.globals();
    globals.set("__VU", vu_id as u32)?;
    globals.set("__ITER", 0i32)?;
    let env_obj = rquickjs::Object::new(ctx.clone())?;
    for (k, v) in &spec.env {
        env_obj.set(k.as_str(), v.as_str())?;
    }
    globals.set("__ENV", env_obj)?;
    if let Some(json) = &spec.setup_data {
        // Set the raw JSON as a string global, then parse it in JS — avoids the
        // quote-escaping fragility of interpolating JSON into a source string.
        globals.set("__k6_setup_data_json", json.as_str())?;
        ctx.eval::<(), _>("globalThis.__k6_setup_data = JSON.parse(__k6_setup_data_json);")?;
    }

    // console is OBSERVABILITY — richer output capture folds into #6 with the
    // logger; a no-op stub is a safe defer.
    ctx.eval::<(), _>(
        "globalThis.console = { log: function () {}, warn: function () {}, error: function () {} };",
    )?;
    // fail + randomSeed are BEHAVIORAL — reuse the sync VU's real impls (one
    // source of truth). randomSeed installs a real seeded xorshift32 PRNG; a
    // no-op stub would silently diverge script results vs the sync VU + upstream.
    crate::vu::QuickJsVu::register_fail(ctx)?;
    crate::vu::QuickJsVu::register_random_seed(ctx)?;

    // Dependency-free k6 API (all &Ctx, no block_on — safe in the coroutine).
    crate::api::encoding::register(ctx)?;
    crate::api::crypto::register(ctx)?;
    crate::api::execution::register(ctx)?;
    crate::api::html::register(ctx)?;
    crate::api::secrets::register(ctx)?;
    crate::api::csv::register(ctx)?;
    crate::api::fs::register(ctx)?;
    crate::api::streams::register(ctx)?;
    crate::api::webcrypto::register(ctx)?;

    // Yielding http + sleep + ws (native fns yield instead of block_on). ws gets
    // a fresh per-VU sibling session registry (not on VuShared).
    register_yielding_http(ctx, yp, shared, metrics.clone())?;
    crate::api::sleep::register_yielding(ctx, yp)?;
    crate::api::ws::register_yielding_ws(ctx, yp, crate::api::ws::WsRegistry::new(), metrics.clone())?;
    crate::api::grpc::register_yielding_grpc(ctx, yp, metrics.clone())?;

    // check + group + custom metric constructors.
    crate::api::check::register_with_metrics(ctx, metrics.clone())?;
    crate::api::check::register_group_with_metrics(ctx, metrics.clone())?;
    if let Some(ref m) = metrics {
        crate::api::metrics::register(ctx, m.registry.clone())?;
    }
    Ok(())
}

/// Build a long-lived coroutine VU for `script` with default env/setup/exec — a
/// thin shim over [`build_coroutine_vu_spec`] used by the coroutine_vu tests.
#[cfg(test)]
pub(crate) fn build_coroutine_vu(
    script: String,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
    result: Rc<RefCell<Option<IterationOutcome>>>,
) -> VuCoroutine {
    // Tests use a never-firing hard token (no CPU-interrupt / hard-stop).
    build_coroutine_vu_spec(
        VuSpec::script(script),
        0,
        shared,
        metrics,
        result,
        tokio_util::sync::CancellationToken::new(),
    )
}

/// Build a long-lived coroutine VU from a full [`VuSpec`]. Bootstrap runs once
/// (installs the k6 API + __VU/__ENV/setup data, then evaluates the user script);
/// each `RunNext` calls the configured `exec` function (the sync body yields for
/// `http.get`), drives the JS event loop to drained, then parks at
/// `IterationBoundary`. The last iteration's return value is published to `result`.
pub(crate) fn build_coroutine_vu_spec(
    spec: VuSpec,
    vu_id: usize,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
    result: Rc<RefCell<Option<IterationOutcome>>>,
    hard_token: tokio_util::sync::CancellationToken,
) -> VuCoroutine {
    let prepared = prepare_script_with_dir(&spec.script, spec.script_dir.as_deref());
    // The exported function to run each iteration; default export otherwise.
    let exec_name = spec.exec_fn.clone().unwrap_or_else(|| "__k6_default".to_string());
    let stack = DefaultStack::new(COROUTINE_STACK_SIZE).expect("allocate coroutine stack");
    Coroutine::with_stack(stack, move |yielder: &Yielder<Resume, Yield>, first: Resume| {
        let yp = YielderPtr::new(yielder);
        let rt = runtime::create_runtime().expect("rt");
        // CPU-bound interrupt (#11): QuickJS calls this at JS bytecode back-edges;
        // returning true throws an (interpreter-level) exception that unwinds the
        // running script — the only way to break a runaway `while(true){}` that
        // never yields to a suspend point (force_unwind can't reach it). Wired to
        // the hard-stop token, so it fires only at the hard deadline. SCOPE: this
        // reaches JS loops only — a hung native host fn or a ReDoS regex in
        // QuickJS's C engine never returns to the interpreter loop, so neither this
        // NOR force_unwind can interrupt them; those fall to the process-level hard
        // kill (second Ctrl-C → exit(130)), the documented backstop for native hangs.
        {
            let t = hard_token.clone();
            rt.set_interrupt_handler(Some(Box::new(move || t.is_cancelled())));
        }
        let ctx = runtime::create_context(&rt).expect("ctx");

        // --- bootstrap ONCE: real yielding http (jar + __wrap_response) + the
        // user module scope (defines the exports). Real API surface grows here.
        ctx.with(|ctx| {
            bootstrap_api(&ctx, yp, shared.clone(), metrics.clone(), &spec, vu_id)
                .expect("bootstrap k6 API");
            ctx.eval::<(), _>(
                "globalThis.__resolvers = {}; globalThis.__done = false; globalThis.__ret = '';",
            )
            .expect("driver globals");
            if let Err(e) = ctx.eval::<(), _>(prepared.as_bytes()) {
                eprintln!("[coroutine_vu] script init error: {e:?}");
            }
            // Resolve the exec function ONCE (after the script defines it) so the
            // per-iteration body is a fixed string (no re-parse). Falls back to the
            // default export if the named function is missing.
            globals_set_exec(&ctx, &exec_name);
        });

        // --- long-lived iteration loop (bootstrap persists; per-iteration driver
        // state reset each pass; per-iteration catch boundary).
        let mut sig = first;
        let mut iter_index = 0u32;
        loop {
            if matches!(sig, Resume::Stop) {
                break;
            }

            // __ITER = this iteration's 0-based index (parity with the sync VU).
            // Set via globals (no eval/parse) each iteration.
            ctx.with(|ctx| {
                let _ = ctx.globals().set("__ITER", iter_index);
            });

            {
                let mut s = shared.0.borrow_mut();
                debug_assert!(
                    s.outstanding == 0
                        && s.registered.is_empty()
                        && s.completed.is_empty()
                        && s.async_meta.is_empty(),
                    "driver bookkeeping bled across IterationBoundary"
                );
                s.outstanding = 0;
                s.registered.clear();
                s.completed.clear();
                s.async_meta.clear();
            }

            // Iteration wall-clock starts here — start of the default fn through
            // event-loop drain, INCLUDING sleeps + I/O waits (the coroutine is
            // suspended during I/O but wall time keeps ticking). Same window the
            // sync VU measures (`vu.rs`), so `iteration_duration` is parity-faithful.
            let iter_start = Instant::now();

            // Call the default fn with a catch boundary; a sync `http.get` inside
            // yields the coroutine (borrow held) — the scheduler runs the request
            // and resumes. Async work (asyncRequest) settles via the driver loop.
            ctx.with(|ctx| {
                let _ = ctx.eval::<(), _>(
                    "globalThis.__done=false; globalThis.__ret=''; globalThis.__err='';
                     globalThis.__failed=false; globalThis.__resolvers={};",
                );
                // Call the resolved exec fn with the setup() data. Fixed string —
                // `__k6_exec` was bound once at bootstrap (no per-iteration parse).
                let _ = ctx.eval::<(), _>(
                    r#"Promise.resolve((async function () {
                           return (typeof __k6_exec === 'function') ? __k6_exec(globalThis.__k6_setup_data) : undefined;
                       })()).then(
                           function (v) { globalThis.__ret = String(v); globalThis.__done = true; },
                           function (e) { globalThis.__err = String(e); globalThis.__failed = true; globalThis.__done = true; });"#,
                );
            });

            // Driver loop: resolve completed async http ops (I2, driver-loop-side
            // metrics), drain jobs, end when main settled AND event loop drained.
            loop {
                ctx.with(|ctx| {
                    let drained: Vec<_> = shared.0.borrow_mut().completed.drain(..).collect();
                    if drained.is_empty() {
                        return;
                    }
                    let resolvers: rquickjs::Object = ctx.globals().get("__resolvers").unwrap();
                    let n = drained.len() as u64;
                    for (op, done) in drained {
                        let key = op.to_string();
                        let resolver = resolvers.get::<_, rquickjs::Function>(key.as_str()).ok();
                        match done {
                            OpDone::Slept => {}
                            // Streaming ops (ws/grpc) are consumed synchronously via
                            // AwaitOne inside the recv loop, never registered async —
                            // so they never reach this driver-loop drain.
                            OpDone::Stream(_) => unreachable!("streaming op in async drain"),
                            OpDone::Http(res) => {
                                let meta = shared
                                    .0
                                    .borrow_mut()
                                    .async_meta
                                    .remove(&op)
                                    .expect("async http meta");
                                let resp = crate::api::http::finish_http_response(
                                    res,
                                    &meta.method,
                                    meta.user_tags,
                                    &meta.response_callback,
                                    metrics.as_ref(),
                                );
                                if let Some(f) = resolver {
                                    let _ = f.call::<_, ()>((resp,));
                                }
                            }
                        }
                        let _ = resolvers.remove(key.as_str());
                    }
                    let mut s = shared.0.borrow_mut();
                    s.outstanding = s.outstanding.saturating_sub(n);
                });

                runtime::drain_pending_jobs(&rt);

                let settled =
                    ctx.with(|ctx| ctx.globals().get::<_, bool>("__done").unwrap_or(false));
                let idle = shared.0.borrow().outstanding == 0;
                if settled && idle {
                    break;
                }
                // Hard-stop fired mid-iteration (the CPU-interrupt broke a runaway
                // loop, or I/O is being abandoned): stop draining. The event loop
                // may keep re-interrupting (the handler stays armed), so waiting for
                // a clean settle would spin — bail and classify below as interrupted.
                if hard_token.is_cancelled() {
                    break;
                }
                let _ = yp.suspend(Yield::AwaitPending);
            }

            // Publish a TYPED outcome. A thrown iteration is Errored (distinct from
            // a Completed value; #5's executor counts on this). BUT if the hard-stop
            // fired during this iteration and it did NOT cleanly complete, the throw
            // is a shutdown artifact (the CPU-interrupt broke it, or the drain
            // bailed) → Interrupted, NOT Errored (design note 3: an interrupt-throw
            // must land in the interrupted lane so it can't trip an error threshold
            // at end-of-run). A clean completion at shutdown still counts Completed.
            ctx.with(|ctx| {
                let failed: bool = ctx.globals().get("__failed").unwrap_or(false);
                let done: bool = ctx.globals().get("__done").unwrap_or(false);
                let outcome = if hard_token.is_cancelled() && (failed || !done) {
                    IterationOutcome::Interrupted
                } else if failed {
                    let message: String = ctx.globals().get("__err").unwrap_or_default();
                    // TODO(#6): surface to the run logger (folds in the init eprintln gap).
                    IterationOutcome::Errored { message }
                } else {
                    // Match the sync VU exactly: record `iteration_duration` +
                    // bump the `iterations` counter ONLY for a completed iteration.
                    // A thrown iteration records neither (the sync path returns Err
                    // before `record_iteration`), so error iterations don't inflate
                    // the iteration trend or count.
                    if let Some(ref m) = metrics {
                        m.record_iteration(iter_start.elapsed().as_secs_f64() * 1000.0);
                    }
                    let value: String = ctx.globals().get("__ret").unwrap_or_default();
                    IterationOutcome::Completed { value }
                };
                *result.borrow_mut() = Some(outcome);
            });

            iter_index += 1;
            sig = yp.suspend(Yield::IterationBoundary);
        }
    })
}

/// Bind `globalThis.__k6_exec` to the exec function once (after the script has
/// defined its exports), so the per-iteration body needn't re-resolve or re-parse.
/// Falls back to the default export (`__k6_default`) if the named function is
/// absent, matching the sync VU's "default when unspecified" behavior.
fn globals_set_exec(ctx: &rquickjs::Ctx<'_>, exec_name: &str) {
    let _ = ctx.globals().set("__k6_exec_name", exec_name);
    let _ = ctx.eval::<(), _>(
        "globalThis.__k6_exec = (typeof globalThis[__k6_exec_name] === 'function') \
             ? globalThis[__k6_exec_name] : globalThis.__k6_default;",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use corosensei::CoroutineResult;
    use k6_core::backpressure::Backpressure;
    use k6_core::traits::{HttpClient, HttpRequest, HttpResponse, ResponseBody, Timings};
    use tokio::task::LocalSet;

    use crate::vu_sched::HostOp;

    /// SPIKE (#5 slice 3b gate): can we `force_unwind` a coroutine that is parked
    /// mid `http.get` — i.e. suspended INSIDE a native http fn called from
    /// QuickJS's C interpreter, so QuickJS C frames sit on the coroutine stack
    /// between the suspend point and the root? corosensei unwinds via a Rust panic
    /// from the suspend point; that panic must pass through those cleanup-free C
    /// frames. On Linux x86-64 CFI unwind tables usually allow it, but it is
    /// platform-fragile and has never been exercised. This drives the risk to a
    /// yes/no before the hard-cancellation tier is built on it.
    ///
    /// `#[ignore]` so a potential abort can't break normal CI; run explicitly:
    ///   cargo test -p k6-js force_unwind_through_quickjs -- --ignored --nocapture
    /// PASS ⇒ the coroutine reaches `done()` cleanly (Context dropped on unwind).
    /// A process abort/SIGILL here ⇒ force_unwind-through-C is UNSAFE → redesign 3b.
    #[test]
    #[ignore = "force_unwind-through-QuickJS-C spike; run explicitly (may abort if unsafe)"]
    fn force_unwind_through_quickjs_c_frames_tears_down_cleanly() {
        // No client / tokio runtime needed: resuming synchronously drives the JS
        // until http.get YIELDS AwaitOne(Http); we never run the op.
        let shared = Shared::new();
        let result = Rc::new(RefCell::new(None));
        let mut coro = build_coroutine_vu(
            "export default function () { http.get('http://x/'); }".to_string(),
            shared.clone(),
            None,
            result,
        );

        // Resume once → the coroutine bootstraps, runs the default fn, and parks at
        // AwaitOne(Http) inside ctx.eval (QuickJS C frames now on its stack).
        match coro.resume(Resume::RunNext) {
            CoroutineResult::Yield(Yield::AwaitOne(HostOp::Http(_))) => {}
            CoroutineResult::Yield(_) => {
                panic!("expected to park at AwaitOne(Http); yielded a different op")
            }
            CoroutineResult::Return(()) => panic!("coroutine returned without parking on http.get"),
        }
        assert!(coro.started() && !coro.done(), "must be parked mid-iteration");

        // The moment of truth: unwind the coroutine while QuickJS C frames are live.
        coro.force_unwind();

        assert!(
            coro.done(),
            "force_unwind must fully unwind the coroutine (Context dropped) — reaching \
             here at all means the Rust panic passed through the QuickJS C frames without \
             aborting the process"
        );
    }

    /// Sibling spike: force_unwind while parked mid `sleep` (AwaitOne(Sleep)) — the
    /// suspend is likewise inside ctx.eval, so C frames are on the stack.
    #[test]
    #[ignore = "force_unwind-through-QuickJS-C spike; run explicitly (may abort if unsafe)"]
    fn force_unwind_while_parked_on_sleep_tears_down_cleanly() {
        let shared = Shared::new();
        let result = Rc::new(RefCell::new(None));
        let mut coro = build_coroutine_vu(
            "export default function () { sleep(10); }".to_string(),
            shared.clone(),
            None,
            result,
        );
        match coro.resume(Resume::RunNext) {
            CoroutineResult::Yield(Yield::AwaitOne(HostOp::Sleep(_))) => {}
            _ => panic!("expected to park at AwaitOne(Sleep)"),
        }
        coro.force_unwind();
        assert!(coro.done(), "force_unwind mid-sleep must tear down cleanly");
    }

    use crate::vu_sched::spawn_vu;

    /// Mock capturing the request headers so we can assert the cookie jar merged
    /// a Cookie header on the request side.
    struct CookieMock {
        set_cookie: String,
        seen_cookie: Arc<std::sync::Mutex<Option<String>>>,
    }
    impl HttpClient for CookieMock {
        fn send(
            &self,
            req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            let cookie = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
                .map(|(_, v)| v.clone());
            *self.seen_cookie.lock().unwrap() = cookie;
            let set_cookie = self.set_cookie.clone();
            async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "application/json".into()),
                        ("set-cookie".into(), set_cookie),
                    ],
                    body: ResponseBody::Buffered(br#"{"ok":true}"#.to_vec()),
                    timings: Timings {
                        duration: 5.0,
                        ..Default::default()
                    },
                    url: "http://example.test/".into(),
                    data_sent: 30,
                    data_received: 40,
                })
            }
        }
    }

    /// Real k6 sync `http.get` through the coroutine VU: it yields to the
    /// scheduler, resolves through the REAL `__wrap_response` (`res.json()`), the
    /// cookie jar works BOTH directions, and metrics are recorded.
    #[test]
    fn sync_http_get_through_coroutine_with_jar_and_wrapper() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        LocalSet::new().block_on(&rt, async {
            let seen = Arc::new(std::sync::Mutex::new(None));
            let client = Arc::new(CookieMock {
                set_cookie: "sid=abc; Path=/".into(),
                seen_cookie: seen.clone(),
            });
            let metrics = BuiltinMetrics::new();

            // Iteration 1 gets a Set-Cookie; iteration 2 must send it back (jar).
            let script = r#"
                export default function () {
                    const r = http.get('http://example.test/');
                    if (r.json().ok !== true) throw new Error('bad json');
                    return r.status;
                }
            "#;
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(
                script.to_string(),
                shared.clone(),
                Some(metrics.clone()),
                result.clone(),
            );
            // Two iterations: exercises jar persistence + long-lived bootstrap.
            spawn_vu(coro, shared.clone(), client, Backpressure::new(16), move |n| n < 2)
                .await
                .unwrap();

            assert_eq!(
                *result.borrow(),
                Some(IterationOutcome::Completed { value: "200".into() }),
                "sync http.get resolved via __wrap_response"
            );
            // Cookie jar, request side: iteration 2 sent the cookie set in iter 1.
            assert_eq!(
                seen.lock().unwrap().as_deref(),
                Some("sid=abc"),
                "cookie jar merged the Set-Cookie from a prior iteration onto the request"
            );
            // Metrics recorded (2 iterations → 2 requests).
            assert_eq!(
                metrics
                    .registry
                    .counter_get("http_reqs{expected_response:true,method:GET,status:200}"),
                2,
                "http_reqs recorded per request, coroutine-side"
            );
        });
    }

    /// A client whose send is never expected to be called (scripts here don't
    /// hit http); returns a trivial 200 if it is.
    struct NoHttp;
    impl HttpClient for NoHttp {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            async {
                Ok(HttpResponse {
                    status: 200,
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

    fn run_script(script: &str, iters: u32) -> Option<IterationOutcome> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        LocalSet::new().block_on(&rt, async {
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(script.to_string(), shared.clone(), None, result.clone());
            spawn_vu(coro, shared.clone(), Arc::new(NoHttp), Backpressure::new(4), move |n| {
                n < iters
            })
            .await
            .unwrap();
            let out = result.borrow().clone();
            out
        })
    }

    /// Full API bootstrap: a script exercising several `api::*` modules
    /// (crypto, encoding, check) alongside http runs through the coroutine — the
    /// whole surface is bootstrapped ONCE and reachable each iteration.
    #[test]
    fn multi_api_script_runs_through_coroutine() {
        let metrics = BuiltinMetrics::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = LocalSet::new().block_on(&rt, async {
            let seen = Arc::new(std::sync::Mutex::new(None));
            let client = Arc::new(CookieMock {
                set_cookie: "s=1".into(),
                seen_cookie: seen,
            });
            let script = r#"
                export default function () {
                    const h = crypto.sha256('hello', 'hex');
                    const b = b64encode('x');
                    const r = http.get('http://example.test/');
                    const ok = check(r, { 'status 200': (res) => res.status === 200 });
                    return h.slice(0, 4) + ':' + b + ':' + ok;
                }
            "#;
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(
                script.to_string(),
                shared.clone(),
                Some(metrics.clone()),
                result.clone(),
            );
            spawn_vu(coro, shared.clone(), client, Backpressure::new(8), |n| n < 1)
                .await
                .unwrap();
            let out = result.borrow().clone();
            out
        });
        // sha256('hello') starts "2cf2", b64encode('x') = "eA==", check passes.
        // check() returning true (the trailing ":true") proves it ran + recorded
        // into the check tree; crypto/encoding produced their values.
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "2cf2:eA==:true".into() }),
            "crypto + encoding + http + check all work in the bootstrapped coroutine"
        );
        let _ = metrics; // http_reqs etc. covered by the dedicated http tests.
    }

    /// Behavioral parity: `randomSeed(42)` installs the REAL seeded xorshift32
    /// PRNG on the coroutine path too (not a no-op) — so two runs produce the
    /// same sequence. A no-op stub would leave `Math.random` non-deterministic
    /// and this asserts false, catching the silent divergence.
    #[test]
    fn random_seed_is_deterministic_on_coroutine_path() {
        let script = "export default function () { randomSeed(42); return Math.random() + ',' + Math.random(); }";
        let a = run_script(script, 1);
        let b = run_script(script, 1);
        match &a {
            Some(IterationOutcome::Completed { value }) => {
                assert!(!value.starts_with("0,") && value.contains(','), "seeded values: {value}")
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert_eq!(a, b, "randomSeed must be a real PRNG (deterministic), not a no-op");
    }

    /// asyncRequest concurrency + parity: two in-VU `http.asyncRequest`s behind
    /// `Promise.all` OVERLAP (~100 ms, not ~200 ms serial), and each resolves a
    /// WRAPPED response (`res.json()` works — the parity bar). The async path
    /// registers (doesn't yield the whole VU), so they run concurrently.
    #[test]
    fn async_requests_overlap_in_vu_with_wrapper_parity() {
        use std::time::{Duration, Instant};

        struct DelayMock;
        impl HttpClient for DelayMock {
            fn send(
                &self,
                _req: HttpRequest,
            ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
                async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(HttpResponse {
                        status: 200,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: ResponseBody::Buffered(br#"{"n":7}"#.to_vec()),
                        timings: Timings { duration: 100.0, ..Default::default() },
                        url: "http://x/".into(),
                        data_sent: 20,
                        data_received: 20,
                    })
                }
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (out, elapsed) = LocalSet::new().block_on(&rt, async {
            let script = r#"
                export default async function () {
                    const rs = await Promise.all([
                        http.asyncRequest('GET', 'http://x/'),
                        http.asyncRequest('GET', 'http://x/'),
                    ]);
                    return rs[0].status + ',' + rs[0].json().n + ',' + rs[1].json().n;
                }
            "#;
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro =
                build_coroutine_vu(script.to_string(), shared.clone(), None, result.clone());
            let start = Instant::now();
            spawn_vu(coro, shared.clone(), Arc::new(DelayMock), Backpressure::new(8), |n| n < 1)
                .await
                .unwrap();
            let out = result.borrow().clone();
            (out, start.elapsed())
        });

        // Wrapper parity: json() works on both resolved responses.
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "200,7,7".into() }),
            "both asyncRequests resolved WRAPPED responses (res.json())"
        );
        // Bracket both: >=90ms proves the requests actually took ~100ms (not
        // resolved instantly / mock bypassed), <180ms proves they overlapped
        // (serialized would be ~200ms).
        assert!(
            elapsed >= Duration::from_millis(90) && elapsed < Duration::from_millis(180),
            "two in-VU asyncRequests should both take ~100 ms AND overlap, got {elapsed:?}"
        );
    }

    /// http.batch (bar c): a SYNCHRONOUS host fn that runs N requests
    /// CONCURRENTLY (yield-and-wait-for-all). Two 100 ms requests overlap
    /// (~100 ms, not ~200 ms serial), each resolves WRAPPED (res.json()), in
    /// input order. The default fn is NOT async — batch returns responses
    /// directly, proving it's a sync yield, not a promise.
    #[test]
    fn http_batch_runs_requests_concurrently_with_wrapper_parity() {
        use std::time::{Duration, Instant};

        struct DelayMock;
        impl HttpClient for DelayMock {
            fn send(
                &self,
                _req: HttpRequest,
            ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
                async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(HttpResponse {
                        status: 200,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: ResponseBody::Buffered(br#"{"n":7}"#.to_vec()),
                        timings: Timings { duration: 100.0, ..Default::default() },
                        url: "http://x/".into(),
                        data_sent: 20,
                        data_received: 20,
                    })
                }
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (out, elapsed) = LocalSet::new().block_on(&rt, async {
            let script = r#"
                export default function () {
                    const rs = http.batch([
                        ['GET', 'http://x/'],
                        ['GET', 'http://y/'],
                    ]);
                    return rs[0].status + ',' + rs[0].json().n + ',' + rs[1].json().n;
                }
            "#;
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(script.to_string(), shared.clone(), None, result.clone());
            let start = Instant::now();
            spawn_vu(coro, shared.clone(), Arc::new(DelayMock), Backpressure::new(8), |n| n < 1)
                .await
                .unwrap();
            let out = result.borrow().clone();
            (out, start.elapsed())
        });
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "200,7,7".into() }),
            "batch resolved both WRAPPED responses in order"
        );
        assert!(
            elapsed >= Duration::from_millis(90) && elapsed < Duration::from_millis(180),
            "two batched requests should both take ~100 ms AND overlap, got {elapsed:?}"
        );
    }

    /// Object-keyed http.batch parity: `http.batch({ a: url, b: url })` returns
    /// `{ a: resp, b: resp }` (keyed), NOT an array — so `responses.a.status`
    /// works, matching upstream. Locks the object-form (array-only tests missed
    /// this silent divergence, same class as randomSeed).
    #[test]
    fn object_keyed_http_batch_returns_keyed_object() {
        let out = LocalSet::new().block_on(
            &tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap(),
            async {
                let script = r#"
                    export default function () {
                        const rs = http.batch({
                            first: ['GET', 'http://x/'],
                            second: ['GET', 'http://y/'],
                        });
                        return rs.first.status + ',' + rs.second.json().ok;
                    }
                "#;
                let client = Arc::new(CookieMock {
                    set_cookie: "x=1".into(),
                    seen_cookie: Arc::new(std::sync::Mutex::new(None)),
                });
                let shared = Shared::new();
                let result = Rc::new(RefCell::new(None));
                let coro = build_coroutine_vu(script.to_string(), shared.clone(), None, result.clone());
                spawn_vu(coro, shared.clone(), client, Backpressure::new(8), |n| n < 1)
                    .await
                    .unwrap();
                let out = result.borrow().clone();
                out
            },
        );
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "200,true".into() }),
            "object-keyed batch returns a keyed object (responses.first.status), not an array"
        );
    }

    /// Regression-lock the async path's `__wrap_response` application: iteration
    /// 1's `asyncRequest` gets a Set-Cookie (extracted into the jar BY the
    /// resolver's `__wrap_response`), iteration 2's `asyncRequest` sends it. If a
    /// future change dropped `__wrap_response` from the async resolver, iter 1
    /// wouldn't extract → iter 2 sends nothing → this fails (the sync-only cookie
    /// test wouldn't catch it).
    #[test]
    fn async_request_participates_in_cookie_jar_both_directions() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let seen = LocalSet::new().block_on(&rt, async {
            let seen = Arc::new(std::sync::Mutex::new(None));
            let client = Arc::new(CookieMock {
                set_cookie: "sid=xyz; Path=/".into(),
                seen_cookie: seen.clone(),
            });
            let script = r#"
                export default async function () {
                    const r = await http.asyncRequest('GET', 'http://example.test/');
                    return r.status;
                }
            "#;
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro =
                build_coroutine_vu(script.to_string(), shared.clone(), None, result);
            spawn_vu(coro, shared.clone(), client, Backpressure::new(8), |n| n < 2)
                .await
                .unwrap();
            let s = seen.lock().unwrap().clone();
            s
        });
        assert_eq!(
            seen.as_deref(),
            Some("sid=xyz"),
            "async request sent the jar cookie extracted from a prior async response"
        );
    }

    /// HEADLINE ws regression lock: a `socket.on('message')` handler that itself
    /// yields (`http.get` inside the handler) — a NESTED `AwaitOne` on the
    /// coroutine stack. Asserts BOTH the outer socket loop delivered the message
    /// AND the inner request completed. This is the one thing that silently breaks
    /// if the coroutine frame didn't survive a nested yield; a flat-handler test
    /// wouldn't catch it. (Real local WS server; mock http for the inner call.)
    #[test]
    fn ws_message_handler_can_nest_yield_http_get() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        struct Mock200;
        impl HttpClient for Mock200 {
            fn send(
                &self,
                _req: HttpRequest,
            ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
                async {
                    Ok(HttpResponse {
                        status: 200,
                        headers: vec![],
                        body: ResponseBody::Buffered(b"{}".to_vec()),
                        timings: Timings::default(),
                        url: "http://x/".into(),
                        data_sent: 0,
                        data_received: 0,
                    })
                }
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = LocalSet::new().block_on(&rt, async {
            // WS server: accept one conn, send a text message, wait for the client
            // to close.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::task::spawn_local(async move {
                if let Ok((stream, _)) = listener.accept().await {
                    if let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await {
                        let _ = ws.send(Message::Text("hello-ws".into())).await;
                        while let Some(Ok(m)) = ws.next().await {
                            if m.is_close() {
                                break;
                            }
                        }
                    }
                }
            });

            let script = format!(
                r#"
                export default function () {{
                    globalThis.__got = '';
                    globalThis.__inner = 0;
                    ws.connect('ws://{addr}/', function (socket) {{
                        socket.on('message', function (msg) {{
                            globalThis.__got = msg;
                            // NESTED YIELD: http.get inside the ws message handler.
                            var r = http.get('http://x/');
                            globalThis.__inner = r.status;
                            socket.close();
                        }});
                    }});
                    return globalThis.__got + ':' + globalThis.__inner;
                }}
            "#
            );
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu(script, shared.clone(), None, result.clone());
            spawn_vu(coro, shared.clone(), Arc::new(Mock200), Backpressure::new(8), |n| n < 1)
                .await
                .unwrap();
            let out = result.borrow().clone();
            out
        });
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "hello-ws:200".into() }),
            "outer socket loop delivered the message AND the inner http.get (nested yield) completed"
        );
    }

    /// 5a feature parity: a full `VuSpec` wires __ENV, the setup() data, a NAMED
    /// exec function, __VU (the id passed to `build_coroutine_vu_spec`), and __ITER
    /// (advancing per iteration). Runs 2 iterations as VU 7; the last returns
    /// "BASE:token:__VU:__ITER" = "http://x:abc:7:1".
    #[test]
    fn vu_spec_wires_env_setup_named_exec_vu_and_iter() {
        let spec = VuSpec {
            script: r#"
                export function myScenario(data) {
                    return __ENV.BASE + ':' + data.token + ':' + __VU + ':' + __ITER;
                }
            "#
            .to_string(),
            script_dir: None,
            env: vec![("BASE".to_string(), "http://x".to_string())],
            setup_data: Some(r#"{"token":"abc"}"#.to_string()),
            exec_fn: Some("myScenario".to_string()),
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = LocalSet::new().block_on(&rt, async {
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu_spec(
                spec,
                7,
                shared.clone(),
                None,
                result.clone(),
                tokio_util::sync::CancellationToken::new(),
            );
            spawn_vu(coro, shared, Arc::new(NoHttp), Backpressure::new(4), |n| n < 2)
                .await
                .unwrap();
            let out = result.borrow().clone();
            out
        });
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "http://x:abc:7:1".into() }),
            "env + setup data + named exec + __VU + __ITER all wired"
        );
    }

    /// #14 fat-frame coupling: deep JS recursion where EACH frame also does
    /// native-heavy work (`crypto.sha256` — a real host fn with its own Rust
    /// frame), so a fat native frame sits beneath every JS frame. QuickJS's 256KB
    /// stack check (anchored onto the coroutine stack) must STILL trip a RangeError,
    /// caught in JS, before the native guard page — proving COROUTINE_STACK_SIZE
    /// (512KB) has headroom for the deepest JS+native frame, not just plain call
    /// depth. (The heaviest native work — the I/O future — runs on the SCHEDULER,
    /// not the coroutine stack, per I1, so this + plain-depth is the whole surface.)
    #[test]
    fn fat_frame_recursion_traps_rangeerror_before_native_overflow() {
        let script = r#"
            export default function () {
                var depth = 0;
                function rec(n) {
                    depth = n;
                    var h = crypto.sha256('x' + n, 'hex'); // fat native frame per level
                    return rec(n + 1) + h.length;
                }
                try { rec(0); return 'no-error'; }
                catch (e) { return 'caught:' + (depth > 50); }
            }
        "#;
        // Reaching a Completed outcome AT ALL means no native SIGSEGV/abort — the
        // Rust panic-free RangeError path ran. `caught:true` also confirms real
        // depth was reached (native frames didn't trip it at depth ~0).
        match run_script(script, 1) {
            Some(IterationOutcome::Completed { value }) => assert_eq!(
                value, "caught:true",
                "fat-frame recursion must trip a caught RangeError at real depth"
            ),
            other => panic!("expected Completed(caught), got {other:?} — a crash here means \
                             COROUTINE_STACK_SIZE is too small for fat frames"),
        }
    }

    /// #14 measurement (run explicitly): the actual C-stack high-water of a
    /// fat-frame recursion, via an `__sp()` probe returning the current stack
    /// pointer. QuickJS caps JS recursion at `VU_MAX_STACK` of C-stack (its anchored
    /// `js_check_stack_overflow`), so the high-water lands near 256KB + the deepest
    /// host-fn frame — the headroom under COROUTINE_STACK_SIZE (512KB). Reports the
    /// number and asserts it fits. `#[ignore]` because it's a measurement.
    ///   cargo test -p k6-js measure_fat_frame -- --ignored --nocapture
    #[test]
    #[ignore = "stack measurement; run explicitly with --ignored --nocapture"]
    fn measure_fat_frame_c_stack_highwater() {
        use rquickjs::Function;
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        let max_used = ctx.with(|ctx| {
            crate::api::crypto::register(&ctx).unwrap();
            let sp = Function::new(ctx.clone(), || -> f64 {
                let probe = 0u8;
                &probe as *const u8 as usize as f64
            })
            .unwrap();
            ctx.globals().set("__sp", sp).unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.__base = __sp();
                globalThis.__max = 0;
                function rec(n) {
                    var used = __base - __sp();            // stack grows downward
                    if (used > __max) __max = used;
                    var h = crypto.sha256('x' + n, 'hex'); // fat native frame per level
                    return rec(n + 1) + h.length;
                }
                try { rec(0); } catch (e) {}
            "#,
            )
            .unwrap();
            ctx.globals().get::<_, f64>("__max").unwrap()
        });
        let kb = max_used / 1024.0;
        println!(
            "=== fat-frame QuickJS C-stack high-water: {kb:.1} KB \
             (VU_MAX_STACK={} KB, COROUTINE_STACK_SIZE={} KB, headroom={:.1} KB) ===",
            VU_MAX_STACK / 1024,
            COROUTINE_STACK_SIZE / 1024,
            (COROUTINE_STACK_SIZE as f64 - max_used) / 1024.0
        );
        assert!(max_used > 0.0, "probe measured nothing");
        assert!(
            (max_used as usize) < COROUTINE_STACK_SIZE,
            "C-stack high-water {kb:.1} KB must fit COROUTINE_STACK_SIZE {} KB",
            COROUTINE_STACK_SIZE / 1024
        );
    }

    /// #11 CPU-interrupt: a VU spinning in `while(true){}` (no yield point, so
    /// force_unwind can't reach it) is broken by the interrupt handler when the hard
    /// token fires, and the cut-short iteration is classified **Interrupted, NOT
    /// Errored** (design note 3 — a shutdown artifact must not trip an error
    /// threshold). The token is fired from a SEPARATE OS thread because the spinning
    /// coroutine wedges its loop thread (mirrors the real watchdog thread).
    #[test]
    fn cpu_spin_is_interrupted_not_errored() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = LocalSet::new().block_on(&rt, async {
            let hard = tokio_util::sync::CancellationToken::new();
            let h2 = hard.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                h2.cancel();
            });
            let shared = Shared::new();
            let result = Rc::new(RefCell::new(None));
            let coro = build_coroutine_vu_spec(
                VuSpec::script("export default function () { while (true) {} }"),
                0,
                shared.clone(),
                None,
                result.clone(),
                hard,
            );
            // drive_vu itself uses a never-firing hard (the interrupt is what breaks
            // the spin, not force_unwind); `|n| n<1` stops after the one iteration.
            spawn_vu(coro, shared, Arc::new(NoHttp), Backpressure::new(4), |n| n < 1)
                .await
                .unwrap();
            let out = result.borrow().clone();
            out
        });
        assert_eq!(
            out,
            Some(IterationOutcome::Interrupted),
            "a hard-stop while spinning must classify Interrupted, not Errored"
        );
    }

    /// grpc (unary, same Streaming mold as ws): `grpc.connect` to a closed port
    /// YIELDS (no block_on panic on the loop) and the failure propagates as a
    /// caught JS error — not a crash. Proves the grpc yield path. (A full invoke
    /// needs a tonic server fixture — follow-up.)
    #[test]
    fn grpc_connect_yields_and_errors_gracefully() {
        let script = r#"
            export default function () {
                var c = new grpc.Client();
                try { c.connect('127.0.0.1:1', { plaintext: true }); return 'connected'; }
                catch (e) { return 'error'; }
            }
        "#;
        let out = run_script(script, 1);
        assert_eq!(
            out,
            Some(IterationOutcome::Completed { value: "error".into() }),
            "grpc.connect to a closed port yields + errors gracefully (no loop panic)"
        );
    }

    /// bar (b): a thrown iteration is a TYPED `Errored`, not a `"ERR:"` string a
    /// return value could collide with.
    #[test]
    fn iteration_error_is_typed_not_swallowed() {
        let out = run_script("export default function () { throw new Error('boom'); }", 1);
        match out {
            Some(IterationOutcome::Errored { message }) => {
                assert!(message.contains("boom"), "message was: {message}")
            }
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    /// The long-lived VU survives a thrown iteration: iter 1 throws, iter 2
    /// returns cleanly (last outcome Completed).
    #[test]
    fn vu_survives_iteration_error() {
        let script = r#"
            export default function () {
                globalThis.__n = (globalThis.__n || 0) + 1;
                if (__n === 1) throw new Error('first');
                return 'ok' + __n;
            }
        "#;
        let out = run_script(script, 2);
        assert_eq!(out, Some(IterationOutcome::Completed { value: "ok2".into() }));
    }

    /// `sleep` yields the coroutine (not `block_on`): two VUs each `sleep(0.1)`
    /// on ONE thread overlap (~100 ms), not serialize (~200 ms). This is the
    /// block_on→yield conversion doing its job — non-blocking cross-VU.
    #[test]
    fn sleep_yields_so_two_vus_overlap_on_one_thread() {
        use std::time::{Duration, Instant};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        LocalSet::new().block_on(&rt, async {
            let mk = || {
                let shared = Shared::new();
                let result = Rc::new(RefCell::new(None));
                let coro = build_coroutine_vu(
                    "export default function () { sleep(0.1); }".to_string(),
                    shared.clone(),
                    None,
                    result,
                );
                spawn_vu(coro, shared, Arc::new(NoHttp), Backpressure::new(4), |n| n < 1)
            };
            let start = Instant::now();
            let (a, b) = (mk(), mk());
            let _ = tokio::join!(a, b);
            let elapsed = start.elapsed();
            // Bracket BOTH: >=90ms proves sleep actually sleeps (a no-op-sleep
            // regression would be ~0ms), <180ms proves overlap (serialized would
            // be ~200ms).
            assert!(
                elapsed >= Duration::from_millis(90) && elapsed < Duration::from_millis(180),
                "two VUs' sleep(0.1) should both actually sleep AND overlap (~100 ms), got {elapsed:?}"
            );
        });
    }

    /// The VU_MAX_STACK ↔ COROUTINE_STACK_SIZE coupling at the tuned size: deep JS
    /// recursion trips QuickJS's `RangeError` (caught in JS), NOT a native
    /// guard-page `SIGSEGV`. A crash here means the coroutine stack is too small
    /// for the 256 KB JS limit + native frames.
    #[test]
    fn quickjs_range_error_trips_before_native_overflow() {
        let script = r#"
            export default function () {
                function rec(n) { return rec(n + 1) + 1; }
                try { rec(0); return 'no-error'; } catch (e) { return 'caught'; }
            }
        "#;
        let out = run_script(script, 1);
        assert_eq!(out, Some(IterationOutcome::Completed { value: "caught".into() }));
    }
}
