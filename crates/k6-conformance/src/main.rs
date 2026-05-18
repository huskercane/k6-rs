use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "k6-conformance",
    about = "k6 / k6-rs behavioral parity harness"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run conformance scripts and diff outputs.
    Run {
        /// Optional substring filter on script name.
        #[arg(long)]
        filter: Option<String>,

        /// Path to upstream k6 binary. Defaults to env K6_BIN or `k6` on PATH.
        #[arg(long)]
        upstream_bin: Option<String>,

        /// Path to k6-rs binary. Defaults to env K6RS_BIN or workspace target.
        #[arg(long)]
        k6rs_bin: Option<String>,

        /// Directory containing scripts/*/script.js + expectations.toml.
        #[arg(long, default_value = "crates/k6-conformance/scripts")]
        scripts_dir: String,

        /// CG-6 — write a structured JSON report to this path. Includes
        /// top-level `overall_status` and per-script `status` +
        /// `reliability` block so downstream tooling (CI, dashboards)
        /// doesn't have to parse finding text.
        #[arg(long)]
        report_json: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            filter,
            upstream_bin,
            k6rs_bin,
            scripts_dir,
            report_json,
        } => {
            let cfg = k6_conformance::runner::Config {
                upstream_bin: upstream_bin
                    .or_else(|| std::env::var("K6_BIN").ok())
                    .unwrap_or_else(|| "k6".into()),
                k6rs_bin: k6rs_bin
                    .or_else(|| std::env::var("K6RS_BIN").ok())
                    .unwrap_or_else(|| "target/debug/k6-rs".into()),
                scripts_dir: scripts_dir.into(),
                filter,
                report_json: report_json.map(std::path::PathBuf::from),
            };
            k6_conformance::runner::run(cfg).await
        }
    }
}
