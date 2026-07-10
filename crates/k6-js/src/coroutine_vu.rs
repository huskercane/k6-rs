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

use corosensei::{Coroutine, Yielder};

use k6_core::metrics::BuiltinMetrics;

use crate::api::http::register_yielding_http;
use crate::runtime;
use crate::vu::prepare_script;
use crate::vu_sched::{OpDone, Resume, Shared, VuCoroutine, Yield, YielderPtr};

/// Build a long-lived coroutine VU for `script`. Bootstrap runs once; each
/// `RunNext` calls the default fn (the sync body yields for `http.get`), drives
/// the JS event loop to drained, then parks at `IterationBoundary`. The last
/// iteration's return value is published to `result`.
pub(crate) fn build_coroutine_vu(
    script: String,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
    result: Rc<RefCell<String>>,
) -> VuCoroutine {
    let prepared = prepare_script(&script);
    Coroutine::new(move |yielder: &Yielder<Resume, Yield>, first: Resume| {
        let yp = YielderPtr::new(yielder);
        let rt = runtime::create_runtime().expect("rt");
        let ctx = runtime::create_context(&rt).expect("ctx");

        // --- bootstrap ONCE: real yielding http (jar + __wrap_response) + the
        // user module scope (defines __k6_default). Real API surface grows here.
        ctx.with(|ctx| {
            register_yielding_http(&ctx, yp, metrics.clone()).expect("register http");
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
                    "globalThis.__done=false; globalThis.__ret=''; globalThis.__resolvers={};",
                );
                let _ = ctx.eval::<(), _>(
                    r#"Promise.resolve((async function () {
                           return (typeof __k6_default === 'function') ? __k6_default() : undefined;
                       })()).then(
                           function (v) { globalThis.__ret = String(v); globalThis.__done = true; },
                           function (e) { globalThis.__ret = 'ERR:' + e; globalThis.__done = true; });"#,
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
                    shared.0.borrow_mut().outstanding -= n;
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

            ctx.with(|ctx| {
                let r: String = ctx.globals().get("__ret").unwrap_or_default();
                *result.borrow_mut() = r;
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
            let result = Rc::new(RefCell::new(String::new()));
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

            assert_eq!(*result.borrow(), "200", "sync http.get resolved via __wrap_response");
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
}
