//! Phase 1b construction (#3/#4): yield primitive + async scheduler + per-VU
//! driver loop, now over REAL ops (`HostOp::Http` via the HttpClient, `Sleep` via
//! tokio::time). Still `b2-spike`-gated (wires into production `QuickJsVu` next).
//!
//! ## Invariants
//! - **I1** — the scheduler (`drive_vu`) owns only `(coroutines, futures)` and a
//!   client; it NEVER borrows a `Context`, and is **metrics-free** (its future is
//!   `async { bp.acquire().await; client.send(req).await }` → owned
//!   `Result<HttpResponse>`, full stop). All `BuiltinMetrics` recording is
//!   coroutine-side: the host fn (sync) after resume, the driver loop (async) on
//!   drain — via [`crate::api::http::finish_http_response`].
//! - **I2 (queue-don't-resolve)** — a future completing while the VU is parked
//!   mid sync `http.get` (borrow held) only QUEUES its result; the driver loop
//!   resolves promises inside its own borrow.
//! - **I3 (spawn_local-only)** — VU futures are thread-pinned; the `unsafe Send`
//!   on the newtypes is an FFI lie, not a license to move threads. Spawn via
//!   [`spawn_vu`]; a `debug_assert` backstops a stray `tokio::spawn`.
//!
//! Run: `cargo test -p k6-js --features b2-spike vu_loop -- --nocapture`

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use futures_util::stream::{FuturesUnordered, StreamExt};
use rquickjs::Function;

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, HttpResponse, HttpRequest};

use crate::api::http::{
    ResponseCallback, build_http_request, finish_http_response, object_entries_to_pairs,
    parse_response_callback,
};
use crate::runtime;

type OpId = u64;

/// Per-request info an async http op needs at *resolution* time (driver-loop
/// side), carried in a side-map keyed by op id so the scheduler stays ignorant of
/// it (I1) — it only moves the owned request/response.
struct AsyncMeta {
    method: String,
    user_tags: Vec<(String, String)>,
    response_callback: ResponseCallback,
}

/// A blocking op the scheduler runs on the VU's behalf. The scheduler is the ONLY
/// place futures are created (I1).
enum HostOp {
    Http(HttpRequest),
    Sleep(Duration),
}

/// The owned outcome of a `HostOp`. Carries `Result` so a transport failure
/// reaches `finish_http_response`'s Err path (status:0 / classify_error /
/// failure-tagged metric) — the bucket that silently rots if only the happy path
/// is tested.
enum OpDone {
    Http(anyhow::Result<HttpResponse>),
    Slept,
}

#[derive(Default)]
struct VuShared {
    next_op: OpId,
    /// Async ops registered but not yet resolved — see the fire-and-forget /
    /// event-loop-drained iteration-end rule in the driver loop.
    outstanding: u64,
    /// Async ops awaiting the scheduler to make futures.
    registered: Vec<(OpId, HostOp)>,
    /// Completed async results — drained ONLY by the driver loop (I2).
    completed: VecDeque<(OpId, OpDone)>,
    /// Resolution-time metadata for async http ops (see [`AsyncMeta`]).
    async_meta: HashMap<OpId, AsyncMeta>,
}

/// `Send` newtype over the per-VU `Rc` (I3 FFI lie; single-threaded in practice).
#[derive(Clone)]
struct Shared(Rc<RefCell<VuShared>>);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

enum Yield {
    /// Sync `http.get`/`sleep`: park (borrow held), run this op, resume directly.
    AwaitOne(HostOp),
    /// Driver loop: wait for a registered async op to complete.
    AwaitPending,
}

enum Resume {
    Start,
    One(OpDone),
    Progressed,
}

/// `Send` newtype over the `Yielder` pointer captured by host-fn closures.
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

/// Run one op to its owned result. The ENTIRE I/O surface of the scheduler, and
/// the only place metrics MUST NOT appear (I1).
async fn run_op<C: HttpClient + 'static>(op: HostOp, client: &Arc<C>, bp: &Backpressure) -> OpDone {
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

/// Build one VU: a sync QuickJS context running `script` (an async fn body) with
/// `httpGet`/`sleepMs` (sync, yielding) and `asyncSleep` (registers, returns a
/// promise), wrapped in the driver loop that IS this VU's event loop. Final
/// `__ret` is published to `result`; optional `probe` reads a second global.
///
/// Non-generic: the coroutine never runs `client.send` (the scheduler does, I1);
/// it only builds requests, yields, and records metrics after resume.
fn vu_coroutine(
    script: String,
    shared: Shared,
    result: Rc<RefCell<String>>,
    probe: Option<(String, Rc<RefCell<String>>)>,
    metrics: Option<BuiltinMetrics>,
) -> VuCoroutine {
    Coroutine::new(move |yielder: &Yielder<Resume, Yield>, _start: Resume| {
        let yp = YielderPtr(yielder as *const _);
        let rt = runtime::create_runtime().expect("rt");
        let ctx = runtime::create_context(&rt).expect("ctx");

        ctx.with(|ctx| {
            let g = ctx.globals();

            // sync http.get: build req (owned, from Values), yield, then record
            // metrics AFTER resume via finish_http_response (pure Rust, no borrow
            // held across the await).
            let m = metrics.clone();
            g.set(
                "__http_get",
                Function::new(
                    ctx.clone(),
                    move |method: String,
                          url: String,
                          body: rquickjs::Value<'_>,
                          headers: rquickjs::Value<'_>,
                          timeout_ms: f64,
                          tags: rquickjs::Value<'_>,
                          cb: rquickjs::Value<'_>|
                          -> crate::api::http::JsHttpResponse {
                        let req = build_http_request(&method, url, &body, &headers, timeout_ms);
                        let user_tags = object_entries_to_pairs(&tags);
                        let response_callback = parse_response_callback(&cb);
                        // method/user_tags/response_callback are coroutine-stack
                        // locals, preserved across the suspend.
                        let result = match yp.suspend(Yield::AwaitOne(HostOp::Http(req))) {
                            Resume::One(OpDone::Http(r)) => r,
                            _ => panic!("bad resume for http.get"),
                        };
                        finish_http_response(
                            result,
                            &method,
                            user_tags,
                            &response_callback,
                            m.as_ref(),
                        )
                    },
                )
                .unwrap(),
            )
            .unwrap();

            // sync sleep: yields, resumes with Slept.
            g.set(
                "__sleep_ms",
                Function::new(ctx.clone(), move |ms: f64| -> String {
                    match yp.suspend(Yield::AwaitOne(HostOp::Sleep(Duration::from_millis(ms as u64)))) {
                        Resume::One(OpDone::Slept) => format!("s{}", ms as u64),
                        _ => panic!("bad resume for sleep"),
                    }
                })
                .unwrap(),
            )
            .unwrap();

            // async register (sleep): pushes a Sleep op, returns its id; the
            // promise is minted in JS and its resolver stashed. Does NOT yield.
            let sh = shared.clone();
            g.set(
                "__register_async",
                Function::new(ctx.clone(), move |ms: f64| -> f64 {
                    let mut s = sh.0.borrow_mut();
                    let id = s.next_op;
                    s.next_op += 1;
                    s.outstanding += 1;
                    s.registered
                        .push((id, HostOp::Sleep(Duration::from_millis(ms as u64))));
                    id as f64
                })
                .unwrap(),
            )
            .unwrap();

            // asyncRequest: build the request (owned) + stash resolution meta,
            // register an Http op, return its id. Does NOT yield — the promise is
            // minted in JS. Metrics are recorded when the driver loop drains the
            // result (driver-loop-side, once), NOT here and NOT on the scheduler.
            let sh = shared.clone();
            g.set(
                "__register_async_http",
                Function::new(
                    ctx.clone(),
                    move |method: String,
                          url: String,
                          body: rquickjs::Value<'_>,
                          headers: rquickjs::Value<'_>,
                          timeout_ms: f64,
                          tags: rquickjs::Value<'_>,
                          cb: rquickjs::Value<'_>|
                          -> f64 {
                        let req = build_http_request(&method, url, &body, &headers, timeout_ms);
                        let meta = AsyncMeta {
                            method,
                            user_tags: object_entries_to_pairs(&tags),
                            response_callback: parse_response_callback(&cb),
                        };
                        let mut s = sh.0.borrow_mut();
                        let id = s.next_op;
                        s.next_op += 1;
                        s.outstanding += 1;
                        s.registered.push((id, HostOp::Http(req)));
                        s.async_meta.insert(id, meta);
                        id as f64
                    },
                )
                .unwrap(),
            )
            .unwrap();

            ctx.eval::<(), _>(
                r#"
                globalThis.__resolvers = {};
                // Minimal stand-in for http.rs's __wrap_response (adds .json());
                // the real one is reused once this graduates into QuickJsVu.
                globalThis.__wrap_response = function (raw) {
                    raw.json = function () { return JSON.parse(raw.body); };
                    return raw;
                };
                globalThis.httpGet = function (url, params) {
                    return __http_get('GET', url, null,
                        (params && params.headers) || {},
                        (params && params.timeout) || 0,
                        (params && params.tags) || null, undefined);
                };
                globalThis.sleepMs = function (ms) { return __sleep_ms(ms); };
                globalThis.asyncSleep = function (ms) {
                    var id = __register_async(ms);
                    return new Promise(function (resolve) { globalThis.__resolvers[id] = resolve; });
                };
                globalThis.asyncGet = function (url, params) {
                    var id = __register_async_http('GET', url, null,
                        (params && params.headers) || {},
                        (params && params.timeout) || 0,
                        (params && params.tags) || null, undefined);
                    // The stored resolver wraps the raw response (parity: .json()).
                    return new Promise(function (resolve) {
                        globalThis.__resolvers[id] = function (raw) { resolve(__wrap_response(raw)); };
                    });
                };
                globalThis.__done = false;
                globalThis.__ret = "";
                "#,
            )
            .unwrap();
        });

        // Run main to its first suspension (borrow spans any sync op's yield).
        ctx.with(|ctx| {
            let wrapped = format!(
                r#"Promise.resolve((async function () {{ {script} }})())
                       .then(function (v) {{ globalThis.__ret = String(v); globalThis.__done = true; }});"#
            );
            if let Err(e) = ctx.eval::<(), _>(wrapped.as_bytes()) {
                eprintln!("[vu_loop] main error: {e:?}");
            }
        });

        // Driver loop: resolve completed (I2) + drain jobs + check event-loop
        // drained, then yield for more async progress.
        loop {
            ctx.with(|ctx| {
                let drained: Vec<(OpId, OpDone)> =
                    shared.0.borrow_mut().completed.drain(..).collect();
                if drained.is_empty() {
                    return;
                }
                let resolvers: rquickjs::Object = ctx.globals().get("__resolvers").unwrap();
                let n = drained.len() as u64;
                for (op, done) in drained {
                    let key = op.to_string();
                    let resolver = resolvers.get::<_, Function>(key.as_str()).ok();
                    match done {
                        OpDone::Slept => {
                            if let Some(f) = resolver {
                                let _ = f.call::<_, ()>(("slept".to_string(),));
                            }
                        }
                        OpDone::Http(result) => {
                            // Async http metrics land HERE — driver-loop-side,
                            // inside our own borrow, exactly once per op (each op
                            // is queued once and drained once). finish_http_response
                            // is pure Rust; the JS resolver wraps the response.
                            let meta = shared
                                .0
                                .borrow_mut()
                                .async_meta
                                .remove(&op)
                                .expect("async http op must have resolution meta");
                            let resp = finish_http_response(
                                result,
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

            // Iteration ends only when main settled AND the event loop is drained
            // (no outstanding async work) — a fire-and-forget asyncRequest still
            // fires + settles first (matches upstream).
            let settled = ctx.with(|ctx| ctx.globals().get::<_, bool>("__done").unwrap_or(false));
            let idle = shared.0.borrow().outstanding == 0;
            if settled && idle {
                break;
            }

            let _ = yp.suspend(Yield::AwaitPending);
        }

        ctx.with(|ctx| {
            let r: String = ctx.globals().get("__ret").unwrap_or_default();
            *result.borrow_mut() = r;
            if let Some((name, slot)) = &probe {
                let v: String = ctx.globals().get(name.as_str()).unwrap_or_default();
                *slot.borrow_mut() = v;
            }
        });
    })
}

/// The ONLY sanctioned way to run a VU (I3): `spawn_local`. The `unsafe Send` on
/// [`Shared`] would let `tokio::spawn` compile — and be UB.
fn spawn_vu<C: HttpClient + 'static>(
    coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(drive_vu(coro, shared, client, bp))
}

/// Drive one VU coroutine to completion. Owns the VU's futures; never touches its
/// `Context` (I1); metrics-free. Runs each op via [`run_op`].
async fn drive_vu<C: HttpClient + 'static>(
    mut coro: VuCoroutine,
    shared: Shared,
    client: Arc<C>,
    bp: Backpressure,
) {
    type PendingFut = Pin<Box<dyn std::future::Future<Output = (OpId, OpDone)>>>;
    let mut pending: FuturesUnordered<PendingFut> = FuturesUnordered::new();
    let mut resume = Resume::Start;
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k6_core::traits::{ResponseBody, Timings};
    use tokio::task::LocalSet;

    fn loop_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// Mock returning a fixed 200 with timings/bytes, for the metrics assertions.
    struct Mock200;
    impl HttpClient for Mock200 {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            async {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: ResponseBody::Buffered(br#"{"ok":true}"#.to_vec()),
                    timings: Timings {
                        duration: 5.0,
                        waiting: 4.0,
                        receiving: 1.0,
                        ..Default::default()
                    },
                    url: "http://x/".into(),
                    data_sent: 30,
                    data_received: 40,
                })
            }
        }
    }

    /// Mock that fails at the transport layer (connection refused) — exercises
    /// finish_http_response's Err path (status:0, failure flag).
    struct FailClient;
    impl HttpClient for FailClient {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            async { Err(anyhow::anyhow!("connection refused")) }
        }
    }

    fn run_vu<C: HttpClient + 'static>(
        script: &str,
        client: Arc<C>,
        metrics: Option<BuiltinMetrics>,
        probe: Option<&str>,
    ) -> (String, String) {
        let rt = loop_runtime();
        let script = script.to_string();
        let probe = probe.map(|s| s.to_string());
        LocalSet::new().block_on(&rt, async move {
            let shared = Shared(Rc::new(RefCell::new(VuShared::default())));
            let result = Rc::new(RefCell::new(String::new()));
            let extra = Rc::new(RefCell::new(String::new()));
            let probe_slot = probe.map(|name| (name, extra.clone()));
            let coro = vu_coroutine(script, shared.clone(), result.clone(), probe_slot, metrics);
            spawn_vu(coro, shared.clone(), client, Backpressure::new(64))
                .await
                .unwrap();
            let out = (result.borrow().clone(), extra.borrow().clone());
            out
        })
    }

    fn dummy() -> Arc<Mock200> {
        Arc::new(Mock200)
    }

    // --- scheduler-semantics composition tests (sleep ops) ---

    #[test]
    fn composition_sync_only() {
        let (ret, _) = run_vu("return sleepMs(30);", dummy(), None, None);
        assert_eq!(ret, "s30");
    }

    #[test]
    fn composition_async_completes_during_sync_park() {
        // asyncSleep(10) completes DURING the sleepMs(50) park (borrow held) →
        // queued, not resolved → resolved after the park by the driver loop.
        let script = r#"
            var asyncVal = null;
            var p = asyncSleep(10).then(function (v) { asyncVal = v; });
            var s = sleepMs(50);
            await p;
            return s + '|' + asyncVal;
        "#;
        let (ret, _) = run_vu(script, dummy(), None, None);
        assert_eq!(ret, "s50|slept", "async op queued during park, resolved after");
    }

    #[test]
    fn composition_promise_all_overlap() {
        let script = r#"
            var vs = await Promise.all([asyncSleep(40), asyncSleep(40)]);
            return vs[0] + ',' + vs[1];
        "#;
        let (ret, _) = run_vu(script, dummy(), None, None);
        assert_eq!(ret, "slept,slept");
    }

    #[test]
    fn fire_and_forget_async_still_fires_before_iteration_end() {
        let script = r#"
            globalThis.__fired = 'no';
            asyncSleep(10).then(function (v) { globalThis.__fired = v; });
            return 5;
        "#;
        let (ret, fired) = run_vu(script, dummy(), None, Some("__fired"));
        assert_eq!(ret, "5");
        assert_eq!(fired, "slept", "fire-and-forget must fire + settle before iteration end");
    }

    // --- real http.get on the driver loop, with the two metric buckets ---

    #[test]
    fn http_get_records_status_200() {
        let metrics = BuiltinMetrics::new();
        let (ret, _) = run_vu(
            "var r = httpGet('http://x/'); return String(r.status);",
            Arc::new(Mock200),
            Some(metrics.clone()),
            None,
        );
        assert_eq!(ret, "200", "sync http.get returns the resolved response");
        // Metrics recorded coroutine-side in finish_http_response, AFTER resume.
        assert_eq!(
            metrics
                .registry
                .counter_get("http_reqs{expected_response:true,method:GET,status:200}"),
            1,
            "success bucket recorded"
        );
    }

    #[test]
    fn http_get_transport_failure_records_status_0() {
        let metrics = BuiltinMetrics::new();
        let (ret, _) = run_vu(
            "var r = httpGet('http://x/'); return String(r.status) + ':' + (r.error !== '');",
            Arc::new(FailClient),
            Some(metrics.clone()),
            None,
        );
        assert_eq!(ret, "0:true", "transport failure surfaces as status 0 + error");
        // The Result::Err path reached finish_http_response and recorded the
        // failure bucket — the thing that silently rots if only 200 is tested.
        assert_eq!(
            metrics
                .registry
                .counter_get("http_reqs{expected_response:false,method:GET,status:0}"),
            1,
            "failure bucket recorded"
        );
    }

    // --- async http on the driver loop (asyncRequest) ---

    #[test]
    fn async_http_resolves_wrapped_response_and_records_once() {
        let metrics = BuiltinMetrics::new();
        // await the async request; the resolved value is wrapped (.json() works).
        let (ret, _) = run_vu(
            "var r = await asyncGet('http://x/'); return String(r.status) + ':' + r.json().ok;",
            Arc::new(Mock200),
            Some(metrics.clone()),
            None,
        );
        assert_eq!(ret, "200:true", "async http resolves a wrapped response");
        // Metrics recorded driver-loop-side, exactly once.
        assert_eq!(
            metrics
                .registry
                .counter_get("http_reqs{expected_response:true,method:GET,status:200}"),
            1,
            "async http records http_reqs exactly once, driver-loop-side"
        );
    }

    #[test]
    fn fire_and_forget_async_http_records_reqs_before_iteration_end() {
        // The conformance-critical case, now on REAL http_reqs (not the sleep
        // model): a no-await asyncRequest must still fire + record before the
        // iteration ends. Assert the METRIC, not just a JS global.
        let metrics = BuiltinMetrics::new();
        let (ret, _) = run_vu(
            "asyncGet('http://x/'); return 7;",
            Arc::new(Mock200),
            Some(metrics.clone()),
            None,
        );
        assert_eq!(ret, "7");
        assert_eq!(
            metrics
                .registry
                .counter_get("http_reqs{expected_response:true,method:GET,status:200}"),
            1,
            "fire-and-forget asyncRequest fired + recorded http_reqs before iteration end"
        );
    }

    #[test]
    fn async_http_completing_during_sync_park_records_once() {
        // Hazard on real http: an async http op completes while the VU is parked
        // in a sync sleep (borrow held) — queued, resolved after — and must
        // record http_reqs EXACTLY once (no double-record from park+redrain).
        let metrics = BuiltinMetrics::new();
        let script = r#"
            var got = null;
            var p = asyncGet('http://x/').then(function (r) { got = r.status; });
            var s = sleepMs(50);
            await p;
            return s + '|' + got;
        "#;
        let (ret, _) = run_vu(script, Arc::new(Mock200), Some(metrics.clone()), None);
        assert_eq!(ret, "s50|200");
        assert_eq!(
            metrics
                .registry
                .counter_get("http_reqs{expected_response:true,method:GET,status:200}"),
            1,
            "exactly one http_reqs despite completing during the park"
        );
    }
}
