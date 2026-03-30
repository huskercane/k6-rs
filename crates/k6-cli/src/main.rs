mod analysis;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use k6_core::backpressure::Backpressure;
use k6_core::config::{self, ExecutorType, TestConfig};
use k6_core::executor::constant_arrival_rate::ConstantArrivalRateExecutor;
use k6_core::executor::constant_vus::ConstantVusExecutor;
use k6_core::executor::externally_controlled::ExternallyControlledExecutor;
use k6_core::executor::per_vu_iterations::PerVuIterationsExecutor;
use k6_core::executor::ramping_arrival_rate::RampingArrivalRateExecutor;
use k6_core::executor::ramping_vus::RampingVusExecutor;
use k6_core::executor::shared_iterations::SharedIterationsExecutor;
use k6_core::metrics::BuiltinMetrics;
use k6_core::vu_pool::VuPool;
use k6_js::http_client::ReqwestHttpClient;
use k6_js::vu::{self, QuickJsVu};

#[derive(Parser)]
#[command(name = "k6-rs", about = "High-performance load testing tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a k6 test script
    Run {
        /// Path to the test script
        script: String,

        /// Number of virtual users (overrides script options)
        #[arg(long)]
        vus: Option<u32>,

        /// Test duration (e.g., "30s", "5m") (overrides script options)
        #[arg(long)]
        duration: Option<String>,

        /// Output plugin(s): json=file.json, csv=file.csv, influxdb=url, prometheus=url, duckdb=file.duckdb
        #[arg(long = "out", short = 'o')]
        outputs: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            script,
            vus,
            duration,
            outputs,
        } => run_test(&script, vus, duration, &outputs).await,
    }
}

async fn run_test(
    script_path: &str,
    vus_override: Option<u32>,
    duration_override: Option<String>,
    output_specs: &[String],
) -> Result<()> {
    // Read and prepare the script, resolving local imports relative to script directory
    let raw_script =
        std::fs::read_to_string(script_path).with_context(|| format!("reading {script_path}"))?;
    let script_dir = std::path::Path::new(script_path)
        .parent()
        .map(|p| p.to_path_buf());
    let script = vu::prepare_script_with_dir(&raw_script, script_dir.as_deref());

    // Extract options from the script by evaluating it in a temporary context
    let options = extract_options(&script)?;

    // Parse config, apply CLI overrides
    let mut test_config = config::parse_options(&options)?;
    if let Some(v) = vus_override {
        test_config.vus = v;
    }
    if let Some(d) = duration_override {
        test_config.duration = config::parse_duration(&d)?;
    }

    // Rebuild default scenario if overrides were applied and only default exists
    if test_config.scenarios.len() == 1 && test_config.scenarios.contains_key("default") {
        test_config.scenarios.insert(
            "default".to_string(),
            k6_core::config::ScenarioConfig {
                executor: ExecutorType::ConstantVus {
                    vus: test_config.vus,
                    duration: test_config.duration,
                },
                exec: None,
                start_time: std::time::Duration::ZERO,
                graceful_stop: std::time::Duration::from_secs(30),
                env: std::collections::HashMap::new(),
                tags: std::collections::HashMap::new(),
            },
        );
    }

    // Run static analysis
    let max_vus: u32 = test_config
        .scenarios
        .values()
        .map(|s| scenario_max_vus(&s.executor))
        .sum();

    let lint_warnings = analysis::script_lint::lint_script(
        &script,
        max_vus,
        test_config.discard_response_bodies,
    );
    if !lint_warnings.is_empty() {
        eprint!("{}", analysis::script_lint::format_warnings(&lint_warnings));
    }

    // Initialize output plugins
    let mut output_plugins: Vec<Box<dyn k6_core::output::Output>> = Vec::new();
    for spec in output_specs {
        let (name, arg) = k6_core::output::parse_out_flag(spec);
        let mut plugin = k6_core::output::create_output(name, arg)?;
        plugin.start()?;
        output_plugins.push(plugin);
    }

    // Print banner
    print_banner(&test_config, script_path, &output_plugins);

    // Create HTTP client and shared metrics
    let client = Arc::new(ReqwestHttpClient::from_config(&test_config)?);
    let handle = tokio::runtime::Handle::current();
    let cancel = CancellationToken::new();
    let metrics = BuiltinMetrics::new();

    // Set up global rate limiter if rps is configured
    let rate_limiter = k6_core::backpressure::RateLimiter::new(test_config.rps);
    let _rate_limiter_handle = rate_limiter.start_replenish(cancel.clone());

    // Set up console output file if configured
    let console_output: Option<Arc<std::sync::Mutex<std::fs::File>>> =
        if let Some(ref path) = test_config.console_output {
            let file = std::fs::File::create(path)
                .with_context(|| format!("creating console output file: {path}"))?;
            Some(Arc::new(std::sync::Mutex::new(file)))
        } else {
            None
        };

    // Set up Ctrl+C handler — first press stops gracefully, second force-kills
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nInterrupted — stopping gracefully (press Ctrl+C again to force)...");
        cancel_clone.cancel();

        // Second Ctrl+C: force exit
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nForce stopping...");
        std::process::exit(130);
    });

    let test_start = std::time::Instant::now();

    // Run setup() if the script defines it
    let setup_data = {
        let bp = Backpressure::new(1);
        let mut setup_vu = QuickJsVu::new_full_with_console(
            0,
            &script,
            &[],
            handle.clone(),
            Arc::clone(&client),
            bp,
            Some(metrics.clone()),
            script_dir.clone(),
            console_output.clone(),
        )?;

        if setup_vu.has_setup() {
            eprintln!("  running setup()...");
            setup_vu.run_setup()?
        } else {
            None
        }
    };

    // Helper to create VUs with metrics, setup data, and optional exec function
    let create_vus =
        |num: u32, bp: &Backpressure, exec: &Option<String>| -> Result<Vec<QuickJsVu>> {
            (0..num)
                .map(|i| {
                    let mut vu = QuickJsVu::new_full_with_console(
                        i,
                        &script,
                        &[],
                        handle.clone(),
                        Arc::clone(&client),
                        bp.clone(),
                        Some(metrics.clone()),
                        script_dir.clone(),
                        console_output.clone(),
                    )?;
                    if let Some(ref data) = setup_data {
                        vu.set_setup_data(data)?;
                    }
                    if let Some(fn_name) = exec {
                        vu.set_exec_fn(fn_name);
                    }
                    Ok(vu)
                })
                .collect()
        };

    // Run each scenario
    for (name, scenario) in &test_config.scenarios {
        // Delay start if startTime is set
        if !scenario.start_time.is_zero() {
            eprintln!("  scenario {name}: waiting {:?} (startTime)", scenario.start_time);
            tokio::time::sleep(scenario.start_time).await;
        }

        eprintln!("  running scenario: {name}");
        let num_vus = scenario_max_vus(&scenario.executor);
        metrics.set_vus(num_vus);
        metrics.set_vus_max(num_vus);

        let summary = match &scenario.executor {
            ExecutorType::ConstantVus { vus, duration } => {
                let bp = Backpressure::from_vus(*vus as usize);
                let vus = create_vus(*vus, &bp, &scenario.exec)?;
                let executor = ConstantVusExecutor::new(vus, *duration);
                executor.run(cancel.clone()).await?
            }
            ExecutorType::ConstantArrivalRate {
                rate,
                time_unit,
                duration,
                pre_allocated_vus,
                max_vus,
            } => {
                let nv = max_vus.unwrap_or(*pre_allocated_vus);
                let bp = Backpressure::from_vus(nv as usize);
                let vus = create_vus(nv, &bp, &scenario.exec)?;
                let pool = Arc::new(VuPool::new(vus));
                let executor =
                    ConstantArrivalRateExecutor::new(pool, *rate, *time_unit, *duration);
                executor.run(cancel.clone()).await?
            }
            ExecutorType::RampingVus {
                start_vus,
                stages,
                ..
            } => {
                let nv = stages
                    .iter()
                    .map(|s| s.target)
                    .max()
                    .unwrap_or(*start_vus)
                    .max(*start_vus);
                let bp = Backpressure::from_vus(nv as usize);
                let vus = create_vus(nv, &bp, &scenario.exec)?;
                let pool = Arc::new(VuPool::new(vus));
                let executor = RampingVusExecutor::new(pool, stages.clone(), *start_vus);
                executor.run(cancel.clone()).await?
            }
            ExecutorType::RampingArrivalRate {
                start_rate,
                stages,
                time_unit,
                pre_allocated_vus,
                max_vus,
            } => {
                let nv = max_vus.unwrap_or(*pre_allocated_vus);
                let bp = Backpressure::from_vus(nv as usize);
                let vus = create_vus(nv, &bp, &scenario.exec)?;
                let pool = Arc::new(VuPool::new(vus));
                let executor = RampingArrivalRateExecutor::new(
                    pool,
                    stages.clone(),
                    *start_rate as f64,
                    *time_unit,
                );
                executor.run(cancel.clone()).await?
            }
            ExecutorType::PerVuIterations {
                vus,
                iterations,
                max_duration,
            } => {
                let bp = Backpressure::from_vus(*vus as usize);
                let vus = create_vus(*vus, &bp, &scenario.exec)?;
                let executor = PerVuIterationsExecutor::new(vus, *iterations, *max_duration);
                executor.run(cancel.clone()).await?
            }
            ExecutorType::SharedIterations {
                vus,
                iterations,
                max_duration,
            } => {
                let bp = Backpressure::from_vus(*vus as usize);
                let vus = create_vus(*vus, &bp, &scenario.exec)?;
                let executor = SharedIterationsExecutor::new(vus, *iterations, *max_duration);
                executor.run(cancel.clone()).await?
            }
            ExecutorType::ExternallyControlled {
                vus,
                max_vus,
                duration,
            } => {
                let nv = *max_vus;
                let bp = Backpressure::from_vus(nv as usize);
                let vus_vec = create_vus(nv, &bp, &scenario.exec)?;
                let pool = Arc::new(VuPool::new(vus_vec));
                let executor = ExternallyControlledExecutor::new(pool, *vus, nv, *duration);
                executor.run(cancel.clone()).await?
            }
        };

        eprintln!("  scenario {name}: {} iterations in {:?}", summary.iterations_completed, summary.duration);
        if summary.iterations_dropped > 0 {
            eprintln!("    dropped: {} (VU pool exhausted)", summary.iterations_dropped);
        }

        // Push snapshot to output plugins after each scenario
        if !output_plugins.is_empty() {
            let elapsed = test_start.elapsed().as_secs_f64();
            let snap = metrics.registry.snapshot(elapsed);
            for plugin in &mut output_plugins {
                if let Err(e) = plugin.add_snapshot(&snap, elapsed) {
                    eprintln!("  warning: output plugin error: {e}");
                }
            }
        }
    }

    // Run teardown() if the script defines it
    {
        let bp = Backpressure::new(1);
        let mut teardown_vu = QuickJsVu::new_full_with_console(
            0,
            &script,
            &[],
            handle.clone(),
            Arc::clone(&client),
            bp,
            Some(metrics.clone()),
            script_dir.clone(),
            console_output.clone(),
        )?;

        if teardown_vu.has_teardown() {
            eprintln!("  running teardown()...");
            if let Some(ref data) = setup_data {
                teardown_vu.set_setup_data(data)?;
            }
            teardown_vu.run_teardown()?;
        }
    }

    // End-of-test summary
    let total_duration = test_start.elapsed();
    let snapshot = metrics.registry.snapshot(total_duration.as_secs_f64());

    // Evaluate thresholds
    let threshold_results = if !test_config.thresholds.is_empty() {
        Some(k6_core::thresholds::evaluate(
            &test_config.thresholds,
            &snapshot,
        ))
    } else {
        None
    };

    // Try handleSummary() if defined, else use default summary
    let mut used_handle_summary = false;
    {
        let bp = Backpressure::new(1);
        let mut summary_vu = QuickJsVu::new_full_with_console(
            0,
            &script,
            &[],
            handle.clone(),
            Arc::clone(&client),
            bp,
            Some(metrics.clone()),
            script_dir.clone(),
            console_output.clone(),
        )?;

        if summary_vu.has_handle_summary() {
            let summary_data = k6_core::summary::build_summary_data(&snapshot, total_duration);
            let data_json = serde_json::to_string(&summary_data)?;
            match summary_vu.run_handle_summary(&data_json) {
                Ok(outputs) => {
                    used_handle_summary = true;
                    for (dest, content) in outputs {
                        match dest.as_str() {
                            "stdout" => print!("{content}"),
                            "stderr" => eprint!("{content}"),
                            path => {
                                if let Err(e) = std::fs::write(path, &content) {
                                    eprintln!("  warning: failed to write {path}: {e}");
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  warning: handleSummary() failed: {e}");
                }
            }
        }
    }

    if !used_handle_summary {
        eprintln!();
        eprint!(
            "{}",
            k6_core::summary::format_summary(
                &snapshot,
                total_duration,
                threshold_results.as_ref()
            )
        );
    }

    // Stop output plugins
    for plugin in &mut output_plugins {
        if let Err(e) = plugin.stop() {
            eprintln!("  warning: output plugin stop error: {e}");
        }
    }

    // Exit with code 99 if thresholds failed (same as k6)
    if let Some(ref results) = threshold_results {
        if !results.all_passed() {
            eprintln!();
            eprintln!("  some thresholds have failed");
            std::process::exit(99);
        }
    }

    Ok(())
}

fn extract_options(script: &str) -> Result<serde_json::Value> {
    let rt = k6_js::runtime::create_runtime()?;
    let ctx = k6_js::runtime::create_context(&rt)?;

    let options = ctx.with(|ctx| -> serde_json::Value {
        // Evaluate just enough to get options — skip HTTP calls
        ctx.eval::<(), _>(script).ok();

        let json_str: Option<String> = ctx
            .eval("typeof __k6_options === 'object' ? JSON.stringify(__k6_options) : null")
            .ok();

        json_str
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({}))
    });

    Ok(options)
}

fn print_banner(config: &TestConfig, script_path: &str, outputs: &[Box<dyn k6_core::output::Output>]) {
    eprintln!();
    eprintln!("         /\\      k6-rs     /‾‾/  ");
    eprintln!("    /\\  /  \\     |\\  __   /  /   ");
    eprintln!("   /  \\/    \\    | |/ /  /   ‾‾\\ ");
    eprintln!("  /          \\   |   (  |  (‾)  |");
    eprintln!(" / __________ \\  |_|\\_\\  \\_____/ ");
    eprintln!();
    eprintln!("     execution: local");
    eprintln!("        script: {script_path}");
    if outputs.is_empty() {
        eprintln!("        output: -");
    } else {
        let descs: Vec<String> = outputs.iter().map(|o| o.description()).collect();
        eprintln!("        output: {}", descs.join(", "));
    }

    let scenario_count = config.scenarios.len();
    let max_vus: u32 = config
        .scenarios
        .values()
        .map(|s| scenario_max_vus(&s.executor))
        .sum();

    // Estimate memory: ~4MB per QuickJS context + overhead
    let estimated_mb = (max_vus as f64 * 4.0) + 50.0; // 4MB/VU + 50MB base
    let memory_str = if estimated_mb >= 1024.0 {
        format!("{:.1} GB", estimated_mb / 1024.0)
    } else {
        format!("{:.0} MB", estimated_mb)
    };

    eprintln!(
        "     scenarios: {scenario_count} scenario(s), {max_vus} max VUs"
    );
    eprintln!("        memory: ~{memory_str} (max {max_vus} VUs)");

    for (name, scenario) in &config.scenarios {
        let desc = format_executor_desc(&scenario.executor);
        eprintln!("              * {name}: {desc}");
    }
    eprintln!();
}

fn scenario_max_vus(executor: &ExecutorType) -> u32 {
    match executor {
        ExecutorType::ConstantVus { vus, .. } => *vus,
        ExecutorType::RampingVus { start_vus, stages, .. } => {
            stages.iter().map(|s| s.target).max().unwrap_or(*start_vus).max(*start_vus)
        }
        ExecutorType::ConstantArrivalRate { max_vus, pre_allocated_vus, .. } => {
            max_vus.unwrap_or(*pre_allocated_vus)
        }
        ExecutorType::RampingArrivalRate { max_vus, pre_allocated_vus, .. } => {
            max_vus.unwrap_or(*pre_allocated_vus)
        }
        ExecutorType::PerVuIterations { vus, .. } => *vus,
        ExecutorType::SharedIterations { vus, .. } => *vus,
        ExecutorType::ExternallyControlled { max_vus, .. } => *max_vus,
    }
}

fn format_executor_desc(executor: &ExecutorType) -> String {
    match executor {
        ExecutorType::ConstantVus { vus, duration } => {
            format!("{vus} looping VUs for {duration:?}")
        }
        ExecutorType::RampingVus { stages, start_vus, .. } => {
            let max_target = stages.iter().map(|s| s.target).max().unwrap_or(0);
            let total_dur: std::time::Duration = stages.iter().map(|s| s.duration).sum();
            format!("ramping {start_vus}→{max_target} VUs over {total_dur:?}")
        }
        ExecutorType::ConstantArrivalRate { rate, duration, pre_allocated_vus, .. } => {
            format!("{rate} iters/s for {duration:?} ({pre_allocated_vus} pre-allocated VUs)")
        }
        ExecutorType::RampingArrivalRate { start_rate, stages, pre_allocated_vus, .. } => {
            let max_target = stages.iter().map(|s| s.target).max().unwrap_or(0);
            let total_dur: std::time::Duration = stages.iter().map(|s| s.duration).sum();
            format!("ramping {start_rate}→{max_target} iters/s over {total_dur:?} ({pre_allocated_vus} pre-allocated VUs)")
        }
        ExecutorType::PerVuIterations { vus, iterations, .. } => {
            format!("{vus} VUs × {iterations} iterations each")
        }
        ExecutorType::SharedIterations { vus, iterations, .. } => {
            format!("{iterations} iterations shared across {vus} VUs")
        }
        ExecutorType::ExternallyControlled { vus, max_vus, duration } => {
            if duration.is_zero() {
                format!("externally controlled, {vus} initial VUs (max {max_vus}), API on :6565")
            } else {
                format!("externally controlled, {vus} initial VUs (max {max_vus}), {duration:?}, API on :6565")
            }
        }
    }
}

