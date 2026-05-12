//! Orchestrates: per binary, start a fresh fixture → run → stop → adapt.
//! Then diff and report. The two binaries NEVER share a live fixture instance.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::adapters::{k6rs::K6rsAdapter, upstream::UpstreamAdapter, Adapter, RunArtifacts};
use crate::canonical::CanonicalRun;
use crate::diff::{diff, DiffFinding};
use crate::expectations::Expectations;
use crate::fixtures::http::HttpFixture;
use crate::report::ScriptReport;

pub struct Config {
    pub upstream_bin: String,
    pub k6rs_bin: String,
    pub scripts_dir: PathBuf,
    pub filter: Option<String>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let scripts = discover_scripts(&cfg.scripts_dir, cfg.filter.as_deref())?;
    if scripts.is_empty() {
        anyhow::bail!("no scripts matched in {}", cfg.scripts_dir.display());
    }

    let mut reports = Vec::new();
    for script in scripts {
        let report = run_one(&cfg, &script).await?;
        reports.push(report);
    }

    let clean = crate::report::print(&reports);
    if !clean {
        std::process::exit(1);
    }
    Ok(())
}

struct ScriptDef {
    name: String,
    script_path: PathBuf,
    expectations_path: PathBuf,
}

fn discover_scripts(root: &Path, filter: Option<&str>) -> Result<Vec<ScriptDef>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("reading scripts dir {}", root.display()))?;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(f) = filter {
            if !name.contains(f) {
                continue;
            }
        }
        let script_path = entry.path().join("script.js");
        let expectations_path = entry.path().join("expectations.toml");
        if !script_path.exists() || !expectations_path.exists() {
            continue;
        }
        out.push(ScriptDef {
            name,
            script_path,
            expectations_path,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn run_one(cfg: &Config, script: &ScriptDef) -> Result<ScriptReport> {
    let exp = Expectations::load(&script.expectations_path)?;
    let workdir = tempfile::tempdir()?;

    // Fresh fixture per binary. Stateful fixtures (counters, connection state)
    // must not leak from one runner into the other.
    let upstream_artifacts =
        invoke_against_fresh_fixture(&cfg.upstream_bin, &script.script_path, &exp, workdir.path(), "upstream").await?;
    let k6rs_artifacts =
        invoke_against_fresh_fixture(&cfg.k6rs_bin, &script.script_path, &exp, workdir.path(), "k6rs").await?;

    let left: CanonicalRun = UpstreamAdapter.adapt(&upstream_artifacts)?;
    let right: CanonicalRun = K6rsAdapter.adapt(&k6rs_artifacts)?;

    let mut findings: Vec<DiffFinding> = diff(&left, &right, &exp);

    if left.exit_code != exp.expected_exit_code {
        findings.push(DiffFinding {
            kind: crate::diff::FindingKind::ExitCode,
            selector: "<upstream>".into(),
            detail: format!(
                "upstream exit {} != expected {}",
                left.exit_code, exp.expected_exit_code
            ),
        });
    }
    if right.exit_code != exp.expected_exit_code {
        findings.push(DiffFinding {
            kind: crate::diff::FindingKind::ExitCode,
            selector: "<k6rs>".into(),
            detail: format!(
                "k6-rs exit {} != expected {}",
                right.exit_code, exp.expected_exit_code
            ),
        });
    }

    Ok(ScriptReport {
        name: script.name.clone(),
        findings,
    })
}

async fn invoke_against_fresh_fixture(
    bin: &str,
    script: &Path,
    exp: &Expectations,
    workdir: &Path,
    tag: &str,
) -> Result<RunArtifacts> {
    let fixture = HttpFixture::start().await?;
    let url = fixture.base_url.clone();
    // Run the (blocking) subprocess on a blocking task so we don't stall the runtime.
    let bin = bin.to_string();
    let script = script.to_path_buf();
    let workdir = workdir.to_path_buf();
    let tag = tag.to_string();
    let exp = exp.clone();
    let artifacts = tokio::task::spawn_blocking(move || {
        invoke_binary(&bin, &script, &url, &workdir, &tag, &exp)
    })
    .await
    .context("subprocess task panicked")??;
    fixture.stop().await;
    Ok(artifacts)
}

fn invoke_binary(
    bin: &str,
    script: &Path,
    base_url: &str,
    workdir: &Path,
    tag: &str,
    exp: &Expectations,
) -> Result<RunArtifacts> {
    let out_json = workdir.join(format!("{tag}.out.json"));
    let summary_export = workdir.join(format!("{tag}.summary.json"));

    let mut cmd = Command::new(bin);
    cmd.arg("run")
        .arg("--out")
        .arg(format!("json={}", out_json.display()))
        .arg("--summary-export")
        .arg(&summary_export);

    // Forward [run] overrides. Both upstream k6 and k6-rs accept -u/-i/-d with
    // matching semantics; CLI flags supersede the script's `options` block.
    if let Some(v) = exp.vus {
        cmd.arg("-u").arg(v.to_string());
    }
    if let Some(i) = exp.iterations {
        cmd.arg("-i").arg(i.to_string());
    }
    if let Some(d) = exp.duration.as_deref() {
        cmd.arg("-d").arg(d);
    }

    // Pass K6_TEST_URL via --env: k6-rs's __ENV does not inherit process env
    // (env.rs only reads .env files + --env flags). Upstream k6 does inherit,
    // but --env works on both and is unambiguous.
    cmd.arg("--env")
        .arg(format!("K6_TEST_URL={base_url}"))
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // Tell k6-rs to use the (b)-spike hyper-level client. Upstream k6
    // ignores unrecognised env vars, so this is safe for both binaries.
    // The hyper client gives us DNS/TCP/sending phase timings and exact
    // wire-byte counts that the reqwest path can't see.
    if tag == "k6rs" {
        cmd.env("K6RS_HTTP_CLIENT", "hyper");
    }

    let status = cmd.status().with_context(|| format!("spawning {bin}"))?;

    Ok(RunArtifacts {
        out_json_path: out_json,
        summary_export_path: summary_export,
        exit_code: status.code().unwrap_or(-1),
    })
}
