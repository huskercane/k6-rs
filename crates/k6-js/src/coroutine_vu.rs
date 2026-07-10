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

use anyhow::Result;
use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, Yielder};

use k6_core::metrics::BuiltinMetrics;

use crate::api::http::register_yielding_http;
use crate::runtime::{self, VU_MAX_STACK};
use crate::vu::prepare_script;
use crate::vu_sched::{OpDone, Resume, Shared, VuCoroutine, Yield, YielderPtr};

/// Per-VU coroutine stack size — **the fixed-memory blast radius**: this × maxVUs
/// (7900 at the soak target) is a first-order term in a fixed-memory tool.
///
/// It MUST exceed `VU_MAX_STACK` (the 256 KB QuickJS *JS* recursion limit) plus
/// the deepest *native* frame beneath it (QuickJS C recursion + host-fn Rust
/// frames on the coroutine stack — note `client.send` runs on the SCHEDULER, not
/// here), so QuickJS trips its own `RangeError` BEFORE the native stack
/// guard-page `SIGSEGV`s. corosensei defaults to 1 MiB; we set it explicitly and
/// smaller. `quickjs_range_error_trips_before_native_overflow` guards the
/// coupling at this size. **TUNE via measurement under the OOM-reference script
/// before the soak (#5)** — do not inherit the default silently.
const COROUTINE_STACK_SIZE: usize = VU_MAX_STACK + 256 * 1024; // 256 KB JS + 256 KB native headroom

/// Typed per-iteration outcome. #5's executor reads this to count completed vs
/// failed iterations and drive thresholds — a thrown iteration must be
/// *type-distinct* from a script that legitimately returns a string, not a
/// `"ERR:"` sentinel that a return value could collide with (bar b).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IterationOutcome {
    Completed { value: String },
    Errored { message: String },
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
) -> Result<()> {
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

    // Yielding http + sleep (native fns yield instead of block_on).
    register_yielding_http(ctx, yp, shared, metrics.clone())?;
    crate::api::sleep::register_yielding(ctx, yp)?;

    // check + group + custom metric constructors.
    crate::api::check::register_with_metrics(ctx, metrics.clone())?;
    crate::api::check::register_group_with_metrics(ctx, metrics.clone())?;
    if let Some(ref m) = metrics {
        crate::api::metrics::register(ctx, m.registry.clone())?;
    }
    Ok(())
}

/// Build a long-lived coroutine VU for `script`. Bootstrap runs once; each
/// `RunNext` calls the default fn (the sync body yields for `http.get`), drives
/// the JS event loop to drained, then parks at `IterationBoundary`. The last
/// iteration's return value is published to `result`.
pub(crate) fn build_coroutine_vu(
    script: String,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
    result: Rc<RefCell<Option<IterationOutcome>>>,
) -> VuCoroutine {
    let prepared = prepare_script(&script);
    let stack = DefaultStack::new(COROUTINE_STACK_SIZE).expect("allocate coroutine stack");
    Coroutine::with_stack(stack, move |yielder: &Yielder<Resume, Yield>, first: Resume| {
        let yp = YielderPtr::new(yielder);
        let rt = runtime::create_runtime().expect("rt");
        let ctx = runtime::create_context(&rt).expect("ctx");

        // --- bootstrap ONCE: real yielding http (jar + __wrap_response) + the
        // user module scope (defines __k6_default). Real API surface grows here.
        ctx.with(|ctx| {
            bootstrap_api(&ctx, yp, shared.clone(), metrics.clone()).expect("bootstrap k6 API");
            ctx.eval::<(), _>(
                "globalThis.__resolvers = {}; globalThis.__done = false; globalThis.__ret = '';",
            )
            .expect("driver globals");
            if let Err(e) = ctx.eval::<(), _>(prepared.as_bytes()) {
                eprintln!("[coroutine_vu] script init error: {e:?}");
            }
        });

        // --- long-lived iteration loop (bootstrap persists; per-iteration driver
        // state reset each pass; per-iteration catch boundary).
        let mut sig = first;
        loop {
            if matches!(sig, Resume::Stop) {
                break;
            }

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

            // Call the default fn with a catch boundary; a sync `http.get` inside
            // yields the coroutine (borrow held) — the scheduler runs the request
            // and resumes. Async work (asyncRequest) settles via the driver loop.
            ctx.with(|ctx| {
                let _ = ctx.eval::<(), _>(
                    "globalThis.__done=false; globalThis.__ret=''; globalThis.__err='';
                     globalThis.__failed=false; globalThis.__resolvers={};",
                );
                let _ = ctx.eval::<(), _>(
                    r#"Promise.resolve((async function () {
                           return (typeof __k6_default === 'function') ? __k6_default() : undefined;
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
                let _ = yp.suspend(Yield::AwaitPending);
            }

            // Publish a TYPED outcome — a thrown iteration is Errored, distinct
            // from a Completed value (bar b; #5's executor counts on this).
            ctx.with(|ctx| {
                let failed: bool = ctx.globals().get("__failed").unwrap_or(false);
                let outcome = if failed {
                    let message: String = ctx.globals().get("__err").unwrap_or_default();
                    // TODO(#5): surface to the run logger (folds in the init eprintln gap).
                    IterationOutcome::Errored { message }
                } else {
                    let value: String = ctx.globals().get("__ret").unwrap_or_default();
                    IterationOutcome::Completed { value }
                };
                *result.borrow_mut() = Some(outcome);
            });

            sig = yp.suspend(Yield::IterationBoundary);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use k6_core::backpressure::Backpressure;
    use k6_core::traits::{HttpClient, HttpRequest, HttpResponse, ResponseBody, Timings};
    use tokio::task::LocalSet;

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
