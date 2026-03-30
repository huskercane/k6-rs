use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use rquickjs::{CatchResultExt, Context as JsContext, Function};

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, IterationResult, VirtualUser};

use crate::runtime;

/// A virtual user backed by a QuickJS JS context.
///
/// Each VU owns its own QuickJS Runtime + Context for full isolation.
/// The script is compiled once and the default function is called
/// repeatedly for each iteration.
pub struct QuickJsVu {
    ctx: JsContext,
    rt: rquickjs::Runtime,
    vu_id: u32,
    iteration: u32,
    metrics: Option<BuiltinMetrics>,
    /// Which function to call per iteration. None = "__k6_default", Some("fn") = named export.
    exec_fn: Option<String>,
}

impl QuickJsVu {
    /// Create a new VU with the given ID and script source.
    ///
    /// The script should already be transformed via `prepare_script()`.
    /// `export default function` should be rewritten to set `globalThis.__k6_default`.
    pub fn new(vu_id: u32, script: &str, env: &[(String, String)]) -> Result<Self> {
        let rt = runtime::create_runtime()?;
        let ctx = runtime::create_context(&rt)?;

        ctx.with(|ctx| {
            // Set k6 globals
            let globals = ctx.globals();
            globals
                .set("__VU", vu_id)
                .context("failed to set __VU")?;
            globals
                .set("__ITER", 0i32)
                .context("failed to set __ITER")?;

            // Set __ENV
            let env_obj = rquickjs::Object::new(ctx.clone()).context("failed to create __ENV")?;
            for (key, val) in env {
                env_obj.set(key.as_str(), val.as_str())?;
            }
            globals.set("__ENV", env_obj)?;

            // Register console.log/warn/error
            Self::register_console(&ctx, None)?;

            // Register Phase 3 modules (no dependencies)
            crate::api::encoding::register(&ctx)?;
            crate::api::crypto::register(&ctx)?;
            crate::api::execution::register(&ctx)?;
            crate::api::html::register(&ctx)?;
            crate::api::secrets::register(&ctx)?;
            crate::api::csv::register(&ctx)?;
            crate::api::fs::register(&ctx)?;
            crate::api::streams::register(&ctx)?;
            crate::api::webcrypto::register(&ctx)?;

            // Register fail() and randomSeed()
            Self::register_fail(&ctx)?;
            Self::register_random_seed(&ctx)?;

            // Evaluate the script
            ctx.eval::<(), _>(script)
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("script evaluation error: {e:?}"))?;

            Ok::<_, anyhow::Error>(())
        })?;

        // Run pending jobs
        runtime::drain_pending_jobs(&rt);

        // Verify __k6_default exists
        let has_default = ctx.with(|ctx| {
            ctx.globals()
                .get::<_, rquickjs::Value>("__k6_default")
                .map(|v| v.is_function())
                .unwrap_or(false)
        });

        if !has_default {
            anyhow::bail!(
                "script must export a default function. \
                 Use: export default function() {{ ... }}"
            );
        }

        Ok(Self {
            ctx,
            rt,
            vu_id,
            iteration: 0,
            metrics: None,
            exec_fn: None,
        })
    }

    /// Create a new VU with full k6 API support (HTTP, check, sleep, group).
    ///
    /// This is the constructor used in production. The script should be
    /// pre-processed via `prepare_script()`.
    pub fn new_with_http<C: HttpClient + 'static>(
        vu_id: u32,
        script: &str,
        env: &[(String, String)],
        handle: tokio::runtime::Handle,
        client: Arc<C>,
        backpressure: Backpressure,
    ) -> Result<Self> {
        Self::new_with_http_and_metrics(vu_id, script, env, handle, client, backpressure, None)
    }

    /// Create a new VU with full k6 API and metrics collection.
    pub fn new_with_http_and_metrics<C: HttpClient + 'static>(
        vu_id: u32,
        script: &str,
        env: &[(String, String)],
        handle: tokio::runtime::Handle,
        client: Arc<C>,
        backpressure: Backpressure,
        metrics: Option<BuiltinMetrics>,
    ) -> Result<Self> {
        Self::new_full(vu_id, script, env, handle, client, backpressure, metrics, None)
    }

    /// Create a new VU with full k6 API, metrics, and file system access.
    ///
    /// `script_dir` enables `open()` for reading files relative to the script location.
    pub fn new_full<C: HttpClient + 'static>(
        vu_id: u32,
        script: &str,
        env: &[(String, String)],
        handle: tokio::runtime::Handle,
        client: Arc<C>,
        backpressure: Backpressure,
        metrics: Option<BuiltinMetrics>,
        script_dir: Option<PathBuf>,
    ) -> Result<Self> {
        Self::new_full_with_console(
            vu_id,
            script,
            env,
            handle,
            client,
            backpressure,
            metrics,
            script_dir,
            None,
        )
    }

    /// Create a new VU with all options including console output redirection.
    pub fn new_full_with_console<C: HttpClient + 'static>(
        vu_id: u32,
        script: &str,
        env: &[(String, String)],
        handle: tokio::runtime::Handle,
        client: Arc<C>,
        backpressure: Backpressure,
        metrics: Option<BuiltinMetrics>,
        script_dir: Option<PathBuf>,
        console_output: Option<Arc<std::sync::Mutex<std::fs::File>>>,
    ) -> Result<Self> {
        let rt = runtime::create_runtime()?;
        let ctx = runtime::create_context(&rt)?;

        ctx.with(|ctx| {
            let globals = ctx.globals();
            globals.set("__VU", vu_id).context("failed to set __VU")?;
            globals.set("__ITER", 0i32).context("failed to set __ITER")?;

            let env_obj = rquickjs::Object::new(ctx.clone())?;
            for (key, val) in env {
                env_obj.set(key.as_str(), val.as_str())?;
            }
            globals.set("__ENV", env_obj)?;

            Self::register_console(&ctx, console_output)?;

            // Register Phase 3 modules (no dependencies)
            crate::api::encoding::register(&ctx)?;
            crate::api::crypto::register(&ctx)?;
            crate::api::execution::register(&ctx)?;
            crate::api::html::register(&ctx)?;
            crate::api::secrets::register(&ctx)?;
            crate::api::csv::register(&ctx)?;
            crate::api::fs::register(&ctx)?;
            crate::api::streams::register(&ctx)?;
            crate::api::webcrypto::register(&ctx)?;

            // Register fail() and randomSeed()
            Self::register_fail(&ctx)?;
            Self::register_random_seed(&ctx)?;

            // Register open() if script directory is known
            if let Some(dir) = script_dir {
                Self::register_open(&ctx, dir)?;
            }

            // Register all k6 API shims
            crate::api::http::register_with_metrics(
                &ctx,
                handle.clone(),
                client,
                backpressure,
                metrics.clone(),
            )?;
            crate::api::check::register_with_metrics(&ctx, metrics.clone())?;
            crate::api::check::register_group_with_metrics(&ctx, metrics.clone())?;
            crate::api::sleep::register(&ctx, handle.clone())?;
            crate::api::timers::register(&ctx)?;

            // Register WebSocket support (k6/ws)
            crate::api::ws::register(&ctx, handle.clone(), metrics.clone())?;

            // Register gRPC support (k6/net/grpc)
            crate::api::grpc::register(&ctx, handle, metrics.clone())?;

            // Register custom metrics constructors (Trend, Counter, Rate, Gauge)
            if let Some(ref m) = metrics {
                crate::api::metrics::register(&ctx, m.registry.clone())?;
            }

            // Evaluate the script
            ctx.eval::<(), _>(script)
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("script evaluation error: {e:?}"))?;

            Ok::<_, anyhow::Error>(())
        })?;

        runtime::drain_pending_jobs(&rt);

        // Disable open() in VU context (init is done)
        ctx.with(|ctx| {
            let _ = ctx.globals().set("__open_init_only", false);
        });

        let has_default = ctx.with(|ctx| {
            ctx.globals()
                .get::<_, rquickjs::Value>("__k6_default")
                .map(|v| v.is_function())
                .unwrap_or(false)
        });

        if !has_default {
            anyhow::bail!(
                "script must export a default function. \
                 Use: export default function() {{ ... }}"
            );
        }

        Ok(Self {
            ctx,
            rt,
            vu_id,
            iteration: 0,
            metrics,
            exec_fn: None,
        })
    }

    /// Attach metrics collection to this VU.
    pub fn set_metrics(&mut self, metrics: BuiltinMetrics) {
        self.metrics = Some(metrics);
    }

    fn register_console(
        ctx: &rquickjs::Ctx<'_>,
        console_output: Option<Arc<std::sync::Mutex<std::fs::File>>>,
    ) -> Result<()> {
        // Use JS wrapper that calls String() on each argument,
        // so we always receive strings in the Rust callback.
        ctx.eval::<(), _>(r#"
            globalThis.console = {
                log: function() {
                    let parts = [];
                    for (let i = 0; i < arguments.length; i++) {
                        parts.push(String(arguments[i]));
                    }
                    __console_log(parts.join(' '));
                },
                warn: function() {
                    let parts = [];
                    for (let i = 0; i < arguments.length; i++) {
                        parts.push(String(arguments[i]));
                    }
                    __console_warn(parts.join(' '));
                },
                error: function() {
                    let parts = [];
                    for (let i = 0; i < arguments.length; i++) {
                        parts.push(String(arguments[i]));
                    }
                    __console_error(parts.join(' '));
                },
            };
        "#)?;

        let globals = ctx.globals();

        if let Some(ref file) = console_output {
            let f = Arc::clone(file);
            globals.set(
                "__console_log",
                Function::new(ctx.clone(), move |msg: String| {
                    use std::io::Write;
                    if let Ok(mut f) = f.lock() {
                        let _ = writeln!(f, "{msg}");
                    }
                })?,
            )?;

            let f = Arc::clone(file);
            globals.set(
                "__console_warn",
                Function::new(ctx.clone(), move |msg: String| {
                    use std::io::Write;
                    if let Ok(mut f) = f.lock() {
                        let _ = writeln!(f, "WARN: {msg}");
                    }
                })?,
            )?;

            let f = Arc::clone(file);
            globals.set(
                "__console_error",
                Function::new(ctx.clone(), move |msg: String| {
                    use std::io::Write;
                    if let Ok(mut f) = f.lock() {
                        let _ = writeln!(f, "ERROR: {msg}");
                    }
                })?,
            )?;
        } else {
            globals.set(
                "__console_log",
                Function::new(ctx.clone(), |msg: String| {
                    println!("{msg}");
                })?,
            )?;

            globals.set(
                "__console_warn",
                Function::new(ctx.clone(), |msg: String| {
                    eprintln!("WARN: {msg}");
                })?,
            )?;

            globals.set(
                "__console_error",
                Function::new(ctx.clone(), |msg: String| {
                    eprintln!("ERROR: {msg}");
                })?,
            )?;
        }

        Ok(())
    }

    fn register_open(ctx: &rquickjs::Ctx<'_>, script_dir: PathBuf) -> Result<()> {
        let globals = ctx.globals();

        // __open_init_only flag — set to true during init, false during VU execution
        globals.set("__open_init_only", true)?;

        globals.set(
            "__open_file",
            Function::new(ctx.clone(), move |path: String| -> rquickjs::Result<String> {
                let resolved = if Path::new(&path).is_absolute() {
                    PathBuf::from(&path)
                } else {
                    script_dir.join(&path)
                };
                std::fs::read_to_string(&resolved).map_err(|e| {
                    rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("open({path}): {e}"),
                    )
                })
            })?,
        )?;

        ctx.eval::<(), _>(
            r#"
            globalThis.open = function(path, mode) {
                if (!__open_init_only) {
                    throw new Error('open() can only be called in the init context (i.e. the global scope), not inside VU code');
                }
                return __open_file(String(path));
            };
        "#,
        )?;
        Ok(())
    }

    fn register_fail(ctx: &rquickjs::Ctx<'_>) -> Result<()> {
        ctx.eval::<(), _>(
            r#"
            globalThis.fail = function(msg) {
                throw new Error('fail: ' + (msg || 'test aborted'));
            };
        "#,
        )?;
        Ok(())
    }

    fn register_random_seed(ctx: &rquickjs::Ctx<'_>) -> Result<()> {
        // randomSeed() replaces Math.random with a seeded PRNG (xorshift32)
        // for reproducible test runs, matching k6 behavior.
        ctx.eval::<(), _>(
            r#"
            globalThis.randomSeed = function(seed) {
                var state = seed | 0;
                if (state === 0) state = 1;
                Math.random = function() {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    return (state >>> 0) / 4294967296;
                };
            };
        "#,
        )?;
        Ok(())
    }

    /// Get the VU ID.
    pub fn vu_id(&self) -> u32 {
        self.vu_id
    }

    /// Get current iteration count.
    pub fn iteration(&self) -> u32 {
        self.iteration
    }

    /// Set the function name to call per iteration (for `exec` scenario option).
    pub fn set_exec_fn(&mut self, name: &str) {
        self.exec_fn = Some(name.to_string());
    }

    /// Check if the script defines a handleSummary() function.
    pub fn has_handle_summary(&self) -> bool {
        self.ctx.with(|ctx| {
            ctx.globals()
                .get::<_, rquickjs::Value>("handleSummary")
                .map(|v| v.is_function())
                .unwrap_or(false)
        })
    }

    /// Execute handleSummary(data) and return the output map.
    ///
    /// Returns a map of destination → content:
    /// - "stdout" → printed to stdout
    /// - "stderr" → printed to stderr
    /// - "path/to/file" → written to file
    pub fn run_handle_summary(&mut self, data_json: &str) -> Result<Vec<(String, String)>> {
        let data_json = data_json.to_string();
        let results = self.ctx.with(|ctx| -> Result<Vec<(String, String)>> {
            let globals = ctx.globals();
            let func: Function = globals
                .get("handleSummary")
                .context("handleSummary not found")?;

            // Parse data and call handleSummary
            let data: rquickjs::Value = ctx
                .eval::<rquickjs::Value, _>(format!("JSON.parse('{}')", data_json.replace('\\', "\\\\").replace('\'', "\\'")))
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("failed to parse summary data: {e:?}"))?;

            let result: rquickjs::Value = func
                .call((data,))
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("handleSummary() error: {e:?}"))?;

            // Result should be an object: { "stdout": "...", "file.json": "..." }
            let mut outputs = Vec::new();
            if let Some(obj) = result.as_object() {
                for entry in obj.props::<String, String>() {
                    if let Ok((key, val)) = entry {
                        outputs.push((key, val));
                    }
                }
            }

            Ok(outputs)
        })?;

        runtime::drain_pending_jobs(&self.rt);
        Ok(results)
    }

    /// Check if the script defines a setup() function.
    pub fn has_setup(&self) -> bool {
        self.ctx.with(|ctx| {
            ctx.globals()
                .get::<_, rquickjs::Value>("setup")
                .map(|v| v.is_function())
                .unwrap_or(false)
        })
    }

    /// Check if the script defines a teardown() function.
    pub fn has_teardown(&self) -> bool {
        self.ctx.with(|ctx| {
            ctx.globals()
                .get::<_, rquickjs::Value>("teardown")
                .map(|v| v.is_function())
                .unwrap_or(false)
        })
    }

    /// Execute setup() and return the JSON-serialized result.
    /// The result is stored as `__k6_setup_data` for use in iterations.
    pub fn run_setup(&mut self) -> Result<Option<String>> {
        let result = self.ctx.with(|ctx| -> Result<Option<String>> {
            let globals = ctx.globals();
            let setup_fn: Function = match globals.get("setup") {
                Ok(f) => f,
                Err(_) => return Ok(None),
            };

            let result: rquickjs::Value = setup_fn
                .call(())
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("setup() error: {e:?}"))?;

            // Serialize the return value to JSON for sharing across VUs
            if result.is_undefined() || result.is_null() {
                Ok(None)
            } else {
                let json_fn: Function = ctx.eval("JSON.stringify").unwrap();
                let json: String = json_fn
                    .call((result,))
                    .catch(&ctx)
                    .map_err(|e| anyhow::anyhow!("failed to serialize setup data: {e:?}"))?;
                Ok(Some(json))
            }
        })?;

        runtime::drain_pending_jobs(&self.rt);

        // Store setup data as global for this VU
        if let Some(ref json) = result {
            self.set_setup_data(json)?;
        }

        Ok(result)
    }

    /// Set the setup data from JSON string (for VUs that didn't run setup themselves).
    pub fn set_setup_data(&mut self, json: &str) -> Result<()> {
        let json_owned = json.to_string();
        self.ctx.with(|ctx| -> Result<()> {
            ctx.eval::<(), _>(format!(
                "globalThis.__k6_setup_data = JSON.parse('{}');",
                json_owned.replace('\\', "\\\\").replace('\'', "\\'")
            ))
            .catch(&ctx)
            .map_err(|e| anyhow::anyhow!("failed to set setup data: {e:?}"))?;
            Ok(())
        })
    }

    /// Execute teardown(data).
    pub fn run_teardown(&mut self) -> Result<()> {
        self.ctx.with(|ctx| -> Result<()> {
            let globals = ctx.globals();
            let teardown_fn: Function = match globals.get("teardown") {
                Ok(f) => f,
                Err(_) => return Ok(()),
            };

            let data: rquickjs::Value = ctx
                .eval("typeof __k6_setup_data !== 'undefined' ? __k6_setup_data : undefined")
                .unwrap_or(rquickjs::Value::new_undefined(ctx.clone()));

            teardown_fn
                .call::<_, rquickjs::Value>((data,))
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("teardown() error: {e:?}"))?;

            Ok(())
        })?;

        runtime::drain_pending_jobs(&self.rt);
        Ok(())
    }
}

impl VirtualUser for QuickJsVu {
    fn run_iteration(&mut self) -> Result<IterationResult> {
        self.iteration += 1;
        let iteration = self.iteration;

        let start = Instant::now();

        self.ctx.with(|ctx| {
            // Update __ITER
            let globals = ctx.globals();
            globals.set("__ITER", iteration)?;

            // Call the exec function (default or named), passing setup data if available
            let fn_name = match &self.exec_fn {
                Some(name) => name.as_str(),
                None => "__k6_default",
            };
            let func: Function = globals
                .get(fn_name)
                .with_context(|| format!("exec function '{fn_name}' not found"))?;

            let data: rquickjs::Value = ctx
                .eval("typeof __k6_setup_data !== 'undefined' ? __k6_setup_data : undefined")
                .unwrap_or(rquickjs::Value::new_undefined(ctx.clone()));

            func.call::<_, rquickjs::Value>((data,))
                .catch(&ctx)
                .map_err(|e| anyhow::anyhow!("iteration error: {e:?}"))?;

            Ok::<_, anyhow::Error>(())
        })?;

        // Run any pending jobs
        runtime::drain_pending_jobs(&self.rt);

        let duration = start.elapsed();

        // Record iteration metrics
        if let Some(ref metrics) = self.metrics {
            metrics.record_iteration(duration.as_secs_f64() * 1000.0);
        }

        Ok(IterationResult { duration })
    }

    fn reset(&mut self) {
        // VU state persists across iterations (like k6) —
        // cookies, variables, etc. carry over.
        // Only per-iteration state (__ITER) is updated in run_iteration.
    }
}

/// Wraps a k6-style script so that `export default function` and
/// `export const options` are accessible as globals.
///
/// QuickJS module exports are not directly accessible from Rust,
/// so we rewrite the script to assign them to `globalThis`.
///
/// If `script_dir` is provided, local file imports (starting with `./ ` or `../`)
/// are resolved and inlined. k6 built-in imports (k6/*, k6) are commented out.
pub fn prepare_script(source: &str) -> String {
    prepare_script_with_dir(source, None)
}

/// Same as `prepare_script` but resolves local imports relative to `script_dir`.
pub fn prepare_script_with_dir(source: &str, script_dir: Option<&Path>) -> String {
    let mut output = String::new();
    let mut inlined_files: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ") {
            if let Some(dir) = script_dir {
                if let Some(path) = extract_import_path(trimmed) {
                    if path.starts_with("./") || path.starts_with("../") {
                        // Local file import — resolve and inline
                        let resolved = dir.join(path);
                        if !inlined_files.contains(&resolved) {
                            inlined_files.insert(resolved.clone());
                            match std::fs::read_to_string(&resolved) {
                                Ok(contents) => {
                                    let parent = resolved.parent().unwrap_or(dir);
                                    output.push_str(&format!("// inlined: {path}\n"));
                                    // Recursively prepare the imported file
                                    let prepared =
                                        prepare_script_with_dir(&contents, Some(parent));
                                    output.push_str(&prepared);
                                    output.push('\n');
                                    continue;
                                }
                                Err(e) => {
                                    output.push_str(&format!(
                                        "// ERROR: could not import {path}: {e}\n"
                                    ));
                                    continue;
                                }
                            }
                        } else {
                            output.push_str(&format!("// already inlined: {path}\n"));
                            continue;
                        }
                    }

                    if path.starts_with("k6/x/") {
                        let ext_name = &path[5..]; // strip "k6/x/"
                        let ext_file = dir.join("extensions").join(format!("{ext_name}.js"));
                        let ext_file_index =
                            dir.join("extensions").join(ext_name).join("index.js");
                        let resolved = if ext_file.exists() {
                            ext_file
                        } else {
                            ext_file_index
                        };
                        if !inlined_files.contains(&resolved) {
                            inlined_files.insert(resolved.clone());
                            match std::fs::read_to_string(&resolved) {
                                Ok(contents) => {
                                    let parent = resolved.parent().unwrap_or(dir);
                                    output.push_str(&format!("// extension: {path}\n"));
                                    let prepared =
                                        prepare_script_with_dir(&contents, Some(parent));
                                    output.push_str(&prepared);
                                    output.push('\n');
                                    continue;
                                }
                                Err(e) => {
                                    output.push_str(&format!(
                                        "// ERROR: extension {path} not found: {e}\n"
                                    ));
                                    continue;
                                }
                            }
                        } else {
                            output.push_str(&format!("// already inlined: {path}\n"));
                            continue;
                        }
                    }
                }
            }
            // k6 built-in import — comment out
            output.push_str("// ");
            output.push_str(line);
            output.push('\n');
        } else if trimmed.starts_with("export default function") {
            // export default function() { ... } → globalThis.__k6_default = function() { ... }
            let rest = trimmed.strip_prefix("export default ").unwrap();
            output.push_str("globalThis.__k6_default = ");
            output.push_str(rest);
            output.push('\n');
        } else if trimmed.starts_with("export const options") {
            // export const options = { ... } → globalThis.__k6_options = ...
            let rest = trimmed.strip_prefix("export const options").unwrap();
            output.push_str("globalThis.__k6_options");
            output.push_str(rest);
            output.push('\n');
        } else if trimmed.starts_with("export function") {
            // export function foo() { ... } → globalThis.foo = function foo() { ... }
            let rest = trimmed.strip_prefix("export ").unwrap();
            if let Some(name) = rest
                .strip_prefix("function ")
                .and_then(|s| s.split('(').next())
            {
                output.push_str(&format!("globalThis.{name} = "));
            }
            output.push_str(rest);
            output.push('\n');
        } else if trimmed.starts_with("export let ")
            || trimmed.starts_with("export var ")
            || trimmed.starts_with("export const ")
        {
            // export const/let/var foo = ... → globalThis.foo = ...
            let rest = trimmed
                .strip_prefix("export const ")
                .or_else(|| trimmed.strip_prefix("export let "))
                .or_else(|| trimmed.strip_prefix("export var "))
                .unwrap();
            // Extract the variable name
            if let Some(name) = rest.split(|c: char| !c.is_alphanumeric() && c != '_').next() {
                let after_name = &rest[name.len()..];
                output.push_str(&format!("globalThis.{name}"));
                output.push_str(after_name);
                output.push('\n');
            } else {
                output.push_str(line);
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

/// Extract the module path from an import statement.
/// e.g. `import { foo } from './utils.js'` → `./utils.js`
fn extract_import_path(import_line: &str) -> Option<&str> {
    // Look for 'path' or "path" after "from"
    let from_idx = import_line.find(" from ")?;
    let after_from = &import_line[from_idx + 6..];
    let trimmed = after_from.trim().trim_end_matches(';');

    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        Some(&trimmed[1..trimmed.len() - 1])
    } else {
        None
    }
}

/// Extract named imports from an import statement.
/// e.g. `import { foo, bar } from 'k6/x/mymodule'` → `["foo", "bar"]`
fn extract_import_names(import_line: &str) -> Option<Vec<&str>> {
    let open = import_line.find('{')?;
    let close = import_line.find('}')?;
    let inner = &import_line[open + 1..close];
    let names: Vec<&str> = inner
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn create_vu_and_run_iteration() {
        let script = r#"
            globalThis.__k6_default = function() {
                return 42;
            };
        "#;

        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();
        assert_eq!(vu.vu_id(), 1);
        assert_eq!(vu.iteration(), 0);

        let result = vu.run_iteration().unwrap();
        assert_eq!(vu.iteration(), 1);
        assert!(result.duration < Duration::from_millis(100));
    }

    #[test]
    fn vu_has_env_access() {
        let script = r#"
            globalThis.__k6_default = function() {
                if (__ENV.BASE_URL !== 'http://localhost:8080') {
                    throw new Error('wrong BASE_URL: ' + __ENV.BASE_URL);
                }
            };
        "#;

        let env = vec![
            ("BASE_URL".to_string(), "http://localhost:8080".to_string()),
        ];
        let mut vu = QuickJsVu::new(1, script, &env).unwrap();
        vu.run_iteration().unwrap();
    }

    #[test]
    fn vu_tracks_iteration_count() {
        let script = r#"
            globalThis.__k6_default = function() {};
        "#;

        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();

        for i in 1..=5 {
            vu.run_iteration().unwrap();
            assert_eq!(vu.iteration(), i);
        }
    }

    #[test]
    fn vu_state_persists_across_iterations() {
        let script = r#"
            let counter = 0;
            globalThis.__k6_default = function() {
                counter++;
                globalThis.__counter = counter;
            };
        "#;

        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();

        vu.run_iteration().unwrap();
        vu.run_iteration().unwrap();
        vu.run_iteration().unwrap();

        vu.ctx.with(|ctx| {
            let counter: i32 = ctx.globals().get("__counter").unwrap();
            assert_eq!(counter, 3);
        });
    }

    #[test]
    fn vu_error_on_missing_default() {
        let script = "let x = 1;";
        let result = QuickJsVu::new(1, script, &[]);
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("default function"), "error was: {err}");
    }

    #[test]
    fn vu_reports_js_errors() {
        let script = r#"
            globalThis.__k6_default = function() {
                throw new Error('intentional error');
            };
        "#;

        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();
        let result = vu.run_iteration();
        assert!(result.is_err());
    }

    #[test]
    fn prepare_script_transforms_exports() {
        let input = r#"
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
  vus: 10,
  duration: '30s',
};

export default function() {
  const res = http.get('http://test.k6.io');
}

export function setup() {
  console.log('setup');
}
"#;

        let output = prepare_script(input);

        assert!(output.contains("globalThis.__k6_default = function()"));
        assert!(output.contains("globalThis.__k6_options"));
        assert!(output.contains("globalThis.setup = function setup()"));
        assert!(output.contains("// import http from 'k6/http';"));
    }

    #[test]
    fn console_log_works() {
        let script = r#"
            globalThis.__k6_default = function() {
                console.log('hello from VU', __VU);
            };
        "#;

        let mut vu = QuickJsVu::new(42, script, &[]).unwrap();
        vu.run_iteration().unwrap();
    }

    #[test]
    fn vu_implements_virtual_user_trait() {
        let script = r#"
            globalThis.__k6_default = function() {};
        "#;

        let mut vu: Box<dyn VirtualUser> = Box::new(
            QuickJsVu::new(1, script, &[]).unwrap(),
        );

        vu.run_iteration().unwrap();
        vu.reset();
    }

    #[test]
    fn prepare_and_run_k6_script() {
        // Test the full pipeline: prepare_script → QuickJsVu::new → run
        let k6_script = r#"
export const options = {
  vus: 10,
  duration: '30s',
};

export default function() {
  // In a real script this would call http.get etc.
  globalThis.__test_ran = true;
}
"#;

        let prepared = prepare_script(k6_script);
        let mut vu = QuickJsVu::new(1, &prepared, &[]).unwrap();
        vu.run_iteration().unwrap();

        vu.ctx.with(|ctx| {
            let ran: bool = ctx.globals().get("__test_ran").unwrap();
            assert!(ran);
        });
    }

    #[test]
    fn setup_returns_data() {
        let script = prepare_script(r#"
export function setup() {
    return { token: 'abc123', users: [1, 2, 3] };
}

export default function(data) {
    if (data.token !== 'abc123') throw new Error('wrong token');
    globalThis.__data_ok = true;
}
"#);

        let mut vu = QuickJsVu::new(1, &script, &[]).unwrap();

        assert!(vu.has_setup());
        let data = vu.run_setup().unwrap();
        assert!(data.is_some());

        let json = data.unwrap();
        assert!(json.contains("abc123"));

        vu.run_iteration().unwrap();

        vu.ctx.with(|ctx| {
            let ok: bool = ctx.globals().get("__data_ok").unwrap();
            assert!(ok);
        });
    }

    #[test]
    fn setup_data_shared_across_vus() {
        let script = prepare_script(r#"
export function setup() {
    return { count: 42 };
}

export default function(data) {
    globalThis.__received_count = data.count;
}
"#);

        // VU 1 runs setup
        let mut vu1 = QuickJsVu::new(1, &script, &[]).unwrap();
        let data = vu1.run_setup().unwrap().unwrap();

        // VU 2 receives setup data without running setup
        let mut vu2 = QuickJsVu::new(2, &script, &[]).unwrap();
        vu2.set_setup_data(&data).unwrap();
        vu2.run_iteration().unwrap();

        vu2.ctx.with(|ctx| {
            let count: i32 = ctx.globals().get("__received_count").unwrap();
            assert_eq!(count, 42);
        });
    }

    #[test]
    fn teardown_receives_setup_data() {
        let script = prepare_script(r#"
export function setup() {
    return { msg: 'hello' };
}

export default function(data) {}

export function teardown(data) {
    globalThis.__teardown_msg = data.msg;
}
"#);

        let mut vu = QuickJsVu::new(1, &script, &[]).unwrap();

        assert!(vu.has_setup());
        assert!(vu.has_teardown());

        let _data = vu.run_setup().unwrap().unwrap();
        vu.run_iteration().unwrap();
        vu.run_teardown().unwrap();

        vu.ctx.with(|ctx| {
            let msg: String = ctx.globals().get("__teardown_msg").unwrap();
            assert_eq!(msg, "hello");
        });
    }

    #[test]
    fn no_setup_no_teardown() {
        let script = prepare_script(r#"
export default function() {
    globalThis.__ran = true;
}
"#);

        let mut vu = QuickJsVu::new(1, &script, &[]).unwrap();
        assert!(!vu.has_setup());
        assert!(!vu.has_teardown());

        vu.run_iteration().unwrap();

        vu.ctx.with(|ctx| {
            let ran: bool = ctx.globals().get("__ran").unwrap();
            assert!(ran);
        });
    }

    #[test]
    fn exec_named_function() {
        let script = prepare_script(r#"
export default function() {
    globalThis.__which = 'default';
}

export function myScenario() {
    globalThis.__which = 'myScenario';
}
"#);

        // Default exec
        let mut vu1 = QuickJsVu::new(1, &script, &[]).unwrap();
        vu1.run_iteration().unwrap();
        vu1.ctx.with(|ctx| {
            let which: String = ctx.globals().get("__which").unwrap();
            assert_eq!(which, "default");
        });

        // Named exec
        let mut vu2 = QuickJsVu::new(2, &script, &[]).unwrap();
        vu2.set_exec_fn("myScenario");
        vu2.run_iteration().unwrap();
        vu2.ctx.with(|ctx| {
            let which: String = ctx.globals().get("__which").unwrap();
            assert_eq!(which, "myScenario");
        });
    }

    #[test]
    fn handle_summary_callback() {
        let script = prepare_script(r#"
export default function() {}

export function handleSummary(data) {
    return {
        stdout: JSON.stringify(data.metrics) + '\n',
    };
}
"#);

        let mut vu = QuickJsVu::new(1, &script, &[]).unwrap();
        assert!(vu.has_handle_summary());

        let data_json = r#"{"metrics":{"http_reqs":{"type":"counter","contains":"default","values":{"count":100,"rate":10}}},"root_group":{"name":"","path":""},"state":{"is_std_out_tty":false,"test_run_duration_ms":10000}}"#;
        let outputs = vu.run_handle_summary(data_json).unwrap();

        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "stdout");
        assert!(outputs[0].1.contains("http_reqs"));
    }

    #[test]
    fn no_handle_summary() {
        let script = prepare_script(r#"
export default function() {}
"#);
        let vu = QuickJsVu::new(1, &script, &[]).unwrap();
        assert!(!vu.has_handle_summary());
    }

    #[test]
    fn fail_aborts_iteration() {
        let script = prepare_script(r#"
export default function() {
    fail('something went wrong');
    globalThis.__should_not_reach = true;
}
"#);
        let mut vu = QuickJsVu::new(1, &script, &[]).unwrap();
        let result = vu.run_iteration();
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("fail:"), "error was: {err}");
    }

    #[test]
    fn encoding_available_in_vu() {
        let script = r#"
            globalThis.__k6_default = function() {
                globalThis.__encoded = b64encode('test');
                globalThis.__decoded = b64decode(globalThis.__encoded);
            };
        "#;
        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();
        vu.run_iteration().unwrap();
        vu.ctx.with(|ctx| {
            let encoded: String = ctx.globals().get("__encoded").unwrap();
            assert_eq!(encoded, "dGVzdA==");
            let decoded: String = ctx.globals().get("__decoded").unwrap();
            assert_eq!(decoded, "test");
        });
    }

    #[test]
    fn crypto_available_in_vu() {
        let script = r#"
            globalThis.__k6_default = function() {
                globalThis.__hash = crypto.sha256('hello');
            };
        "#;
        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();
        vu.run_iteration().unwrap();
        vu.ctx.with(|ctx| {
            let hash: String = ctx.globals().get("__hash").unwrap();
            assert_eq!(hash, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        });
    }

    #[test]
    fn execution_available_in_vu() {
        let script = r#"
            globalThis.__k6_default = function() {
                globalThis.__vu_id = execution.vu.idInInstance;
            };
        "#;
        let mut vu = QuickJsVu::new(42, script, &[]).unwrap();
        vu.run_iteration().unwrap();
        vu.ctx.with(|ctx| {
            let id: i32 = ctx.globals().get("__vu_id").unwrap();
            assert_eq!(id, 42);
        });
    }

    #[test]
    fn html_available_in_vu() {
        let script = r#"
            globalThis.__k6_default = function() {
                var doc = parseHTML('<div><span class="test">hello</span></div>');
                globalThis.__text = doc.find('span').text();
            };
        "#;
        let mut vu = QuickJsVu::new(1, script, &[]).unwrap();
        vu.run_iteration().unwrap();
        vu.ctx.with(|ctx| {
            let text: String = ctx.globals().get("__text").unwrap();
            assert_eq!(text, "hello");
        });
    }

    #[test]
    fn prepare_script_with_local_imports() {
        // Create temp files for testing imports
        let dir = std::env::temp_dir().join("k6rs_test_imports");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(
            dir.join("helpers.js"),
            "export function greet(name) { return 'hello ' + name; }\n",
        )
        .unwrap();

        let script = r#"
import { greet } from './helpers.js';
import http from 'k6/http';

export default function() {
    globalThis.__greeting = greet('world');
}
"#;

        let output = prepare_script_with_dir(script, Some(&dir));

        // Local import should be inlined
        assert!(output.contains("// inlined: ./helpers.js"), "output: {output}");
        assert!(output.contains("globalThis.greet = function greet"));

        // k6 import should be commented out
        assert!(output.contains("// import http from 'k6/http'"));

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_script_exports_const_let_var() {
        let script = r#"
export const BASE_URL = 'http://localhost:8080';
export let counter = 0;
export var name = 'test';

export default function() {}
"#;

        let output = prepare_script(script);
        assert!(output.contains("globalThis.BASE_URL = 'http://localhost:8080'"), "output: {output}");
        assert!(output.contains("globalThis.counter = 0"), "output: {output}");
        assert!(output.contains("globalThis.name = 'test'"), "output: {output}");
    }

    #[test]
    fn random_seed_produces_deterministic_output() {
        let script = r#"
            randomSeed(42);
            globalThis.__k6_default = function() {
                globalThis.__r1 = Math.random();
                globalThis.__r2 = Math.random();
            };
        "#;

        let mut vu1 = QuickJsVu::new(1, script, &[]).unwrap();
        vu1.run_iteration().unwrap();

        let (r1a, r2a) = vu1.ctx.with(|ctx| {
            let r1: f64 = ctx.globals().get("__r1").unwrap();
            let r2: f64 = ctx.globals().get("__r2").unwrap();
            (r1, r2)
        });

        // Same seed should produce same sequence
        let mut vu2 = QuickJsVu::new(2, script, &[]).unwrap();
        vu2.run_iteration().unwrap();

        let (r1b, r2b) = vu2.ctx.with(|ctx| {
            let r1: f64 = ctx.globals().get("__r1").unwrap();
            let r2: f64 = ctx.globals().get("__r2").unwrap();
            (r1, r2)
        });

        assert_eq!(r1a, r1b);
        assert_eq!(r2a, r2b);
        // Should be different values
        assert_ne!(r1a, r2a);
    }

    #[test]
    fn extract_import_path_works() {
        assert_eq!(
            extract_import_path("import { foo } from './utils.js'"),
            Some("./utils.js")
        );
        assert_eq!(
            extract_import_path(r#"import http from "k6/http""#),
            Some("k6/http")
        );
        assert_eq!(
            extract_import_path("import { check } from 'k6';"),
            Some("k6")
        );
    }

    #[test]
    fn extract_import_names_basic() {
        let names = extract_import_names("import { foo, bar } from 'k6/x/mod'").unwrap();
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn extract_import_names_single() {
        let names = extract_import_names("import { greet } from 'k6/x/mymod'").unwrap();
        assert_eq!(names, vec!["greet"]);
    }

    #[test]
    fn extract_import_names_none() {
        assert!(extract_import_names("import http from 'k6/http'").is_none());
    }

    #[test]
    fn extension_import_inlines_file() {
        let dir = std::env::temp_dir().join("k6rs_test_ext");
        let ext_dir = dir.join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("mymod.js"),
            "export function greet(name) { return 'hello ' + name; }\n",
        )
        .unwrap();

        let script = r#"
import { greet } from 'k6/x/mymod';
export default function() { console.log(greet('world')); }
"#;
        let prepared = prepare_script_with_dir(script, Some(&dir));
        assert!(prepared.contains("// extension: k6/x/mymod"));
        assert!(prepared.contains("globalThis.greet = function greet"));
        assert!(!prepared.contains("import { greet }"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_import_index_js() {
        let dir = std::env::temp_dir().join("k6rs_test_ext_idx");
        let ext_dir = dir.join("extensions").join("complex");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("index.js"),
            "export const VERSION = '1.0';\nexport function run() { return true; }\n",
        )
        .unwrap();

        let script = r#"
import { VERSION, run } from 'k6/x/complex';
export default function() {}
"#;
        let prepared = prepare_script_with_dir(script, Some(&dir));
        assert!(prepared.contains("// extension: k6/x/complex"));
        assert!(prepared.contains("globalThis.VERSION"));
        assert!(prepared.contains("globalThis.run = function run"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_import_missing_file() {
        let dir = std::env::temp_dir().join("k6rs_test_ext_missing");
        std::fs::create_dir_all(&dir).unwrap();

        let script =
            "import { foo } from 'k6/x/nonexistent';\nexport default function() {}\n";
        let prepared = prepare_script_with_dir(script, Some(&dir));
        assert!(prepared.contains("// ERROR: extension k6/x/nonexistent not found"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_dedup_multiple_imports() {
        let dir = std::env::temp_dir().join("k6rs_test_ext_dedup");
        let ext_dir = dir.join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("utils.js"),
            "export function helper() { return 42; }\n",
        )
        .unwrap();

        let script = r#"
import { helper } from 'k6/x/utils';
import { helper } from 'k6/x/utils';
export default function() {}
"#;
        let prepared = prepare_script_with_dir(script, Some(&dir));
        // Should only inline once
        let count = prepared.matches("// extension: k6/x/utils").count();
        assert_eq!(count, 1);
        assert!(prepared.contains("// already inlined: k6/x/utils"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
