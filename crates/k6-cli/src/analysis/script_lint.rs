/// Static analysis warnings for k6 scripts.
///
/// Scans JS source for patterns known to cause memory issues in long-running
/// load tests. Runs at startup before any VUs are created — zero cost.

#[derive(Debug, Clone)]
pub struct LintWarning {
    pub line: usize,
    pub severity: Severity,
    pub message: String,
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Severity {
    Warning,
    Info,
}

/// Analyze a script for risky patterns. Returns warnings sorted by line number.
pub fn lint_script(source: &str, max_vus: u32, discard_response_bodies: bool) -> Vec<LintWarning> {
    let mut warnings = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    // Track if we're inside a function or at global scope
    let mut brace_depth: i32 = 0;
    let mut has_sleep = false;
    let mut has_default_fn = false;

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        let trimmed = line.trim();

        // Track brace depth for scope analysis
        brace_depth += trimmed.chars().filter(|&c| c == '{').count() as i32;
        brace_depth -= trimmed.chars().filter(|&c| c == '}').count() as i32;

        // Detect default function
        if trimmed.contains("__k6_default") || trimmed.contains("export default function") {
            has_default_fn = true;
        }

        // Detect sleep usage
        if trimmed.contains("sleep(") {
            has_sleep = true;
        }

        // --- Check for unbounded global data structures ---

        // Global `new Set()` or `new Map()`
        if brace_depth <= 0 && (trimmed.contains("new Set(") || trimmed.contains("new Map(")) {
            let ds_type = if trimmed.contains("new Set(") {
                "Set"
            } else {
                "Map"
            };
            // Extract variable name if possible
            let var_name = extract_var_name(trimmed);
            warnings.push(LintWarning {
                line: line_num,
                severity: Severity::Warning,
                message: format!(
                    "Unbounded global {ds_type}{}",
                    var_name
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default()
                ),
                suggestion: format!(
                    "This {ds_type} will grow every iteration and never shrink. \
                     Over long tests this causes OOM.\n\
                     Fix: Clear periodically (if (__ITER % 1000 === 0) {}.clear()), \
                     or use SharedCache with a bounded size.",
                    var_name.unwrap_or("cache")
                ),
            });
        }

        // Global array with push/add patterns on subsequent lines
        if brace_depth <= 0 {
            // `const foo = []` or `let foo = []`
            if (trimmed.contains("= []") || trimmed.contains("= new Array"))
                && !trimmed.starts_with("//")
            {
                let var_name = extract_var_name(trimmed);
                // Check next ~10 lines for .push() usage
                let has_push = lines[i + 1..std::cmp::min(i + 50, lines.len())]
                    .iter()
                    .any(|l| {
                        if let Some(name) = var_name {
                            l.contains(&format!("{name}.push("))
                        } else {
                            l.contains(".push(")
                        }
                    });

                if has_push {
                    warnings.push(LintWarning {
                        line: line_num,
                        severity: Severity::Warning,
                        message: format!(
                            "Unbounded global array{}",
                            var_name
                                .map(|n| format!(" ({n})"))
                                .unwrap_or_default()
                        ),
                        suggestion: format!(
                            "This array grows via push() every iteration and is never trimmed.\n\
                             Fix: Reset at start of each iteration, use a fixed-size buffer, \
                             or move to SharedArray (read-only, shared across VUs)."
                        ),
                    });
                }
            }
        }
    }

    // --- Config-level warnings ---

    if max_vus > 100 && !discard_response_bodies {
        warnings.push(LintWarning {
            line: 0,
            severity: Severity::Info,
            message: format!(
                "{max_vus} maxVUs with discardResponseBodies: false"
            ),
            suggestion: format!(
                "Each VU keeps HTTP response bodies in memory. With {max_vus} VUs, \
                 this can use significant memory.\n\
                 If your checks only inspect r.status and r.timings, consider \
                 discardResponseBodies: true to reduce memory usage."
            ),
        });
    }

    if has_default_fn && !has_sleep {
        warnings.push(LintWarning {
            line: 0,
            severity: Severity::Info,
            message: "No sleep() in default function".to_string(),
            suggestion:
                "VUs will loop as fast as possible without think time. \
                 Add sleep(1) for realistic user simulation, or ignore \
                 this if you intentionally want max throughput."
                    .to_string(),
        });
    }

    warnings.sort_by_key(|w| w.line);
    warnings
}

/// Try to extract a variable name from a line like `const ICON_CACHE = new Set();`
fn extract_var_name(line: &str) -> Option<&str> {
    let line = line.trim();
    for prefix in &["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.split([' ', '=', ':'].as_ref()).next();
        }
    }
    None
}

/// Format warnings for terminal output.
pub fn format_warnings(warnings: &[LintWarning]) -> String {
    if warnings.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "\n\u{26a0}  SCRIPT ANALYSIS ({} warning{})\n\n",
        warnings.len(),
        if warnings.len() == 1 { "" } else { "s" }
    ));

    for (i, w) in warnings.iter().enumerate() {
        let loc = if w.line > 0 {
            format!("line {}", w.line)
        } else {
            "config".to_string()
        };
        let icon = match w.severity {
            Severity::Warning => "\u{26a0} ",
            Severity::Info => "\u{2139}  ",
        };
        out.push_str(&format!(
            "  {}{}. {} \u{2014} {}\n     {}\n\n",
            icon,
            i + 1,
            loc,
            w.message,
            w.suggestion.replace('\n', "\n     ")
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_global_set() {
        let script = r#"
const ICON_CACHE = new Set();

globalThis.__k6_default = function() {
    ICON_CACHE.add('hash123');
};
"#;
        let warnings = lint_script(script, 10, false);
        assert_eq!(warnings.len(), 2); // Set + no sleep
        let set_warning = warnings.iter().find(|w| w.message.contains("Set")).unwrap();
        assert_eq!(set_warning.severity, Severity::Warning);
        assert!(set_warning.message.contains("ICON_CACHE"));
    }

    #[test]
    fn detects_global_map() {
        let script = r#"
let cache = new Map();
globalThis.__k6_default = function() { sleep(1); };
"#;
        let warnings = lint_script(script, 10, false);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("Map"));
        assert!(warnings[0].message.contains("cache"));
    }

    #[test]
    fn detects_global_array_with_push() {
        let script = r#"
const totalPolicies = [];

globalThis.__k6_default = function() {
    totalPolicies.push({ id: 1 });
    sleep(1);
};
"#;
        let warnings = lint_script(script, 10, false);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("array"));
        assert!(warnings[0].message.contains("totalPolicies"));
    }

    #[test]
    fn ignores_local_set_inside_function() {
        let script = r#"
globalThis.__k6_default = function() {
    const localSet = new Set();
    localSet.add('ok');
    sleep(1);
};
"#;
        let warnings = lint_script(script, 10, false);
        assert!(warnings.is_empty());
    }

    #[test]
    fn warns_high_vus_without_discard() {
        let script = r#"
globalThis.__k6_default = function() { sleep(1); };
"#;
        let warnings = lint_script(script, 500, false);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("500 maxVUs"));
    }

    #[test]
    fn no_warning_with_discard_bodies() {
        let script = r#"
globalThis.__k6_default = function() { sleep(1); };
"#;
        let warnings = lint_script(script, 500, true);
        assert!(warnings.is_empty());
    }

    #[test]
    fn warns_no_sleep() {
        let script = r#"
globalThis.__k6_default = function() {
    // no sleep
};
"#;
        let warnings = lint_script(script, 10, false);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("sleep"));
    }

    #[test]
    fn clean_script_no_warnings() {
        let script = r#"
globalThis.__k6_default = function() {
    const localArr = [];
    localArr.push(1);
    sleep(1);
};
"#;
        let warnings = lint_script(script, 10, false);
        assert!(warnings.is_empty());
    }

    #[test]
    fn format_warnings_output() {
        let warnings = vec![LintWarning {
            line: 23,
            severity: Severity::Warning,
            message: "Unbounded global Set (ICON_CACHE)".to_string(),
            suggestion: "Clear periodically".to_string(),
        }];

        let output = format_warnings(&warnings);
        assert!(output.contains("SCRIPT ANALYSIS"));
        assert!(output.contains("line 23"));
        assert!(output.contains("ICON_CACHE"));
    }
}
