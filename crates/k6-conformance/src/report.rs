use crate::diff::{DiffFinding, FindingKind};

pub struct ScriptReport {
    pub name: String,
    pub findings: Vec<DiffFinding>,
}

pub fn print(reports: &[ScriptReport]) -> bool {
    let mut all_clean = true;
    for r in reports {
        if r.findings.is_empty() {
            println!("PASS  {}", r.name);
            continue;
        }
        all_clean = false;
        println!("FAIL  {}", r.name);
        for f in &r.findings {
            let tag = match f.kind {
                FindingKind::ExitCode => "exit",
                FindingKind::MissingMetric => "missing",
                FindingKind::Drift => "drift",
                FindingKind::KindMismatch => "kind",
                FindingKind::MissingCheck => "check-missing",
                FindingKind::CheckCountMismatch => "check-counts",
                FindingKind::CheckIdMismatch => "check-id",
                FindingKind::MissingGroup => "group-missing",
                FindingKind::GroupIdMismatch => "group-id",
            };
            println!("    [{tag}] {}  {}", f.selector, f.detail);
        }
    }
    all_clean
}
