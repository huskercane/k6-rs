use anyhow::Result;
use rquickjs::Ctx;

/// Register k6/execution module.
///
/// Provides execution context objects: execution.scenario, execution.vu,
/// execution.instance, execution.test.
///
/// The objects read from globals set by the executor:
/// - __VU, __ITER (already set)
/// - __EXEC_VU_ID_IN_TEST, __EXEC_VU_ITERATION_IN_SCENARIO
/// - __EXEC_SCENARIO_NAME, __EXEC_SCENARIO_EXECUTOR, __EXEC_SCENARIO_START_TIME
/// - __EXEC_SCENARIO_PROGRESS, __EXEC_SCENARIO_ITERATION_IN_INSTANCE
/// - __EXEC_SCENARIO_ITERATION_IN_TEST
/// - __EXEC_TEST_ABORT (function to set abort flag)
pub fn register(ctx: &Ctx<'_>) -> Result<()> {
    ctx.eval::<(), _>(
        r#"
        var execution = {
            scenario: {
                get name() {
                    return typeof __EXEC_SCENARIO_NAME !== 'undefined' ? __EXEC_SCENARIO_NAME : '';
                },
                get executor() {
                    return typeof __EXEC_SCENARIO_EXECUTOR !== 'undefined' ? __EXEC_SCENARIO_EXECUTOR : '';
                },
                get startTime() {
                    return typeof __EXEC_SCENARIO_START_TIME !== 'undefined' ? __EXEC_SCENARIO_START_TIME : Date.now();
                },
                get progress() {
                    return typeof __EXEC_SCENARIO_PROGRESS !== 'undefined' ? __EXEC_SCENARIO_PROGRESS : 0;
                },
                get iterationInInstance() {
                    return typeof __EXEC_SCENARIO_ITERATION_IN_INSTANCE !== 'undefined' ? __EXEC_SCENARIO_ITERATION_IN_INSTANCE : __ITER;
                },
                get iterationInTest() {
                    return typeof __EXEC_SCENARIO_ITERATION_IN_TEST !== 'undefined' ? __EXEC_SCENARIO_ITERATION_IN_TEST : __ITER;
                }
            },
            vu: {
                get idInInstance() { return __VU; },
                get idInTest() {
                    return typeof __EXEC_VU_ID_IN_TEST !== 'undefined' ? __EXEC_VU_ID_IN_TEST : __VU;
                },
                get iterationInInstance() { return __ITER; },
                get iterationInScenario() {
                    return typeof __EXEC_VU_ITERATION_IN_SCENARIO !== 'undefined' ? __EXEC_VU_ITERATION_IN_SCENARIO : __ITER;
                },
                get tags() {
                    return typeof __EXEC_VU_TAGS !== 'undefined' ? __EXEC_VU_TAGS : {};
                },
                set tags(v) {
                    globalThis.__EXEC_VU_TAGS = v;
                },
                get metrics() {
                    return { tags: this.tags };
                }
            },
            instance: {
                get iterationsCompleted() {
                    return typeof __EXEC_INSTANCE_ITERATIONS !== 'undefined' ? __EXEC_INSTANCE_ITERATIONS : 0;
                },
                get iterationsInterrupted() {
                    return typeof __EXEC_INSTANCE_INTERRUPTED !== 'undefined' ? __EXEC_INSTANCE_INTERRUPTED : 0;
                },
                get vusActive() {
                    return typeof __EXEC_INSTANCE_VUS_ACTIVE !== 'undefined' ? __EXEC_INSTANCE_VUS_ACTIVE : 0;
                },
                get vusInitialized() {
                    return typeof __EXEC_INSTANCE_VUS_INIT !== 'undefined' ? __EXEC_INSTANCE_VUS_INIT : 0;
                },
                get currentTestRunDuration() {
                    return typeof __EXEC_INSTANCE_DURATION !== 'undefined' ? __EXEC_INSTANCE_DURATION : 0;
                }
            },
            test: {
                abort: function(reason) {
                    if (typeof __exec_test_abort === 'function') {
                        __exec_test_abort(reason || '');
                    } else {
                        throw new Error('test.abort: ' + (reason || 'aborted'));
                    }
                },
                get options() {
                    return typeof __k6_options !== 'undefined' ? __k6_options : {};
                }
            }
        };
        globalThis.execution = execution;
    "#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    fn with_ctx(f: impl FnOnce(&Ctx<'_>)) {
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        ctx.with(|ctx| {
            // Set up minimal globals like a VU would have
            ctx.eval::<(), _>("var __VU = 5; var __ITER = 3;").unwrap();
            register(&ctx).unwrap();
            f(&ctx);
        });
    }

    #[test]
    fn vu_id() {
        with_ctx(|ctx| {
            let id: i32 = ctx.eval("execution.vu.idInInstance").unwrap();
            assert_eq!(id, 5);
        });
    }

    #[test]
    fn vu_iteration() {
        with_ctx(|ctx| {
            let iter: i32 = ctx.eval("execution.vu.iterationInInstance").unwrap();
            assert_eq!(iter, 3);
        });
    }

    #[test]
    fn vu_explicit_test_id_and_scenario_iteration_override_fallbacks() {
        // Port of the k6/internal/execution execution-info contract: VU IDs
        // and per-scenario VU iterations can differ from local __VU/__ITER
        // once multiple scenarios or distributed execution are involved.
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__EXEC_VU_ID_IN_TEST = 17;
                globalThis.__EXEC_VU_ITERATION_IN_SCENARIO = 9;
                "#,
            )
            .unwrap();

            let id: i32 = ctx.eval("execution.vu.idInTest").unwrap();
            assert_eq!(id, 17);

            let iter: i32 = ctx.eval("execution.vu.iterationInScenario").unwrap();
            assert_eq!(iter, 9);
        });
    }

    #[test]
    fn scenario_defaults() {
        with_ctx(|ctx| {
            let name: String = ctx.eval("execution.scenario.name").unwrap();
            assert_eq!(name, "");

            let progress: f64 = ctx.eval("execution.scenario.progress").unwrap();
            assert_eq!(progress, 0.0);
        });
    }

    #[test]
    fn scenario_with_context() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                "globalThis.__EXEC_SCENARIO_NAME = 'load_test'; globalThis.__EXEC_SCENARIO_EXECUTOR = 'ramping-vus';",
            )
            .unwrap();

            let name: String = ctx.eval("execution.scenario.name").unwrap();
            assert_eq!(name, "load_test");

            let executor: String = ctx.eval("execution.scenario.executor").unwrap();
            assert_eq!(executor, "ramping-vus");
        });
    }

    #[test]
    fn scenario_iteration_globals_are_distinct_from_vu_iteration() {
        // Mirrors upstream TestExecutionInfoScenarioIter /
        // TestSharedIterationsStable at the API boundary: scenario iteration
        // counters are scheduler-assigned global counters, not aliases for the
        // VU-local __ITER when the scheduler supplies explicit values.
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__EXEC_SCENARIO_ITERATION_IN_INSTANCE = 21;
                globalThis.__EXEC_SCENARIO_ITERATION_IN_TEST = 34;
                "#,
            )
            .unwrap();

            let vu_iter: i32 = ctx.eval("execution.vu.iterationInInstance").unwrap();
            assert_eq!(vu_iter, 3);

            let instance_iter: i32 = ctx.eval("execution.scenario.iterationInInstance").unwrap();
            assert_eq!(instance_iter, 21);

            let test_iter: i32 = ctx.eval("execution.scenario.iterationInTest").unwrap();
            assert_eq!(test_iter, 34);
        });
    }

    #[test]
    fn execution_info_all_top_level_objects_are_readable() {
        // Port of upstream TestExecutionInfoAll's smoke assertion. The exact
        // time-based fields are intentionally not asserted here; this catches
        // broken getters or missing top-level execution objects.
        with_ctx(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__EXEC_SCENARIO_EXECUTOR = 'constant-vus';
                globalThis.__EXEC_INSTANCE_VUS_ACTIVE = 1;
                "#,
            )
            .unwrap();

            let executor: String = ctx.eval("execution.scenario.executor").unwrap();
            assert_eq!(executor, "constant-vus");

            let active: i32 = ctx.eval("execution.instance.vusActive").unwrap();
            assert_eq!(active, 1);

            let vu_id: i32 = ctx.eval("execution.vu.idInTest").unwrap();
            assert_eq!(vu_id, 5);
        });
    }

    #[test]
    fn instance_defaults() {
        with_ctx(|ctx| {
            let completed: i32 = ctx.eval("execution.instance.iterationsCompleted").unwrap();
            assert_eq!(completed, 0);
        });
    }

    #[test]
    fn test_abort_throws() {
        with_ctx(|ctx| {
            let result: Result<(), _> = ctx.eval::<(), _>("execution.test.abort('test stopped')");
            assert!(result.is_err());
        });
    }

    #[test]
    fn vu_tags_read_write() {
        with_ctx(|ctx| {
            ctx.eval::<(), _>("execution.vu.tags = { name: 'login' };")
                .unwrap();
            let name: String = ctx.eval("execution.vu.tags.name").unwrap();
            assert_eq!(name, "login");
        });
    }
}
