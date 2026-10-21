//! Exercise the real CLI wiring, including file routing and lifecycle order.
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "k6-console-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn console_file_contains_lifecycle_and_named_vu_messages_in_order() {
    let dir = TestDir::new();
    let script = dir.0.join("test.js");
    let log = dir.0.join("console.log");
    std::fs::write(&script, r#"
export const options = {
    scenarios: { test: { executor: 'per-vu-iterations', vus: 2, iterations: 2, exec: 'journey' } }
};
export function setup() { console.log('SETUP'); return { token: 'abc' }; }
export function journey(data) {
    if (console.log('ITER', __VU, __ITER, data.token) !== undefined) throw new Error('console return');
    console.warn('WARNING', null, undefined, true, 42);
    console.error('ERROR', { key: 1 });
}
export function teardown() { console.log('TEARDOWN'); }
export function handleSummary() { console.log('SUMMARY'); return {}; }
"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_k6-rs"))
        .args([
            "run",
            script.to_str().unwrap(),
            "--console-output",
            log.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.starts_with("SETUP\n"), "{text}");
    assert!(text.ends_with("TEARDOWN\nSUMMARY\n"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("ITER "))
            .count(),
        4,
        "{text}"
    );
    assert_eq!(
        text.matches("WARN: WARNING null undefined true 42\n")
            .count(),
        4
    );
    assert_eq!(text.matches("ERROR: ERROR [object Object]\n").count(), 4);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("ITER "));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("WARN: WARNING"));
}

#[test]
fn default_vu_console_routes_levels_to_terminal() {
    let dir = TestDir::new();
    let script = dir.0.join("test.js");
    std::fs::write(
        &script,
        r#"
export default function () {
    console.log('VU-STDOUT');
    console.warn('VU-WARN');
    console.error('VU-ERROR');
    console.log();
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_k6-rs"))
        .args([
            "run",
            script.to_str().unwrap(),
            "--iterations",
            "1",
            "--vus",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("VU-STDOUT\n\n"), "{stdout}");
    assert!(!stdout.contains("VU-WARN"));
    assert!(stderr.contains("WARN: VU-WARN\n"), "{stderr}");
    assert!(stderr.contains("ERROR: VU-ERROR\n"), "{stderr}");
    assert!(!stderr.contains("VU-STDOUT"));
}

#[test]
fn oversized_console_messages_are_bounded_and_reported() {
    let dir = TestDir::new();
    let script = dir.0.join("test.js");
    let log = dir.0.join("console.log");
    std::fs::write(
        &script,
        r#"
export default function () {
    let converted = 0;
    console.log('界'.repeat(20000), { toString() { converted++; return 'tail'; } });
    if (converted !== 1) throw new Error('lost conversion side effect');
    console.log('😀'.repeat(20000));
    console.log('AFTER');
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_k6-rs"))
        .args([
            "run",
            script.to_str().unwrap(),
            "--iterations",
            "1",
            "--vus",
            "1",
            "--console-output",
            log.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(log).unwrap();
    assert_eq!(
        text,
        format!(
            "{}\n{}\nAFTER\n",
            "界".repeat(16384 / 3),
            "😀".repeat(16384 / 4)
        )
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("2 truncated"));
}
