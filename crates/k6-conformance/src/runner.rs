//! Orchestrates: per binary, start a fresh fixture → run → stop → adapt.
//! Then diff and report. The two binaries NEVER share a live fixture instance.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::adapters::{
    Adapter, RunArtifacts, json_stream, k6rs::K6rsAdapter, upstream::UpstreamAdapter,
};
use crate::canonical::{CanonicalEventStream, CanonicalRun, Reliability, SideReliability};
use crate::diff::{DiffFinding, FindingKind, diff};
use crate::expectations::Expectations;
use crate::fixtures::http::HttpFixture;
use crate::report::ScriptReport;

pub struct Config {
    pub upstream_bin: String,
    pub k6rs_bin: String,
    pub k6rs_http_client: String,
    pub scripts_dir: PathBuf,
    pub filter: Option<String>,
    /// CG-6 — when set, write a structured JSON report to this path.
    /// Downstream tooling reads top-level `overall_status` plus per-script
    /// `status` and `reliability` block without parsing finding text.
    pub report_json: Option<PathBuf>,
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

    if let Some(path) = &cfg.report_json {
        let json = crate::report::build_json_report(&reports);
        let f = std::fs::File::create(path)
            .with_context(|| format!("creating report-json file {}", path.display()))?;
        serde_json::to_writer_pretty(f, &json).context("serializing JSON report")?;
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
    let upstream_artifacts = invoke_against_fresh_fixture(
        &cfg.upstream_bin,
        &script.script_path,
        &exp,
        workdir.path(),
        "upstream",
        None,
    )
    .await?;
    let k6rs_artifacts = invoke_against_fresh_fixture(
        &cfg.k6rs_bin,
        &script.script_path,
        &exp,
        workdir.path(),
        "k6rs",
        Some(cfg.k6rs_http_client.as_str()),
    )
    .await?;

    let mut left: CanonicalRun = UpstreamAdapter.adapt(&upstream_artifacts)?;
    let mut right: CanonicalRun = K6rsAdapter.adapt(&k6rs_artifacts)?;

    let SinkArtifacts {
        upstream_stream,
        k6rs_stream,
        reliability,
        pre_diff_findings,
    } = load_sink_artifacts(
        &upstream_artifacts.out_json_path,
        &k6rs_artifacts.out_json_path,
    );

    left.event_stream = upstream_stream;
    right.event_stream = k6rs_stream;
    let reliability = Some(reliability);

    let mut findings: Vec<DiffFinding> = diff(&left, &right, &exp);
    // Pre-diff findings (stream/sidecar load failures) come first in the
    // report so the reader sees the root cause before the cascade of
    // missing-def / sample-count findings that follow from a broken
    // stream.
    findings.splice(0..0, pre_diff_findings);

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
        reliability,
    })
}

async fn invoke_against_fresh_fixture(
    bin: &str,
    script: &Path,
    exp: &Expectations,
    workdir: &Path,
    tag: &str,
    k6rs_http_client: Option<&str>,
) -> Result<RunArtifacts> {
    let fixture = HttpFixture::start().await?;
    let url = fixture.base_url.clone();
    // Run the (blocking) subprocess on a blocking task so we don't stall the runtime.
    let bin = bin.to_string();
    let script = script.to_path_buf();
    let workdir = workdir.to_path_buf();
    let tag = tag.to_string();
    let k6rs_http_client = k6rs_http_client.map(str::to_string);
    let exp = exp.clone();
    let artifacts = tokio::task::spawn_blocking(move || {
        invoke_binary(
            &bin,
            &script,
            &url,
            &workdir,
            &tag,
            &exp,
            k6rs_http_client.as_deref(),
        )
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
    k6rs_http_client: Option<&str>,
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

    // Select which k6-rs HTTP backend this conformance pass exercises.
    // Upstream k6 never receives this env var.
    if tag == "k6rs" {
        if let Some(client) = k6rs_http_client {
            cmd.env("K6RS_HTTP_CLIENT", client);
        }
    }

    let status = cmd.status().with_context(|| format!("spawning {bin}"))?;

    Ok(RunArtifacts {
        out_json_path: out_json,
        summary_export_path: summary_export,
        exit_code: status.code().unwrap_or(-1),
    })
}

/// CG-6 review fix — load per-side sink artifacts (stream + sidecar) into
/// a canonical bundle with explicit failure findings instead of silent
/// `None`/zero-drop defaults. Each failure mode produces a finding that
/// the diff layer can't filter out via known_drift (selectors are
/// `<upstream-stream>` / `<k6rs-stream>` / `<k6rs-sidecar>`, which scripts
/// wouldn't legitimately need to ignore).
///
/// Extracted from `run_one` so the wiring is unit-testable without
/// running real subprocesses.
struct SinkArtifacts {
    upstream_stream: Option<CanonicalEventStream>,
    k6rs_stream: Option<CanonicalEventStream>,
    reliability: Reliability,
    pre_diff_findings: Vec<DiffFinding>,
}

fn load_sink_artifacts(upstream_stream_path: &Path, k6rs_stream_path: &Path) -> SinkArtifacts {
    let mut pre_diff_findings: Vec<DiffFinding> = Vec::new();

    // Stream file reads — Err is a hard finding (StreamFileError). The
    // diff layer would otherwise silently skip the entire sink-stream
    // check when either side's stream is None. A missing/corrupt stream
    // file means the sink pipeline broke, which is the exact failure
    // mode CG-6's conformance was meant to surface.
    let upstream_stream = match json_stream::read_event_stream(upstream_stream_path) {
        Ok(s) => Some(s),
        Err(e) => {
            pre_diff_findings.push(DiffFinding {
                kind: FindingKind::StreamFileError,
                selector: "<upstream-stream>".into(),
                detail: e.to_string(),
            });
            None
        }
    };
    let k6rs_stream = match json_stream::read_event_stream(k6rs_stream_path) {
        Ok(s) => Some(s),
        Err(e) => {
            pre_diff_findings.push(DiffFinding {
                kind: FindingKind::StreamFileError,
                selector: "<k6rs-stream>".into(),
                detail: e.to_string(),
            });
            None
        }
    };

    // Sidecars — asymmetric handling per side:
    //   upstream: tolerant. Upstream's writer is unbounded
    //     (internal/output/json/json.go) and never emits a sidecar by
    //     design. Absence is the expected state and means zero drops.
    //   k6-rs: required. The k6-rs writer task always emits a sidecar.
    //     Absence means the binary crashed before flush OR a code bug
    //     in the writer OR an FS error preventing write. ALL three
    //     mean we have no reliability evidence for the k6-rs side, so
    //     the run must be classified UNRELIABLE (SideReliability.error
    //     is what surfaces the failure to is_unreliable()). Silently
    //     defaulting to zero drops would mask the exact failure mode
    //     CG-6 was meant to surface.
    let upstream_reliability = json_stream::read_sidecar_tolerant(upstream_stream_path);
    let k6rs_reliability = match json_stream::read_sidecar_required(k6rs_stream_path) {
        Ok(r) => r,
        Err(e) => {
            pre_diff_findings.push(DiffFinding {
                kind: FindingKind::SidecarUnreadable,
                selector: "<k6rs-sidecar>".into(),
                detail: e.clone(),
            });
            SideReliability {
                error: Some(e),
                ..Default::default()
            }
        }
    };

    SinkArtifacts {
        upstream_stream,
        k6rs_stream,
        reliability: Reliability {
            upstream: upstream_reliability,
            k6rs: k6rs_reliability,
        },
        pre_diff_findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("k6rs_conformance_runner_tests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn write_sidecar(stream_path: &Path, contents: &str) {
        let mut p = stream_path.as_os_str().to_owned();
        p.push(".diagnostics.json");
        std::fs::write(&p, contents).unwrap();
    }

    fn well_formed_stream() -> &'static str {
        concat!(
            r#"{"type":"Metric","data":{"name":"http_reqs","type":"counter","contains":"default"},"metric":"http_reqs"}"#,
            "\n",
            r#"{"metric":"http_reqs","type":"Point","data":{"time":"2026-05-12T16:00:00Z","value":1,"tags":{}}}"#,
            "\n",
        )
    }

    fn well_formed_sidecar() -> &'static str {
        r#"{"capacity":1024,"peak_occupancy":0,"dropped_samples":{"total":0,"per_metric":{}}}"#
    }

    /// Happy path: both sides have well-formed stream files + k6-rs has
    /// a valid sidecar. No pre-diff findings; both streams populate.
    #[test]
    fn load_sink_artifacts_happy_path_no_findings() {
        let upstream = write_temp("happy_upstream.json", well_formed_stream());
        let k6rs = write_temp("happy_k6rs.json", well_formed_stream());
        write_sidecar(&k6rs, well_formed_sidecar());

        let result = load_sink_artifacts(&upstream, &k6rs);
        assert!(result.pre_diff_findings.is_empty());
        assert!(result.upstream_stream.is_some());
        assert!(result.k6rs_stream.is_some());
        assert!(!result.reliability.is_unreliable());
        assert!(result.reliability.k6rs.error.is_none());
    }

    /// CG-6 review fix: a missing k6-rs stream file MUST produce a
    /// StreamFileError finding. Before the fix, `read_event_stream(...).ok()`
    /// turned this into `None` and the diff layer silently skipped the
    /// entire stream check.
    #[test]
    fn load_sink_artifacts_missing_k6rs_stream_produces_finding() {
        let upstream = write_temp("missing_k6rs_upstream.json", well_formed_stream());
        let k6rs_path = std::env::temp_dir()
            .join("k6rs_conformance_runner_tests")
            .join("does_not_exist.json");
        // No sidecar either — but we expect the stream error first.
        let result = load_sink_artifacts(&upstream, &k6rs_path);
        let stream_err = result
            .pre_diff_findings
            .iter()
            .find(|f| f.kind == FindingKind::StreamFileError && f.selector == "<k6rs-stream>")
            .expect("k6-rs stream file error finding must surface");
        assert!(
            stream_err.detail.contains("No such file") || stream_err.detail.contains("reading"),
            "finding detail must explain the cause; got {:?}",
            stream_err.detail
        );
        assert!(result.k6rs_stream.is_none());
    }

    /// Symmetric: upstream-side stream file missing also surfaces.
    /// The diff direction doesn't change the failure handling.
    #[test]
    fn load_sink_artifacts_missing_upstream_stream_produces_finding() {
        let upstream_path = std::env::temp_dir()
            .join("k6rs_conformance_runner_tests")
            .join("does_not_exist_upstream.json");
        let k6rs = write_temp("missing_upstream_k6rs.json", well_formed_stream());
        write_sidecar(&k6rs, well_formed_sidecar());

        let result = load_sink_artifacts(&upstream_path, &k6rs);
        assert!(
            result.pre_diff_findings.iter().any(
                |f| f.kind == FindingKind::StreamFileError && f.selector == "<upstream-stream>"
            )
        );
        assert!(result.upstream_stream.is_none());
    }

    /// CG-6 review fix: a missing k6-rs sidecar MUST produce a
    /// SidecarUnreadable finding AND set SideReliability.error AND
    /// classify the run UNRELIABLE. Before the fix, missing sidecar
    /// returned the zero-drop default — silently masking exactly the
    /// reliability failure CG-6 was meant to surface.
    #[test]
    fn load_sink_artifacts_missing_k6rs_sidecar_flags_unreliable() {
        let upstream = write_temp("sidecar_test_upstream.json", well_formed_stream());
        let k6rs = write_temp("sidecar_test_k6rs.json", well_formed_stream());
        // Deliberately DO NOT write a sidecar next to k6rs.

        let result = load_sink_artifacts(&upstream, &k6rs);
        let finding = result
            .pre_diff_findings
            .iter()
            .find(|f| f.kind == FindingKind::SidecarUnreadable)
            .expect("SidecarUnreadable finding must surface");
        assert_eq!(finding.selector, "<k6rs-sidecar>");
        assert!(result.reliability.k6rs.error.is_some());
        assert!(
            result.reliability.is_unreliable(),
            "missing k6-rs sidecar must classify run UNRELIABLE — got reliability={:?}",
            result.reliability
        );
    }

    /// Asymmetric: upstream sidecar absence is EXPECTED (its writer is
    /// unbounded — no sidecar by design). Must NOT produce a finding and
    /// must NOT flag UNRELIABLE.
    #[test]
    fn load_sink_artifacts_missing_upstream_sidecar_is_tolerated() {
        let upstream = write_temp("upstream_sidecar_absent.json", well_formed_stream());
        // No sidecar on upstream — by design.
        let k6rs = write_temp("upstream_sidecar_absent_k6rs.json", well_formed_stream());
        write_sidecar(&k6rs, well_formed_sidecar());

        let result = load_sink_artifacts(&upstream, &k6rs);
        // No SidecarUnreadable finding (upstream tolerated).
        assert!(
            !result
                .pre_diff_findings
                .iter()
                .any(|f| f.kind == FindingKind::SidecarUnreadable)
        );
        assert!(!result.reliability.is_unreliable());
    }

    /// CG-6 review fix: a corrupt/unparseable k6-rs sidecar must also
    /// flag UNRELIABLE — same evidence-missing semantic as absent.
    /// Without this, a writer task that emits partial JSON before
    /// crashing would silently appear reliable.
    #[test]
    fn load_sink_artifacts_unparseable_k6rs_sidecar_flags_unreliable() {
        let upstream = write_temp("corrupt_sidecar_upstream.json", well_formed_stream());
        let k6rs = write_temp("corrupt_sidecar_k6rs.json", well_formed_stream());
        write_sidecar(&k6rs, "NOT JSON {{{");

        let result = load_sink_artifacts(&upstream, &k6rs);
        assert!(
            result
                .pre_diff_findings
                .iter()
                .any(|f| f.kind == FindingKind::SidecarUnreadable)
        );
        assert!(result.reliability.is_unreliable());
    }
}
