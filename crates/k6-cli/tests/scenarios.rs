//! Scheduling regressions through the actual CLI and coroutine executors.
use serde_json::Value;
use std::process::Command;

fn run(source: &str) -> (std::process::Output, String, Value) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("test.js");
    let log = dir.path().join("console.log");
    let summary = dir.path().join("summary.json");
    std::fs::write(&script, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_k6-rs"))
        .arg("run")
        .arg(&script)
        .arg("--console-output")
        .arg(&log)
        .arg("--summary-export")
        .arg(&summary)
        .arg("--no-summary")
        .output()
        .unwrap();
    let log = std::fs::read_to_string(log)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr)));
    let summary = serde_json::from_slice(
        &std::fs::read(summary)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr))),
    )
    .unwrap();
    (output, log, summary)
}

#[test]
fn overlaps_scenarios_and_measures_delays_from_end_of_setup() {
    let (output, log, summary) = run(r#"
import { sleep } from 'k6';
export const options = {
    scenarios: {
        early: { executor: 'per-vu-iterations', vus: 1, iterations: 1, exec: 'early' },
        later: { executor: 'per-vu-iterations', vus: 2, iterations: 1, exec: 'later', startTime: '400ms' }
    },
    thresholds: { iterations: ['count>=3'] }
};
export function setup() { const until = Date.now() + 500; while (Date.now() < until) {} console.log('SETUP'); return { start: Date.now() }; }
export function early(data) { console.log('EARLY', Date.now() - data.start); sleep(1); console.log('EARLY_END'); }
export function later(data) { console.log('LATER', Date.now() - data.start); sleep(1); console.log('LATER_END'); }
export function teardown() { console.log('TEARDOWN'); }
"#);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let early_end = log.find("EARLY_END").unwrap();
    let later_start = log.find("LATER ").unwrap();
    assert!(later_start < early_end, "scenarios must overlap: {log}");
    for line in log.lines().filter(|l| l.contains("LATER ")) {
        let delay: u64 = line.split_whitespace().last().unwrap().parse().unwrap();
        assert!((350..900).contains(&delay), "delay from setup: {log}");
    }
    assert_eq!(log.matches("SETUP").count(), 1);
    assert_eq!(log.matches("TEARDOWN").count(), 1);
    assert!(log.rfind("LATER_END").unwrap() < log.find("TEARDOWN").unwrap());
    assert_eq!(summary["metrics"]["iterations"]["values"]["count"], 3.0);
    assert_eq!(summary["metrics"]["vus"]["values"]["max"], 3.0);
    assert_eq!(summary["metrics"]["vus"]["values"]["value"], 0.0);
    assert_eq!(summary["metrics"]["vus_max"]["values"]["value"], 3.0);
    // Three iterations over ~1.4s, rather than the summed ~2s scenario windows.
    let rate = summary["metrics"]["iterations"]["values"]["rate"]
        .as_f64()
        .unwrap();
    assert!((1.7..2.3).contains(&rate), "{summary}");
}

#[test]
fn includes_idle_start_time_in_duration() {
    let (output, _, summary) = run(r#"
import { sleep } from 'k6';
export const options = { scenarios: {
    delayed: { executor: 'per-vu-iterations', vus: 1, iterations: 1, startTime: '500ms' }
} };
export default function() { sleep(0.1); }
"#);
    assert!(output.status.success());
    let rate = summary["metrics"]["iterations"]["values"]["rate"]
        .as_f64()
        .unwrap();
    assert!((1.0..1.8).contains(&rate), "{summary}");
}

#[test]
fn threshold_abort_stops_overlapping_and_pending_scenarios() {
    let (output, log, summary) = run(r#"
import { sleep } from 'k6';
export const options = {
    scenarios: {
        first: { executor: 'constant-vus', vus: 1, duration: '20s', gracefulStop: '0s' },
        second: { executor: 'constant-vus', vus: 2, duration: '20s', gracefulStop: '0s' },
        pending: { executor: 'per-vu-iterations', vus: 1, iterations: 1, startTime: '10s', exec: 'pending' }
    },
    thresholds: { iterations: [{ threshold: 'count<0', abortOnFail: true }] }
};
export default function() { sleep(0.05); }
export function pending() { console.log('PENDING_RAN'); }
export function teardown() { console.log('TEARDOWN'); }
"#);
    assert_eq!(output.status.code(), Some(99));
    assert!(!log.contains("PENDING_RAN"), "{log}");
    assert!(log.contains("TEARDOWN"));
    assert_eq!(summary["metrics"]["vus"]["values"]["max"], 3.0);
    assert_eq!(summary["metrics"]["vus"]["values"]["value"], 0.0);
    assert!(String::from_utf8_lossy(&output.stderr).contains("threshold abort"));
}
