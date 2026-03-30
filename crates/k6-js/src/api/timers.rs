use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rquickjs::{Ctx, Function};

/// Register k6/timers functions.
///
/// Provides: setTimeout, clearTimeout, setInterval, clearInterval.
///
/// In k6, timers are synchronous within a VU's execution. setTimeout schedules
/// a callback that runs after the specified delay. Since each VU is single-threaded
/// (running in spawn_blocking), std::thread::sleep is the right primitive.
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    let globals = ctx.globals();

    // Shared set of cleared timer IDs
    let cleared: Arc<Mutex<std::collections::HashSet<i32>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // __timer_sleep_ms: synchronous thread sleep
    globals.set(
        "__timer_sleep_ms",
        Function::new(ctx.clone(), |ms: f64| {
            let dur = Duration::from_millis(ms.max(0.0) as u64);
            std::thread::sleep(dur);
        })?,
    )?;

    // __timer_is_cleared: check if timer ID has been cleared
    {
        let cleared = Arc::clone(&cleared);
        globals.set(
            "__timer_is_cleared",
            Function::new(ctx.clone(), move |id: i32| -> bool {
                cleared.lock().unwrap().contains(&id)
            })?,
        )?;
    }

    // __timer_clear: mark a timer ID as cleared
    {
        let cleared = Arc::clone(&cleared);
        globals.set(
            "__timer_clear",
            Function::new(ctx.clone(), move |id: i32| {
                cleared.lock().unwrap().insert(id);
            })?,
        )?;
    }

    // __timer_unmark: remove from cleared set (cleanup)
    {
        let cleared = Arc::clone(&cleared);
        globals.set(
            "__timer_unmark",
            Function::new(ctx.clone(), move |id: i32| {
                cleared.lock().unwrap().remove(&id);
            })?,
        )?;
    }

    // JS-level timer implementation — fully synchronous
    // In k6, VUs run in a blocking context (spawn_blocking), so
    // setTimeout/setInterval blocking the thread is correct behavior.
    ctx.eval::<(), _>(
        r#"
        (function() {
            var _nextTimerId = 1;

            globalThis.setTimeout = function(callback, delay) {
                var id = _nextTimerId++;
                var ms = Number(delay) || 0;
                __timer_sleep_ms(ms);
                if (!__timer_is_cleared(id)) {
                    callback();
                }
                __timer_unmark(id);
                return id;
            };

            globalThis.clearTimeout = function(id) {
                __timer_clear(id);
            };

            globalThis.setInterval = function(callback, delay) {
                var id = _nextTimerId++;
                var ms = Number(delay) || 0;
                while (!__timer_is_cleared(id)) {
                    __timer_sleep_ms(ms);
                    if (!__timer_is_cleared(id)) {
                        callback();
                    }
                }
                __timer_unmark(id);
                return id;
            };

            globalThis.clearInterval = function(id) {
                __timer_clear(id);
            };
        })();
    "#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    fn with_ctx(f: impl FnOnce(&Ctx<'_>)) {
        let qjs_rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&qjs_rt).unwrap();
        ctx.with(|ctx| {
            register(&ctx).unwrap();
            f(&ctx);
        });
    }

    #[test]
    fn set_timeout_fires() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__timer_result = 0;
                setTimeout(function() { globalThis.__timer_result = 42; }, 10);
            "#,
            )
            .unwrap();

            let result: i32 = ctx.eval("__timer_result").unwrap();
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn clear_timeout_prevents_fire() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__timer_result = 0;
                // Pre-clear the next timer ID
                __timer_clear(1);
                setTimeout(function() { globalThis.__timer_result = 99; }, 10);
            "#,
            )
            .unwrap();

            let result: i32 = ctx.eval("__timer_result").unwrap();
            assert_eq!(result, 0);
        });
    }

    #[test]
    fn set_interval_fires_multiple_times() {
        with_ctx(|ctx| {
            // Since setInterval is synchronous and blocks until cleared,
            // we need to know the ID ahead of time. The next ID after
            // no prior allocations in this context will be 1.
            ctx.eval::<(), _>(
                r#"
                globalThis.__interval_count = 0;
                // We know the next timer ID will be 1
                setInterval(function() {
                    globalThis.__interval_count++;
                    if (globalThis.__interval_count >= 3) {
                        __timer_clear(1);
                    }
                }, 10);
            "#,
            )
            .unwrap();

            let count: i32 = ctx.eval("__interval_count").unwrap();
            assert_eq!(count, 3);
        });
    }

    #[test]
    fn set_timeout_returns_id() {
        with_ctx(|ctx| {
            let id: i32 = ctx.eval("setTimeout(function(){}, 0)").unwrap();
            assert!(id > 0);
        });
    }

    #[test]
    fn multiple_timeouts_different_ids() {
        with_ctx(|ctx| {
            let id1: i32 = ctx.eval("setTimeout(function(){}, 0)").unwrap();
            let id2: i32 = ctx.eval("setTimeout(function(){}, 0)").unwrap();
            assert_ne!(id1, id2);
        });
    }
}
