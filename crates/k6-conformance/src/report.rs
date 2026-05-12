//! CG-6 — three-class result reporting (PASS / FAIL / UNRELIABLE) with a
//! structured reliability block, plus optional JSON report output.
//!
//! Result semantics (per CG-6 design):
//!  - **PASS**: zero findings AND no drops on either side.
//!  - **FAIL**: at least one parity finding AND no drops on either side.
//!  - **UNRELIABLE**: drops on at least one side. UNRELIABLE dominates PASS
//!    but does NOT hide parity findings — if both drops AND diffs exist,
//!    the result is UNRELIABLE and the findings are still listed.
//!
//! The JSON report format is designed for downstream tooling to consume
//! without having to parse finding text. Top-level `overall_status` plus
//! per-script `status` + `reliability` block makes CI gates trivial:
//!   `jq '.overall_status == "PASS"' report.json`

use serde::Serialize;

use crate::canonical::{Reliability, SideReliability};
use crate::diff::{DiffFinding, FindingKind};

/// Three-class result. UNRELIABLE is a distinct state from PASS/FAIL
/// because drops are an evidence problem (the run can't be trusted),
/// not a parity problem. Collapsing it into FAIL would blur the two
/// causes and make CI/reporting harder to interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScriptResult {
    Pass,
    Fail,
    Unreliable,
}

impl ScriptResult {
    pub fn from_findings_and_reliability(
        findings: &[DiffFinding],
        reliability: Option<&Reliability>,
    ) -> Self {
        let unreliable = reliability.map(Reliability::is_unreliable).unwrap_or(false);
        // UNRELIABLE dominates: drops mean the evidence can't be trusted,
        // regardless of whether parity findings exist. Findings are still
        // listed in the report — they just don't promote the result above
        // UNRELIABLE.
        if unreliable {
            ScriptResult::Unreliable
        } else if findings.is_empty() {
            ScriptResult::Pass
        } else {
            ScriptResult::Fail
        }
    }

    fn label(self) -> &'static str {
        match self {
            ScriptResult::Pass => "PASS",
            ScriptResult::Fail => "FAIL",
            ScriptResult::Unreliable => "UNRELIABLE",
        }
    }
}

pub struct ScriptReport {
    pub name: String,
    pub findings: Vec<DiffFinding>,
    pub reliability: Option<Reliability>,
}

impl ScriptReport {
    pub fn result(&self) -> ScriptResult {
        ScriptResult::from_findings_and_reliability(
            &self.findings,
            self.reliability.as_ref(),
        )
    }
}

/// Print human-readable report to stdout. Returns true iff every script
/// passed (no FAIL, no UNRELIABLE). The runner exits non-zero when this
/// returns false.
pub fn print(reports: &[ScriptReport]) -> bool {
    let mut all_clean = true;
    for r in reports {
        let result = r.result();
        if result != ScriptResult::Pass {
            all_clean = false;
        }
        println!("{}  {}", result.label(), r.name);

        // UNRELIABLE: show the reliability block at the top so the reader
        // sees the cause immediately. Findings still follow because the
        // CG-6 design's "don't hide diffs" rule applies.
        if let Some(rel) = &r.reliability {
            if rel.is_unreliable() {
                print_reliability_block("  ", rel);
            }
        }

        for f in &r.findings {
            let tag = finding_tag(f.kind);
            println!("    [{tag}] {}  {}", f.selector, f.detail);
        }
    }
    all_clean
}

fn print_reliability_block(prefix: &str, rel: &Reliability) {
    let lines = format_reliability_block(rel);
    for line in lines {
        println!("{prefix}{line}");
    }
}

fn format_reliability_block(rel: &Reliability) -> Vec<String> {
    let mut out = Vec::new();
    out.push("[reliability] sink overflow detected — run cannot be trusted as parity evidence".into());
    if rel.upstream.drops_total > 0 {
        out.push(format!(
            "  upstream  drops={} peak={}/{} metrics_affected={:?}",
            rel.upstream.drops_total,
            rel.upstream.peak_occupancy,
            rel.upstream.capacity,
            rel.upstream.drops_per_metric.keys().collect::<Vec<_>>()
        ));
    }
    if rel.k6rs.drops_total > 0 {
        out.push(format!(
            "  k6rs      drops={} peak={}/{} metrics_affected={:?}",
            rel.k6rs.drops_total,
            rel.k6rs.peak_occupancy,
            rel.k6rs.capacity,
            rel.k6rs.drops_per_metric.keys().collect::<Vec<_>>()
        ));
    }
    out
}

fn finding_tag(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::ExitCode => "exit",
        FindingKind::MissingMetric => "missing",
        FindingKind::Drift => "drift",
        FindingKind::KindMismatch => "kind",
        FindingKind::MissingCheck => "check-missing",
        FindingKind::CheckCountMismatch => "check-counts",
        FindingKind::CheckIdMismatch => "check-id",
        FindingKind::MissingGroup => "group-missing",
        FindingKind::GroupIdMismatch => "group-id",
        FindingKind::MissingMetricDef => "stream-missing-def",
        FindingKind::MetricKindMismatch => "stream-kind",
        FindingKind::MetricContainsMismatch => "stream-contains",
        FindingKind::SampleCountMismatch => "stream-count",
        FindingKind::StreamFileError => "stream-file-error",
        FindingKind::SidecarUnreadable => "sidecar-unreadable",
    }
}

// --- JSON report (CG-6) ---

#[derive(Debug, Clone, Serialize)]
pub struct JsonReport {
    pub overall_status: ScriptResult,
    pub scripts: Vec<JsonScriptReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonScriptReport {
    pub name: String,
    pub status: ScriptResult,
    pub reliability: Option<JsonReliabilityBlock>,
    pub findings: Vec<JsonFindingEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonReliabilityBlock {
    pub upstream: JsonSideReliability,
    pub k6rs: JsonSideReliability,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonSideReliability {
    pub capacity: u64,
    pub peak_occupancy: u64,
    pub drops_total: u64,
    pub drops_per_metric: std::collections::BTreeMap<String, u64>,
}

impl From<&SideReliability> for JsonSideReliability {
    fn from(s: &SideReliability) -> Self {
        Self {
            capacity: s.capacity,
            peak_occupancy: s.peak_occupancy,
            drops_total: s.drops_total,
            drops_per_metric: s.drops_per_metric.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonFindingEntry {
    pub kind: String,
    pub selector: String,
    pub detail: String,
}

/// Build the structured JSON report. `overall_status` collapses
/// per-script statuses: UNRELIABLE dominates FAIL dominates PASS.
pub fn build_json_report(reports: &[ScriptReport]) -> JsonReport {
    let scripts: Vec<JsonScriptReport> = reports
        .iter()
        .map(|r| JsonScriptReport {
            name: r.name.clone(),
            status: r.result(),
            reliability: r.reliability.as_ref().map(|rel| JsonReliabilityBlock {
                upstream: (&rel.upstream).into(),
                k6rs: (&rel.k6rs).into(),
            }),
            findings: r
                .findings
                .iter()
                .map(|f| JsonFindingEntry {
                    kind: finding_tag(f.kind).to_string(),
                    selector: f.selector.clone(),
                    detail: f.detail.clone(),
                })
                .collect(),
        })
        .collect();

    let overall_status = scripts
        .iter()
        .map(|s| s.status)
        .fold(ScriptResult::Pass, dominate);

    JsonReport {
        overall_status,
        scripts,
    }
}

/// CG-6 status precedence: UNRELIABLE > FAIL > PASS. Note: the comparison
/// is by INFORMATION CONTENT, not severity. UNRELIABLE means "we don't
/// know" which is strictly worse than "we know it failed".
fn dominate(a: ScriptResult, b: ScriptResult) -> ScriptResult {
    use ScriptResult::*;
    match (a, b) {
        (Unreliable, _) | (_, Unreliable) => Unreliable,
        (Fail, _) | (_, Fail) => Fail,
        _ => Pass,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Reliability, SideReliability};
    use crate::diff::FindingKind;

    fn mk_finding() -> DiffFinding {
        DiffFinding {
            kind: FindingKind::Drift,
            selector: "http_req_duration".into(),
            detail: "avg drift 5%".into(),
        }
    }

    fn rel_with_drops(upstream: u64, k6rs: u64) -> Reliability {
        Reliability {
            upstream: SideReliability {
                capacity: 1024,
                peak_occupancy: 100,
                drops_total: upstream,
                drops_per_metric: Default::default(),
                error: None,
            },
            k6rs: SideReliability {
                capacity: 1024,
                peak_occupancy: 100,
                drops_total: k6rs,
                drops_per_metric: Default::default(),
                error: None,
            },
        }
    }

    /// PASS = no findings AND no drops. Baseline behavior — most scripts
    /// produce this.
    #[test]
    fn pass_when_no_findings_and_no_drops() {
        let r = ScriptResult::from_findings_and_reliability(
            &[],
            Some(&rel_with_drops(0, 0)),
        );
        assert_eq!(r, ScriptResult::Pass);
    }

    /// FAIL = findings present, no drops. Standard diff failure.
    #[test]
    fn fail_when_findings_present_and_no_drops() {
        let r = ScriptResult::from_findings_and_reliability(
            &[mk_finding()],
            Some(&rel_with_drops(0, 0)),
        );
        assert_eq!(r, ScriptResult::Fail);
    }

    /// UNRELIABLE dominates PASS. Drops on either side flag the run as
    /// untrustworthy regardless of zero findings — the missing samples
    /// could hide divergence that's invisible to the diff.
    #[test]
    fn unreliable_dominates_pass_when_drops_present() {
        // upstream-only drops
        let r = ScriptResult::from_findings_and_reliability(
            &[],
            Some(&rel_with_drops(5, 0)),
        );
        assert_eq!(r, ScriptResult::Unreliable);
        // k6rs-only drops
        let r = ScriptResult::from_findings_and_reliability(
            &[],
            Some(&rel_with_drops(0, 5)),
        );
        assert_eq!(r, ScriptResult::Unreliable);
    }

    /// UNRELIABLE dominates FAIL too — but findings are still preserved
    /// in the report. Locks the CG-6 design's "don't hide diffs" rule.
    #[test]
    fn unreliable_dominates_fail_but_findings_persist() {
        let findings = vec![mk_finding()];
        let report = ScriptReport {
            name: "07".into(),
            findings: findings.clone(),
            reliability: Some(rel_with_drops(3, 0)),
        };
        assert_eq!(report.result(), ScriptResult::Unreliable);
        // Findings remain present on the report (don't-hide-diffs invariant).
        assert_eq!(report.findings.len(), 1);
        let json = build_json_report(&[report]);
        assert_eq!(json.overall_status, ScriptResult::Unreliable);
        assert_eq!(json.scripts[0].findings.len(), 1);
    }

    /// Overall status precedence: any UNRELIABLE script makes the whole
    /// report UNRELIABLE; otherwise FAIL dominates PASS.
    #[test]
    fn overall_status_obeys_dominate_precedence() {
        let pass = ScriptReport {
            name: "01".into(),
            findings: vec![],
            reliability: Some(rel_with_drops(0, 0)),
        };
        let fail = ScriptReport {
            name: "02".into(),
            findings: vec![mk_finding()],
            reliability: Some(rel_with_drops(0, 0)),
        };
        let unrel = ScriptReport {
            name: "07".into(),
            findings: vec![],
            reliability: Some(rel_with_drops(5, 0)),
        };

        assert_eq!(
            build_json_report(&[pass]).overall_status,
            ScriptResult::Pass
        );
        let pass2 = ScriptReport {
            name: "01".into(),
            findings: vec![],
            reliability: None,
        };
        assert_eq!(
            build_json_report(&[pass2, fail]).overall_status,
            ScriptResult::Fail
        );
        let pass3 = ScriptReport {
            name: "01".into(),
            findings: vec![],
            reliability: None,
        };
        let fail2 = ScriptReport {
            name: "02".into(),
            findings: vec![mk_finding()],
            reliability: Some(rel_with_drops(0, 0)),
        };
        assert_eq!(
            build_json_report(&[pass3, fail2, unrel]).overall_status,
            ScriptResult::Unreliable
        );
    }

    /// JSON report shape: top-level overall_status and per-script
    /// reliability block populate so downstream tooling can read
    /// structured fields rather than parse finding text.
    #[test]
    fn json_report_carries_structured_reliability_block() {
        let r = ScriptReport {
            name: "07".into(),
            findings: vec![],
            reliability: Some(rel_with_drops(0, 7)),
        };
        let json = build_json_report(&[r]);
        assert_eq!(json.overall_status, ScriptResult::Unreliable);
        let block = json.scripts[0]
            .reliability
            .as_ref()
            .expect("reliability block present");
        assert_eq!(block.k6rs.drops_total, 7);
        assert_eq!(block.upstream.drops_total, 0);
    }

    /// CG-6 review fix: k6-rs sidecar absence (error field set on
    /// SideReliability) classifies the run UNRELIABLE even with zero
    /// drops. Before this fix, missing-sidecar = zero-drop default =
    /// silent PASS, which exactly masked the failure mode CG-6 was
    /// meant to surface. Lock-in test: regression-fail on the unfixed
    /// code (without the `error` field carrying through).
    #[test]
    fn unreliable_when_k6rs_sidecar_unreadable_even_without_drops() {
        let rel = Reliability {
            upstream: SideReliability::default(),
            k6rs: SideReliability {
                error: Some("reading sidecar /tmp/x.json.diagnostics.json: No such file or directory".into()),
                ..Default::default()
            },
        };
        let r = ScriptResult::from_findings_and_reliability(&[], Some(&rel));
        assert_eq!(
            r, ScriptResult::Unreliable,
            "k6-rs sidecar absence must flag UNRELIABLE — missing-sidecar = unknown reliability"
        );
    }

    /// Upstream sidecar absence is fine (its writer is unbounded, no
    /// sidecar by design). Don't mark the run UNRELIABLE for it.
    #[test]
    fn reliable_when_upstream_sidecar_absent_with_no_drops() {
        // Upstream side's `error` field is NEVER set — the tolerant
        // reader returns default. Construction here mirrors that path.
        let rel = Reliability {
            upstream: SideReliability::default(),
            k6rs: SideReliability::default(),
        };
        let r = ScriptResult::from_findings_and_reliability(&[], Some(&rel));
        assert_eq!(r, ScriptResult::Pass);
    }
}
