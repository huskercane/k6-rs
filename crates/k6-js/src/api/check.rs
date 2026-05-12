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
///
/// CG-1: check() reads `globalThis.__current_group_path` (initialized here as
/// `''` so the variable always exists, and pushed/popped by `group()` — see
/// `register_group_with_metrics`) and threads the path through to
/// `__check_result`. The path is recorded with each check; CG-1 leaves the
/// summary group tree mostly flat under root, but the path is real from day
/// one so CG-2's tree assembly is a pure summary-time concern.
pub fn register_with_metrics(ctx: &Ctx<'_>, metrics: Option<BuiltinMetrics>) -> Result<()> {
    ctx.eval::<(), _>(r#"
        if (typeof globalThis.__current_group_path !== 'string') {
            globalThis.__current_group_path = '';
        }
        globalThis.check = function(val, checks, tags) {
            let allPassed = true;
            const groupPath = globalThis.__current_group_path || '';
            for (const name in checks) {
                let passed = false;
                try {
                    passed = !!checks[name](val);
                } catch (e) {
                    passed = false;
                }

                __check_result(name, passed, groupPath);
                if (!passed) {
                    allPassed = false;
                }
            }
            return allPassed;
        };
    "#)?;

    let globals = ctx.globals();
    globals.set(
        "__check_result",
        rquickjs::Function::new(
            ctx.clone(),
            move |name: String, passed: bool, group_path: String| {
                if let Some(ref m) = metrics {
                    m.record_check(&name, &group_path, passed);
                }
                if !passed {
                    eprintln!("  ✗ {name}");
                }
            },
        )?,
    )?;

    Ok(())
}

/// Register the k6 `group(name, fn)` function.
pub fn register_group(ctx: &Ctx<'_>) -> Result<()> {
    register_group_with_metrics(ctx, None)
}

/// Register group() with optional metrics recording.
///
/// CG-1: maintains `globalThis.__current_group_path` (initialized to `''` if
/// not yet set by `register_with_metrics`). On entry the new path is
/// `prev + '::' + name`, matching upstream k6's `GroupSeparator` convention
/// (lib/models.go). The `finally` block restores `prev` so an exception inside
/// `fn()` does not leak a stale path into subsequent checks.
///
/// CG-2: a dedicated `__group_enter(fullPath)` hook fires on entry so the
/// engine can register the group node *immediately*, before fn() runs. This
/// guarantees that `group('x', fn)` materializes `root_group.groups["x"]`
/// even if fn() throws before any check or duration recording — group
/// registration is now an explicit invariant, not a side effect of
/// `__group_end`. The exit hook `__group_end(fullPath, ms)` still fires
/// (also in `finally`) to record the per-group duration.
pub fn register_group_with_metrics(
    ctx: &Ctx<'_>,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    let entry_metrics = metrics.clone();
    ctx.globals().set(
        "__group_enter",
        rquickjs::Function::new(ctx.clone(), move |group_path: String| {
            if let Some(ref m) = entry_metrics {
                m.register_group(&group_path);
            }
        })?,
    )?;
    ctx.globals().set(
        "__group_end",
        rquickjs::Function::new(
            ctx.clone(),
            move |group_path: String, duration_ms: f64| {
                if let Some(ref m) = metrics {
                    m.record_group_duration(&group_path, duration_ms);
                }
            },
        )?,
    )?;

    ctx.eval::<(), _>(r#"
        if (typeof globalThis.__current_group_path !== 'string') {
            globalThis.__current_group_path = '';
        }
        globalThis.group = function(name, fn) {
            const prev = globalThis.__current_group_path || '';
            const fullPath = prev + '::' + name;
            globalThis.__current_group_path = fullPath;
            __group_enter(fullPath);
            const start = Date.now();
            try {
                return fn();
            } finally {
                globalThis.__current_group_path = prev;
                __group_end(fullPath, Date.now() - start);
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

    #[test]
    fn nested_group_path_is_threaded_and_restored() {
        // CG-1 regression: even though the summary still aggregates checks
        // under root for now, the JS-side group-path threading must already
        // be correct so CG-2 can light up the nested tree as a pure
        // summary-time concern. This test:
        //   1. asserts paths inside nested groups match upstream's `::A::B`
        //      convention (lib/models.go GroupSeparator),
        //   2. asserts `prev` is restored even when an exception escapes,
        //   3. asserts the global stays `''` at the top level.
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            register_group(&ctx).unwrap();
            register(&ctx).unwrap();

            // Observed paths recorded by a stub __check_result; we rely on
            // `globalThis.__current_group_path` being a plain string the JS
            // can read directly.
            let paths_json: String = ctx
                .eval(
                    r#"
                    const seen = [];
                    function trace(label) {
                        seen.push(label + '|' + globalThis.__current_group_path);
                    }

                    trace('top');
                    group('outer', function() {
                        trace('outer');
                        group('inner', function() {
                            trace('inner');
                        });
                        trace('after-inner');
                    });
                    trace('after-outer');

                    // Exception inside a group must still restore prev.
                    try {
                        group('boom', function() {
                            trace('inside-boom');
                            throw new Error('boom');
                        });
                    } catch (_) {}
                    trace('after-boom');

                    JSON.stringify(seen);
                "#,
                )
                .unwrap();

            let paths: Vec<String> = serde_json::from_str(&paths_json).unwrap();
            assert_eq!(
                paths,
                vec![
                    "top|".to_string(),
                    "outer|::outer".to_string(),
                    "inner|::outer::inner".to_string(),
                    "after-inner|::outer".to_string(),
                    "after-outer|".to_string(),
                    "inside-boom|::boom".to_string(),
                    "after-boom|".to_string(),
                ],
            );
        });
    }
}
