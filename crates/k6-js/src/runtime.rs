use anyhow::{Context, Result};
use rquickjs::{AsyncContext, AsyncRuntime, Context as JsContext, Runtime};

/// Per-VU heap cap. Prevents a single VU from consuming unbounded memory; 64 MB
/// is generous for k6-style scripts. Kept **per VU** even under pool-of-loops
/// (many VUs share a loop thread but never a heap) — see `ASYNC_RUNTIME_PLAN.md`.
pub const VU_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Per-VU max JS stack (256 KB).
pub const VU_MAX_STACK: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Sync runtime — DELETED at the end of Phase 1 (async-runtime migration).
// Retained transiently so consumers can migrate file-by-file on this branch.
// ---------------------------------------------------------------------------

/// Create a new QuickJS runtime with default configuration.
///
/// Each VU gets its own runtime for full isolation (no GC pauses crossing VUs).
pub fn create_runtime() -> Result<Runtime> {
    let runtime = Runtime::new().context("failed to create QuickJS runtime")?;
    runtime.set_memory_limit(VU_MEMORY_LIMIT);
    runtime.set_max_stack_size(VU_MAX_STACK);
    Ok(runtime)
}

/// Create a new JS context within a runtime.
///
/// The context has the standard library (console, JSON, etc.) available.
pub fn create_context(runtime: &Runtime) -> Result<JsContext> {
    let ctx = JsContext::full(runtime).context("failed to create QuickJS context")?;
    Ok(ctx)
}

/// Run pending jobs on a runtime until there are none left.
pub fn drain_pending_jobs(runtime: &Runtime) {
    while runtime.execute_pending_job().is_ok_and(|more| more) {}
}

// ---------------------------------------------------------------------------
// Async runtime — the pool-of-loops foundation (Phase 0-proven).
//
// One isolated `AsyncRuntime` per VU (own heap + own 64 MB cap). Many VUs are
// hosted on one loop thread (current-thread tokio runtime + `LocalSet`); each
// VU's `drive()` task advances its spawned host futures + JS microtasks while
// the VU awaits. This is the mechanism validated in the Phase 0 spike.
// ---------------------------------------------------------------------------

/// Create an isolated per-VU async runtime with the standard per-VU limits.
///
/// Plain `async fn`: fallible construction (`?`) plus two sequenced limit-setter
/// awaits — this is *not* the infallible-single-tail-await shape, so it must not
/// masquerade as `fn -> impl Future`. No `Send` bound: the returned future is
/// `spawn_local`'d onto a `!Send` loop `LocalSet` and never crosses threads.
pub async fn create_async_runtime() -> Result<AsyncRuntime> {
    let rt = AsyncRuntime::new().context("failed to create async QuickJS runtime")?;
    rt.set_memory_limit(VU_MEMORY_LIMIT).await;
    rt.set_max_stack_size(VU_MAX_STACK).await;
    Ok(rt)
}

/// Create a full (stdlib) async JS context within an async runtime.
pub async fn create_async_context(runtime: &AsyncRuntime) -> Result<AsyncContext> {
    AsyncContext::full(runtime)
        .await
        .context("failed to create async QuickJS context")
}

/// Spawn the per-VU driver onto the current `LocalSet`.
///
/// MUST be called from within a `LocalSet` (i.e. on a loop thread). The returned
/// handle drives this runtime's spawned host futures + JS microtasks until it is
/// aborted or the runtime is dropped. Abort it when the VU is torn down.
///
/// Relies on `drive()` yielding an owned `'static` future — it holds a weak
/// handle to the runtime (`AsyncRuntime` is a cheap cloneable `Arc`), which is
/// what lets `spawn_local` (which demands `'static`) accept it.
pub fn spawn_driver(runtime: &AsyncRuntime) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_local(runtime.drive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_runtime_and_context() {
        let rt = create_runtime().unwrap();
        let ctx = create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let result: i32 = ctx.eval("1 + 2").unwrap();
            assert_eq!(result, 3);
        });
    }

    #[test]
    fn eval_basic_js() {
        let rt = create_runtime().unwrap();
        let ctx = create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let result: String = ctx.eval("'hello' + ' ' + 'world'").unwrap();
            assert_eq!(result, "hello world");
        });
    }

    #[test]
    fn eval_json_operations() {
        let rt = create_runtime().unwrap();
        let ctx = create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let result: String = ctx
                .eval("JSON.stringify({ status: 200, body: 'ok' })")
                .unwrap();
            assert_eq!(result, r#"{"status":200,"body":"ok"}"#);
        });
    }

    #[test]
    fn call_function() {
        let rt = create_runtime().unwrap();
        let ctx = create_context(&rt).unwrap();

        ctx.with(|ctx| {
            ctx.eval::<(), _>("globalThis.add = function(a, b) { return a + b; }")
                .unwrap();

            let globals = ctx.globals();
            let func: rquickjs::Function = globals.get("add").unwrap();
            let result: i32 = func.call((3, 4)).unwrap();
            assert_eq!(result, 7);
        });
    }

    #[test]
    fn globals_are_accessible() {
        let rt = create_runtime().unwrap();
        let ctx = create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let globals = ctx.globals();
            globals.set("__VU", 5).unwrap();
            globals.set("__ITER", 0).unwrap();

            let vu: i32 = ctx.eval("__VU").unwrap();
            assert_eq!(vu, 5);

            ctx.eval::<(), _>("__ITER = 42").unwrap();
            let iter: i32 = globals.get("__ITER").unwrap();
            assert_eq!(iter, 42);
        });
    }

    // --- async foundation ---

    use tokio::task::LocalSet;

    /// Build the single-threaded loop runtime a pool-of-loops executor thread owns.
    fn loop_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn async_runtime_evals_and_isolates() {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let qjs = create_async_runtime().await.unwrap();
            let ctx = create_async_context(&qjs).await.unwrap();

            // Basic eval on the async context.
            let sum: i32 = ctx.with(|ctx| ctx.eval("1 + 2").unwrap()).await;
            assert_eq!(sum, 3);

            // Second runtime is a separate heap: a global in one is invisible
            // in the other (the per-VU isolation pool-of-loops depends on).
            let qjs2 = create_async_runtime().await.unwrap();
            let ctx2 = create_async_context(&qjs2).await.unwrap();
            ctx.with(|ctx| ctx.eval::<(), _>("globalThis.x = 1").unwrap())
                .await;
            let leaked: bool = ctx2
                .with(|ctx| ctx.eval::<bool, _>("typeof globalThis.x !== 'undefined'").unwrap())
                .await;
            assert!(!leaked, "async runtimes must be isolated heaps");
        });
    }

    #[test]
    fn async_per_vu_memory_limit_is_isolated() {
        // Not just heap isolation — the *limit* is per-runtime. Exhausting one
        // VU's small cap must not affect another's. This guards the production
        // foundation for the property the throwaway async_spike proves today
        // (and which is deleted with it at the end of Phase 1).
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let small = AsyncRuntime::new().unwrap();
            small.set_memory_limit(2 * 1024 * 1024).await;
            let ctx_small = create_async_context(&small).await.unwrap();

            let big = create_async_runtime().await.unwrap(); // full 64 MB
            let ctx_big = create_async_context(&big).await.unwrap();

            let hit_limit: bool = ctx_small
                .with(|ctx| {
                    ctx.eval::<(), _>("let a=[]; for(let i=0;i<5_000_000;i++) a.push(i);")
                        .is_err()
                })
                .await;
            assert!(hit_limit, "small VU should hit its own 2 MB limit");

            let ok: i32 = ctx_big
                .with(|ctx| ctx.eval::<i32, _>("let a=[]; for(let i=0;i<1000;i++) a.push(i); a.length").unwrap())
                .await;
            assert_eq!(ok, 1000, "the other VU's heap must be unaffected");
        });
    }

    #[test]
    fn async_driver_advances_spawned_futures() {
        let rt = loop_runtime();
        LocalSet::new().block_on(&rt, async {
            let qjs = create_async_runtime().await.unwrap();
            let ctx = create_async_context(&qjs).await.unwrap();

            // Register an async host fn, then the driver must let a JS `await`
            // of it resolve.
            ctx.with(|ctx| {
                let f = rquickjs::Function::new(
                    ctx.clone(),
                    rquickjs::prelude::Async(|| async {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        21
                    }),
                )
                .unwrap();
                ctx.globals().set("host", f).unwrap();
            })
            .await;

            let driver = spawn_driver(&qjs);

            let result: i32 = ctx
                .async_with(async |ctx| {
                    let p: rquickjs::Promise = ctx.eval("host().then(v => v * 2)").unwrap();
                    p.into_future().await.unwrap()
                })
                .await;
            assert_eq!(result, 42);

            driver.abort();
        });
    }
}
