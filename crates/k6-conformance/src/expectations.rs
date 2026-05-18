//! Per-script `expectations.toml` parsing.
//!
//! Schema (spike):
//! ```toml
//! [run]
//! expected_exit_code = 0
//! # Optional workload overrides. If set, forwarded as -u/-i/-d to BOTH binaries,
//! # superseding the script's inline `options`. Used to pin a workload independent
//! # of script intent — e.g. to isolate engine bugs from option-parsing bugs.
//! vus = 1
//! iterations = 10
//! duration = "10s"
//!
//! [tolerances]
//! default_counter = "exact"      # or "relative:0.02"
//! default_trend   = "relative:0.05"
//!
//! [tolerances.overrides]
//! "http_reqs"             = { count = "exact", rate = "relative:0.05" }
//! "http_req_duration"     = { trend = "relative:0.05" }
//!
//! [known_drift]
//! # selector = "reason"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum Tolerance {
    Exact,
    Relative(f64),
}

impl Tolerance {
    fn parse(s: &str) -> Result<Self> {
        if s == "exact" {
            return Ok(Self::Exact);
        }
        if let Some(rest) = s.strip_prefix("relative:") {
            let v: f64 = rest
                .parse()
                .context("relative tolerance must be a number")?;
            return Ok(Self::Relative(v));
        }
        anyhow::bail!("invalid tolerance: {s}")
    }
}

#[derive(Debug, Clone)]
pub struct ToleranceProfile {
    pub count: Tolerance,
    pub rate: Tolerance,
    pub trend: Tolerance,
}

#[derive(Debug, Clone)]
pub struct Expectations {
    pub expected_exit_code: i32,
    /// Override `options.vus` from the script. Forwarded as `-u N` to both binaries.
    pub vus: Option<u32>,
    /// Override `options.iterations`. Forwarded as `-i N` to both binaries.
    pub iterations: Option<u64>,
    /// Override duration. Forwarded as `-d D` to both binaries.
    pub duration: Option<String>,
    default_counter: Tolerance,
    default_trend: Tolerance,
    overrides: BTreeMap<String, RawOverride>,
    known_drift: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawOverride {
    count: Option<String>,
    rate: Option<String>,
    trend: Option<String>,
}

impl Expectations {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let parsed: RawExpectations = toml::from_str(&raw).context("parsing expectations.toml")?;

        let default_counter = Tolerance::parse(
            parsed
                .tolerances
                .default_counter
                .as_deref()
                .unwrap_or("exact"),
        )?;
        let default_trend = Tolerance::parse(
            parsed
                .tolerances
                .default_trend
                .as_deref()
                .unwrap_or("relative:0.05"),
        )?;

        Ok(Self {
            expected_exit_code: parsed.run.expected_exit_code.unwrap_or(0),
            vus: parsed.run.vus,
            iterations: parsed.run.iterations,
            duration: parsed.run.duration,
            default_counter,
            default_trend,
            overrides: parsed.tolerances.overrides.unwrap_or_default(),
            known_drift: parsed.known_drift.unwrap_or_default(),
        })
    }

    pub fn tolerance_for(&self, selector: &str) -> ToleranceProfile {
        let o = self.overrides.get(selector);
        let count = o
            .and_then(|o| o.count.as_deref())
            .and_then(|s| Tolerance::parse(s).ok())
            .unwrap_or_else(|| self.default_counter.clone());
        let rate = o
            .and_then(|o| o.rate.as_deref())
            .and_then(|s| Tolerance::parse(s).ok())
            .unwrap_or_else(|| self.default_trend.clone());
        let trend = o
            .and_then(|o| o.trend.as_deref())
            .and_then(|s| Tolerance::parse(s).ok())
            .unwrap_or_else(|| self.default_trend.clone());
        ToleranceProfile { count, rate, trend }
    }

    pub fn is_known_drift(&self, selector: &str) -> bool {
        self.known_drift.contains_key(selector)
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawExpectations {
    #[serde(default)]
    run: RawRun,
    #[serde(default)]
    tolerances: RawTolerances,
    #[serde(default)]
    known_drift: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawRun {
    expected_exit_code: Option<i32>,
    vus: Option<u32>,
    iterations: Option<u64>,
    duration: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTolerances {
    default_counter: Option<String>,
    default_trend: Option<String>,
    overrides: Option<BTreeMap<String, RawOverride>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_run_overrides() {
        let toml_text = r#"
            [run]
            expected_exit_code = 0
            vus = 5
            iterations = 50
            duration = "30s"

            [tolerances]
            default_counter = "exact"
        "#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();
        let exp = Expectations::load(tmp.path()).unwrap();
        assert_eq!(exp.vus, Some(5));
        assert_eq!(exp.iterations, Some(50));
        assert_eq!(exp.duration.as_deref(), Some("30s"));
    }

    #[test]
    fn parses_basic_expectations() {
        let toml_text = r#"
            [run]
            expected_exit_code = 0

            [tolerances]
            default_counter = "exact"
            default_trend = "relative:0.05"

            [tolerances.overrides]
            "http_reqs" = { count = "exact", rate = "relative:0.10" }

            [known_drift]
            "summary.tag_breakdown" = "schema not yet expanded"
        "#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_text).unwrap();
        let exp = Expectations::load(tmp.path()).unwrap();
        assert_eq!(exp.expected_exit_code, 0);
        assert_eq!(exp.vus, None);
        assert_eq!(exp.iterations, None);
        assert!(exp.is_known_drift("summary.tag_breakdown"));
        let tol = exp.tolerance_for("http_reqs");
        assert!(matches!(tol.count, Tolerance::Exact));
        assert!(matches!(tol.rate, Tolerance::Relative(v) if (v - 0.10).abs() < 1e-9));
    }
}
