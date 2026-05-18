use super::{DiffFinding, FindingKind, check_tolerance};
use crate::expectations::ToleranceProfile;

pub fn diff(
    selector: &str,
    a: (f64, f64),
    b: (f64, f64),
    tol: &ToleranceProfile,
) -> Vec<DiffFinding> {
    let mut out = Vec::new();
    let (a_count, a_rate) = a;
    let (b_count, b_rate) = b;

    if let Some(detail) = check_tolerance("count", a_count, b_count, &tol.count) {
        out.push(DiffFinding {
            kind: FindingKind::Drift,
            selector: selector.into(),
            detail,
        });
    }
    if let Some(detail) = check_tolerance("rate", a_rate, b_rate, &tol.rate) {
        out.push(DiffFinding {
            kind: FindingKind::Drift,
            selector: selector.into(),
            detail,
        });
    }
    out
}
