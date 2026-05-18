use super::{DiffFinding, FindingKind, check_tolerance};
use crate::canonical::CanonicalTrend;
use crate::expectations::ToleranceProfile;

pub fn diff(
    selector: &str,
    a: &CanonicalTrend,
    b: &CanonicalTrend,
    tol: &ToleranceProfile,
) -> Vec<DiffFinding> {
    let mut out = Vec::new();

    // Only compare count if both sides expose it (upstream summary-export
    // omits count for trends).
    if let (Some(ac), Some(bc)) = (a.count, b.count) {
        if let Some(detail) = check_tolerance("count", ac as f64, bc as f64, &tol.count) {
            out.push(DiffFinding {
                kind: FindingKind::Drift,
                selector: selector.into(),
                detail,
            });
        }
    }
    for (field, av, bv) in [
        ("avg", a.avg, b.avg),
        ("med", a.med, b.med),
        ("p90", a.p90, b.p90),
        ("p95", a.p95, b.p95),
    ] {
        if let Some(detail) = check_tolerance(field, av, bv, &tol.trend) {
            out.push(DiffFinding {
                kind: FindingKind::Drift,
                selector: selector.into(),
                detail,
            });
        }
    }
    // p99 only diffed when BOTH sides expose it. Upstream's --summary-export
    // omits p(99) unless `options.summaryTrendStats` requests it; scripts that
    // want p99 parity must opt in. k6-rs always exports p(99).
    if let (Some(ap99), Some(bp99)) = (a.p99, b.p99) {
        if let Some(detail) = check_tolerance("p99", ap99, bp99, &tol.trend) {
            out.push(DiffFinding {
                kind: FindingKind::Drift,
                selector: selector.into(),
                detail,
            });
        }
    }
    out
}
