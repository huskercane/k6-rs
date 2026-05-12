//! Diff layer: compares two `CanonicalRun`s under per-script tolerances.
//!
//! HARD CONSTRAINT (spike acceptance criterion): nothing in this module
//! references "upstream" or "k6rs" or branches on which adapter produced a
//! CanonicalRun. Both sides are interchangeable inputs; the diff is symmetric.

pub mod counters;
pub mod trends;

use crate::canonical::{
    CanonicalEventStream, CanonicalMetricKind, CanonicalRun,
};
use crate::expectations::{Expectations, Tolerance};

#[derive(Debug, Clone)]
pub struct DiffFinding {
    pub kind: FindingKind,
    pub selector: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// Metric present on one side, missing on the other.
    MissingMetric,
    /// Numeric drift beyond tolerance.
    Drift,
    /// Exit code mismatch.
    ExitCode,
    /// Metric type mismatch (e.g. counter vs trend with same name).
    KindMismatch,
    /// Check present on one side, missing on the other (CG-1).
    MissingCheck,
    /// Check pass/fail counts differ (CG-1).
    CheckCountMismatch,
    /// Same canonical path on both sides but different serialized `id`. This
    /// indicates a hash/serialization bug, not a counts bug — under a
    /// correct implementation, `id = md5(path)` is identical on both sides
    /// by construction.
    CheckIdMismatch,
    /// Group present on one side, missing on the other (CG-2). The tree was
    /// asymmetric — one runner materialized a group node that the other
    /// didn't. CG-2's main signal: catches dropped `group()` calls or
    /// regressions in the group-only branch.
    MissingGroup,
    /// Same canonical group path on both sides but different serialized
    /// `id` (CG-2). Mirrors `CheckIdMismatch`: a hash/serialization-layer
    /// bug, not a tree-shape bug.
    GroupIdMismatch,
    /// CG-6 — Metric definition event present in only one side's event
    /// stream.
    MissingMetricDef,
    /// CG-6 — Metric definition kind differs (e.g. counter vs trend).
    MetricKindMismatch,
    /// CG-6 — Metric definition `contains` field differs (`"default"` vs
    /// `"time"`).
    MetricContainsMismatch,
    /// CG-6 — Per-metric sample count differs beyond the script's
    /// tolerance. Catches dropped emissions or extra spurious emissions.
    SampleCountMismatch,
    /// CG-6 follow-up — The per-side `--out json` event-stream file
    /// either didn't exist after the run OR was unparseable. Hard
    /// finding (FAIL): a missing/corrupt stream file means the sink
    /// pipeline broke and the diff has no evidence to work from.
    /// Distinct from UNRELIABLE (drops happened but evidence is intact)
    /// because here the evidence itself is gone.
    StreamFileError,
    /// CG-6 follow-up — The k6-rs `.diagnostics.json` sidecar was
    /// missing or unparseable. Always set on the k6-rs side; tolerated
    /// (no finding) on upstream because upstream's writer is unbounded
    /// and doesn't emit a sidecar. The presence of this finding marks
    /// the run UNRELIABLE — we have no reliability evidence for the
    /// k6-rs side, so its stream can't be trusted as parity evidence.
    SidecarUnreadable,
}

/// Compare two runs and produce findings. Caller is responsible for
/// interpreting findings against `Expectations::known_drift`.
pub fn diff(left: &CanonicalRun, right: &CanonicalRun, exp: &Expectations) -> Vec<DiffFinding> {
    let mut out = Vec::new();

    if left.exit_code != right.exit_code {
        out.push(DiffFinding {
            kind: FindingKind::ExitCode,
            selector: "<run>".into(),
            detail: format!("exit codes differ: {} vs {}", left.exit_code, right.exit_code),
        });
    }

    // Walk the union of selectors.
    let mut all: Vec<&String> = left.metrics.keys().chain(right.metrics.keys()).collect();
    all.sort();
    all.dedup();

    for sel in all {
        // known_drift applies to all finding kinds for this selector. Used for
        // acknowledged architectural asymmetries (e.g. upstream summary-export
        // omits gauges) — never to silence engine bugs.
        if exp.is_known_drift(sel) {
            continue;
        }
        match (left.metrics.get(sel), right.metrics.get(sel)) {
            (None, Some(_)) | (Some(_), None) => {
                out.push(DiffFinding {
                    kind: FindingKind::MissingMetric,
                    selector: sel.clone(),
                    detail: "present on only one side".into(),
                });
            }
            (Some(l), Some(r)) => {
                let tol = exp.tolerance_for(sel);
                match (&l.kind, &r.kind) {
                    (
                        CanonicalMetricKind::Counter { count: lc, rate: lr },
                        CanonicalMetricKind::Counter { count: rc, rate: rr },
                    ) => out.extend(counters::diff(sel, (*lc, *lr), (*rc, *rr), &tol)),
                    (CanonicalMetricKind::Trend(lt), CanonicalMetricKind::Trend(rt)) => {
                        out.extend(trends::diff(sel, lt, rt, &tol))
                    }
                    // Gauge/Rate not in spike scope.
                    (CanonicalMetricKind::Gauge { .. }, CanonicalMetricKind::Gauge { .. }) => {}
                    (CanonicalMetricKind::Rate { .. }, CanonicalMetricKind::Rate { .. }) => {}
                    _ => out.push(DiffFinding {
                        kind: FindingKind::KindMismatch,
                        selector: sel.clone(),
                        detail: "metric kind differs between runs".into(),
                    }),
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // CG-1: per-check identity diff. Pass/fail counts are exact-match; any
    // identity present on only one side is a MissingCheck finding. Same
    // `known_drift` filter as metrics — the path string is the selector.
    let mut check_keys: Vec<&String> =
        left.checks.keys().chain(right.checks.keys()).collect();
    check_keys.sort();
    check_keys.dedup();
    for path in check_keys {
        if exp.is_known_drift(path) {
            continue;
        }
        match (left.checks.get(path), right.checks.get(path)) {
            (None, Some(_)) | (Some(_), None) => {
                out.push(DiffFinding {
                    kind: FindingKind::MissingCheck,
                    selector: path.clone(),
                    detail: "check present on only one side".into(),
                });
            }
            (Some(l), Some(r)) => {
                if l.passes != r.passes || l.fails != r.fails {
                    out.push(DiffFinding {
                        kind: FindingKind::CheckCountMismatch,
                        selector: path.clone(),
                        detail: format!(
                            "passes/fails differ: {}/{} vs {}/{}",
                            l.passes, l.fails, r.passes, r.fails
                        ),
                    });
                }
                // Skip id comparison when either side reports an empty id
                // (legacy fixtures, or a runner that hasn't wired it yet).
                // Two real ids that disagree on the same path is the signal
                // — it means the hash function or path encoding diverged.
                if !l.id.is_empty() && !r.id.is_empty() && l.id != r.id {
                    out.push(DiffFinding {
                        kind: FindingKind::CheckIdMismatch,
                        selector: path.clone(),
                        detail: format!("id differs: {} vs {}", l.id, r.id),
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // CG-6: per-sample event stream diff (Metric defs + per-metric sample
    // counts). Only runs when BOTH sides produced a stream — older scripts
    // without sink-stream coverage carry None and skip this. Tag-bucket
    // counts are intentionally NOT compared in the first cut: k6-rs is
    // missing several upstream-default tags (name/url/proto/group/
    // scenario), so per-tag-bucket diff would fail every script until
    // those tag-emission gaps close (tracked separately).
    if let (Some(l_stream), Some(r_stream)) =
        (&left.event_stream, &right.event_stream)
    {
        out.extend(diff_event_streams(l_stream, r_stream, exp));
    }

    // CG-2: per-group identity diff. Pass-through on the same `known_drift`
    // filter — paths can be acknowledged as architectural asymmetries.
    let mut group_keys: Vec<&String> =
        left.groups.keys().chain(right.groups.keys()).collect();
    group_keys.sort();
    group_keys.dedup();
    for path in group_keys {
        if exp.is_known_drift(path) {
            continue;
        }
        match (left.groups.get(path), right.groups.get(path)) {
            (None, Some(_)) | (Some(_), None) => {
                out.push(DiffFinding {
                    kind: FindingKind::MissingGroup,
                    selector: path.clone(),
                    detail: "group present on only one side".into(),
                });
            }
            (Some(l), Some(r)) => {
                if !l.id.is_empty() && !r.id.is_empty() && l.id != r.id {
                    out.push(DiffFinding {
                        kind: FindingKind::GroupIdMismatch,
                        selector: path.clone(),
                        detail: format!("id differs: {} vs {}", l.id, r.id),
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }

    out
}

/// CG-6 — event-stream diff. Compares Metric definitions and per-metric
/// sample counts symmetrically. Per the spike acceptance criterion: no
/// adapter-specific branches. The `left` and `right` arguments are
/// interchangeable; identical inputs always produce identical findings.
pub(crate) fn diff_event_streams(
    left: &CanonicalEventStream,
    right: &CanonicalEventStream,
    exp: &Expectations,
) -> Vec<DiffFinding> {
    let mut out = Vec::new();

    // 1. Metric definition union: missing + kind + contains mismatches.
    let mut def_names: Vec<&String> = left
        .metric_defs
        .keys()
        .chain(right.metric_defs.keys())
        .collect();
    def_names.sort();
    def_names.dedup();
    for name in def_names {
        if exp.is_known_drift(name) {
            continue;
        }
        match (
            left.metric_defs.get(name),
            right.metric_defs.get(name),
        ) {
            (None, Some(_)) | (Some(_), None) => out.push(DiffFinding {
                kind: FindingKind::MissingMetricDef,
                selector: name.clone(),
                detail: "Metric event present on only one side".into(),
            }),
            (Some(l), Some(r)) => {
                if l.kind != r.kind {
                    out.push(DiffFinding {
                        kind: FindingKind::MetricKindMismatch,
                        selector: name.clone(),
                        detail: format!(
                            "kind differs: {} vs {}",
                            l.kind.as_str(),
                            r.kind.as_str()
                        ),
                    });
                }
                if l.contains != r.contains {
                    out.push(DiffFinding {
                        kind: FindingKind::MetricContainsMismatch,
                        selector: name.clone(),
                        detail: format!(
                            "contains differs: {:?} vs {:?}",
                            l.contains, r.contains
                        ),
                    });
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // 2. Per-metric sample count diff. Uses the metric's tolerance (from
    // expectations.toml) — `Exact` for counters, `Relative(eps)` for
    // metrics with naturally-variable sample counts. A metric present in
    // only one side counts the other as 0 and either matches (if drift
    // tolerated) or is flagged.
    let mut count_names: Vec<&String> = left
        .sample_counts
        .keys()
        .chain(right.sample_counts.keys())
        .collect();
    count_names.sort();
    count_names.dedup();
    for name in count_names {
        if exp.is_known_drift(name) {
            continue;
        }
        let l = *left.sample_counts.get(name).unwrap_or(&0);
        let r = *right.sample_counts.get(name).unwrap_or(&0);
        // Sample counts are integer-valued — use the `count` tolerance
        // from the script's expectations (matches the counter-metric
        // tolerance dimension; trends/rates would use different fields).
        let profile = exp.tolerance_for(name);
        if let Some(detail) =
            check_tolerance("sample_count", l as f64, r as f64, &profile.count)
        {
            out.push(DiffFinding {
                kind: FindingKind::SampleCountMismatch,
                selector: name.clone(),
                detail,
            });
        }
    }

    out
}

/// Helper: relative drift, safe for zero baselines.
pub(crate) fn relative_drift(a: f64, b: f64) -> f64 {
    let mag = a.abs().max(b.abs());
    if mag < f64::EPSILON {
        0.0
    } else {
        (a - b).abs() / mag
    }
}

/// Check a numeric against its tolerance, return Some(detail) if breached.
pub(crate) fn check_tolerance(
    field: &str,
    a: f64,
    b: f64,
    tol: &Tolerance,
) -> Option<String> {
    match tol {
        Tolerance::Exact => {
            if (a - b).abs() > f64::EPSILON {
                Some(format!("{field}: {a} vs {b} (exact required)"))
            } else {
                None
            }
        }
        Tolerance::Relative(eps) => {
            let d = relative_drift(a, b);
            if d > *eps {
                Some(format!(
                    "{field}: {a} vs {b} (Δ={:.3}%, allowed {:.1}%)",
                    d * 100.0,
                    eps * 100.0
                ))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{CanonicalCheck, CanonicalRun};
    use crate::expectations::Expectations;
    use std::collections::BTreeMap;

    fn run_with_check(c: CanonicalCheck) -> CanonicalRun {
        let mut checks = BTreeMap::new();
        checks.insert(format!("{}::{}", c.group_path, c.name), c);
        CanonicalRun {
            checks,
            ..CanonicalRun::default()
        }
    }

    fn empty_exp() -> Expectations {
        // Minimal Expectations with no known_drift / overrides. The diff
        // tests below only exercise the check-diff path; tolerance fields
        // never come into play because checks use exact-match comparison.
        // Build via the TOML loader using a tempfile so we don't have to
        // export private fields just for tests.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[run]\nexpected_exit_code = 0").unwrap();
        Expectations::load(tmp.path()).unwrap()
    }

    #[test]
    fn check_count_mismatch_is_flagged() {
        // CG-1 regression: same canonical path on both sides but different
        // passes/fails must produce a CheckCountMismatch finding so the
        // harness fails closed when an engine bug changes per-check counts.
        let left = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "id".into(),
            passes: 10,
            fails: 0,
        });
        let right = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "id".into(),
            passes: 9,
            fails: 1,
        });
        let findings = diff(&left, &right, &empty_exp());
        let f = findings
            .iter()
            .find(|f| f.kind == FindingKind::CheckCountMismatch)
            .expect("CheckCountMismatch finding present");
        assert_eq!(f.selector, "::ok");
        assert!(f.detail.contains("10/0"));
        assert!(f.detail.contains("9/1"));
        assert!(
            !findings
                .iter()
                .any(|f| f.kind == FindingKind::CheckIdMismatch),
            "id was equal — no CheckIdMismatch expected"
        );
    }

    #[test]
    fn check_id_mismatch_is_flagged_independently_of_counts() {
        // CG-1 follow-up: same path + same counts on both sides, but the
        // serialized id differs. This indicates a hash function or path
        // encoding bug at the serialization layer — counts cannot catch it.
        // Without this comparison the harness would have silently accepted
        // upstream's md5 alongside a different hash on the k6-rs side.
        let left = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "aaaa".into(),
            passes: 10,
            fails: 0,
        });
        let right = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "bbbb".into(),
            passes: 10,
            fails: 0,
        });
        let findings = diff(&left, &right, &empty_exp());
        let f = findings
            .iter()
            .find(|f| f.kind == FindingKind::CheckIdMismatch)
            .expect("CheckIdMismatch finding present");
        assert_eq!(f.selector, "::ok");
        assert!(f.detail.contains("aaaa"));
        assert!(f.detail.contains("bbbb"));
        // Counts agreed → no count finding.
        assert!(!findings
            .iter()
            .any(|f| f.kind == FindingKind::CheckCountMismatch));
    }

    #[test]
    fn check_id_mismatch_tolerated_when_either_side_blank() {
        // Tolerate fixtures / legacy adapters that don't serialize id.
        // The signal is two *real* ids that disagree; a missing id is
        // not actionable.
        let left = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "".into(),
            passes: 1,
            fails: 0,
        });
        let right = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "real".into(),
            passes: 1,
            fails: 0,
        });
        let findings = diff(&left, &right, &empty_exp());
        assert!(!findings
            .iter()
            .any(|f| f.kind == FindingKind::CheckIdMismatch));
    }

    #[test]
    fn missing_group_is_flagged() {
        // CG-2 regression: a `group()` call that exists on one side and not
        // the other must surface as MissingGroup. The signal catches dropped
        // group registration or a regression in the group-only branch
        // (group with no inner check).
        use crate::canonical::CanonicalGroup;
        let mut groups = BTreeMap::new();
        groups.insert(
            "::audit".to_string(),
            CanonicalGroup {
                name: "audit".to_string(),
                path: "::audit".to_string(),
                id: "id".to_string(),
            },
        );
        let left = CanonicalRun {
            groups,
            ..CanonicalRun::default()
        };
        let right = CanonicalRun::default();
        let findings = diff(&left, &right, &empty_exp());
        let f = findings
            .iter()
            .find(|f| f.kind == FindingKind::MissingGroup)
            .expect("MissingGroup finding present");
        assert_eq!(f.selector, "::audit");
    }

    #[test]
    fn group_id_mismatch_is_flagged_independently_of_existence() {
        // CG-2 regression: same group path on both sides, different `id`
        // (md5 of path). Symmetric with CheckIdMismatch — a serialization
        // bug, not a tree-shape bug.
        use crate::canonical::CanonicalGroup;
        let mk = |id: &str| {
            let mut g = BTreeMap::new();
            g.insert(
                "::api".to_string(),
                CanonicalGroup {
                    name: "api".to_string(),
                    path: "::api".to_string(),
                    id: id.to_string(),
                },
            );
            CanonicalRun {
                groups: g,
                ..CanonicalRun::default()
            }
        };
        let findings = diff(&mk("aaaa"), &mk("bbbb"), &empty_exp());
        let f = findings
            .iter()
            .find(|f| f.kind == FindingKind::GroupIdMismatch)
            .expect("GroupIdMismatch finding present");
        assert_eq!(f.selector, "::api");
        assert!(f.detail.contains("aaaa"));
        assert!(f.detail.contains("bbbb"));
    }

    #[test]
    fn missing_check_is_flagged() {
        let left = run_with_check(CanonicalCheck {
            name: "ok".into(),
            group_path: "".into(),
            id: "id".into(),
            passes: 1,
            fails: 0,
        });
        let right = CanonicalRun::default();
        let findings = diff(&left, &right, &empty_exp());
        let f = findings
            .iter()
            .find(|f| f.kind == FindingKind::MissingCheck)
            .expect("MissingCheck finding present");
        assert_eq!(f.selector, "::ok");
    }
}
