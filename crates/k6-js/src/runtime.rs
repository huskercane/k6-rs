use anyhow::{Context, Result};
use rquickjs::{Context as JsContext, Runtime};

/// Create a new QuickJS runtime with default configuration.
///
/// Each VU gets its own runtime for full isolation (no GC pauses crossing VUs).
pub fn create_runtime() -> Result<Runtime> {
    let runtime = Runtime::new().context("failed to create QuickJS runtime")?;

    // Set memory limit to prevent a single VU from consuming unbounded memory.
    // 64MB per VU is generous for k6-style scripts.
    runtime.set_memory_limit(64 * 1024 * 1024);

    // Set max stack size (256KB)
    runtime.set_max_stack_size(256 * 1024);

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
            ctx.eval::<(), _>(
                "globalThis.add = function(a, b) { return a + b; }",
            )
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
}
