use anyhow::Result;
use rquickjs::Ctx;

use k6_core::metrics::BuiltinMetrics;

/// Register the k6 `check(val, checks, tags)` function.
///
/// `checks` is an object where keys are check names and values are
/// functions that receive `val` and return a boolean.
/// Returns `true` if all checks pass.
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    register_with_metrics(ctx, None)
}

/// Register check() with optional metrics recording.
pub fn register_with_metrics(ctx: &Ctx<'_>, metrics: Option<BuiltinMetrics>) -> Result<()> {
    ctx.eval::<(), _>(r#"
        globalThis.check = function(val, checks, tags) {
            let allPassed = true;
            for (const name in checks) {
                let passed = false;
                try {
                    passed = !!checks[name](val);
                } catch (e) {
                    passed = false;
                }

                if (passed) {
                    __check_result(name, true);
                } else {
                    __check_result(name, false);
                    allPassed = false;
                }
            }
            return allPassed;
        };
    "#)?;

    let globals = ctx.globals();
    globals.set(
        "__check_result",
        rquickjs::Function::new(ctx.clone(), move |name: String, passed: bool| {
            if let Some(ref m) = metrics {
                m.record_check(passed);
            }
            if !passed {
                eprintln!("  ✗ {name}");
            }
        })?,
    )?;

    Ok(())
}

/// Register the k6 `group(name, fn)` function.
pub fn register_group(ctx: &Ctx<'_>) -> Result<()> {
    register_group_with_metrics(ctx, None)
}

/// Register group() with optional metrics recording.
pub fn register_group_with_metrics(
    ctx: &Ctx<'_>,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    ctx.globals().set(
        "__group_end",
        rquickjs::Function::new(ctx.clone(), move |_name: String, duration_ms: f64| {
            if let Some(ref m) = metrics {
                m.record_group_duration(duration_ms);
            }
        })?,
    )?;

    ctx.eval::<(), _>(r#"
        globalThis.group = function(name, fn) {
            const start = Date.now();
            try {
                return fn();
            } finally {
                __group_end(name, Date.now() - start);
            }
        };
    "#)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    #[test]
    fn check_all_pass() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            register(&ctx).unwrap();

            let result: bool = ctx
                .eval(r#"
                    check({ status: 200 }, {
                        'status is 200': (r) => r.status === 200,
                        'has status': (r) => r.status !== undefined,
                    })
                "#)
                .unwrap();

            assert!(result);
        });
    }

    #[test]
    fn check_some_fail() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            register(&ctx).unwrap();

            let result: bool = ctx
                .eval(r#"
                    check({ status: 500 }, {
                        'status is 200': (r) => r.status === 200,
                        'has status': (r) => r.status !== undefined,
                    })
                "#)
                .unwrap();

            assert!(!result);
        });
    }

    #[test]
    fn check_with_exception_in_check_fn() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            register(&ctx).unwrap();

            let result: bool = ctx
                .eval(r#"
                    check(null, {
                        'throws': (r) => r.nonexistent.property,
                    })
                "#)
                .unwrap();

            // Exception in check function → treated as failure
            assert!(!result);
        });
    }

    #[test]
    fn group_runs_function() {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            register_group(&ctx).unwrap();

            let result: i32 = ctx
                .eval(r#"
                    group('test group', function() {
                        return 42;
                    })
                "#)
                .unwrap();

            assert_eq!(result, 42);
        });
    }
}
