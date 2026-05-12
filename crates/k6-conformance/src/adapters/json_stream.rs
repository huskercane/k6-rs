//! CG-6 — JSON event-stream adapter.
//!
//! Shared by both upstream and k6-rs: after the engine work, both binaries
//! emit identical wire format (`Metric` + `Point` NDJSON, RFC3339 timestamps).
//! The adapter reads the file, normalizes selectors, counts samples per
//! metric, and reads the per-side sidecar diagnostics file if present.
//!
//! "Single parser since both sides emit the same wire format" is the
//! spike-acceptance constraint applied to sink work — no `if upstream
//! { ... }` branches here. Adapter asymmetries (e.g. upstream's sidecar
//! conventions vs k6-rs's) live in `read_sidecar`, which is honest about
//! the asymmetry: missing sidecar = zero-drop assumption with a brief
//! note for why that's safe.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use crate::canonical::{
    CanonicalEventStream, CanonicalMetricDef, CanonicalMetricKindTag, SideReliability,
};

/// Parse a `--out json=FILE` NDJSON stream into a `CanonicalEventStream`.
///
/// The parser is lenient on unknown fields and tolerant of trailing blank
/// lines. A line that fails to parse is logged via stderr but does NOT
/// abort the adapt — the goal is to surface as many findings as possible
/// per run, not bail on first surprise.
pub fn read_event_stream(path: &Path) -> Result<CanonicalEventStream> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let mut metric_defs: BTreeMap<String, CanonicalMetricDef> = BTreeMap::new();
    let mut sample_counts: BTreeMap<String, u64> = BTreeMap::new();

    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: Result<WireEvent, _> = serde_json::from_str(line);
        let Ok(ev) = parsed else {
            eprintln!(
                "warning: json_stream adapter skipping malformed line {} in {}",
                lineno + 1,
                path.display()
            );
            continue;
        };
        match ev.event_type.as_str() {
            "Metric" => {
                let Some(data) = ev.data else { continue };
                let Some(kind) = CanonicalMetricKindTag::parse(&data.metric_type) else {
                    continue;
                };
                let name = ev.metric.clone();
                // Only the FIRST definition wins. Subsequent dupes (shouldn't
                // happen if both binaries respect "def-once" — but tolerate
                // anyway) are ignored.
                metric_defs.entry(name.clone()).or_insert(CanonicalMetricDef {
                    name,
                    kind,
                    contains: data.contains.unwrap_or_default(),
                });
            }
            "Point" => {
                // Tally per-metric sample count. The metric name on the
                // envelope is the canonical base name (matches upstream's
                // `sample.Metric.Name` and k6-rs's `SinkEvent.metric_name`).
                *sample_counts.entry(ev.metric.clone()).or_insert(0) += 1;
            }
            _ => {
                // Unknown event type — ignored. This keeps the adapter
                // forward-compatible with upstream extensions (e.g. new
                // event kinds added in future k6 versions) without
                // requiring a parser bump.
            }
        }
    }

    Ok(CanonicalEventStream {
        metric_defs,
        sample_counts,
    })
}

/// CG-6 — TOLERANT sidecar read. For upstream: its `output.SampleBuffer`
/// is unbounded (`internal/output/json/json.go`) and the writer never
/// emits a sidecar by design. Absence is the EXPECTED state and means
/// zero drops by construction. Parse errors on a present-but-corrupt
/// upstream sidecar are also tolerated (upstream doesn't write one
/// today so anything found there is third-party).
pub fn read_sidecar_tolerant(stream_path: &Path) -> SideReliability {
    match read_sidecar_inner(stream_path) {
        Ok(side) => side,
        Err(_) => SideReliability::default(),
    }
}

/// CG-6 — REQUIRED sidecar read. For k6-rs: the writer task always emits
/// a sidecar at stop (see `event_stream::run_writer`). Absence or parse
/// failure indicates one of:
///   - the binary crashed before flushing the sidecar
///   - a code bug in the writer task
///   - a permissions/filesystem error preventing the write
/// All three mean we have no reliability evidence for the k6-rs side,
/// which is EXACTLY the failure mode CG-6 was meant to surface. Caller
/// must turn the Err into a `SidecarUnreadable` finding and propagate
/// the error string into `SideReliability.error` so the run is
/// classified UNRELIABLE.
pub fn read_sidecar_required(stream_path: &Path) -> Result<SideReliability, String> {
    read_sidecar_inner(stream_path)
}

fn read_sidecar_inner(stream_path: &Path) -> Result<SideReliability, String> {
    let mut p = stream_path.as_os_str().to_owned();
    p.push(".diagnostics.json");
    let content = std::fs::read_to_string(&p).map_err(|e| {
        format!("reading sidecar {}: {e}", std::path::Path::new(&p).display())
    })?;
    let parsed: WireSidecar = serde_json::from_str(&content).map_err(|e| {
        format!(
            "parsing sidecar {}: {e}",
            std::path::Path::new(&p).display()
        )
    })?;
    Ok(SideReliability {
        capacity: parsed.capacity,
        peak_occupancy: parsed.peak_occupancy,
        drops_total: parsed.dropped_samples.total,
        drops_per_metric: parsed.dropped_samples.per_metric,
        error: None,
    })
}

// --- Wire types (private) ---

#[derive(Debug, Deserialize)]
struct WireEvent {
    #[serde(rename = "type")]
    event_type: String,
    metric: String,
    #[serde(default)]
    data: Option<WireMetricData>,
}

#[derive(Debug, Deserialize)]
struct WireMetricData {
    #[serde(rename = "type", default)]
    metric_type: String,
    #[serde(default)]
    contains: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireSidecar {
    #[serde(default)]
    capacity: u64,
    #[serde(default)]
    peak_occupancy: u64,
    dropped_samples: WireDrops,
}

#[derive(Debug, Deserialize)]
struct WireDrops {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    per_metric: BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("k6rs_conformance_json_stream_tests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    /// Both Metric definitions and Point samples land in the canonical
    /// event stream. Per-metric counts are tallied independently of tag
    /// content (first-cut diff scope per CG-6 design).
    #[test]
    fn parses_metric_def_and_point_into_canonical_stream() {
        let path = write_temp(
            "stream_basic.json",
            r#"{"type":"Metric","data":{"name":"http_reqs","type":"counter","contains":"default"},"metric":"http_reqs"}
{"metric":"http_reqs","type":"Point","data":{"time":"2026-05-12T16:00:00Z","value":1,"tags":{}}}
{"metric":"http_reqs","type":"Point","data":{"time":"2026-05-12T16:00:01Z","value":1,"tags":{"method":"GET"}}}
"#,
        );
        let stream = read_event_stream(&path).unwrap();
        let def = stream
            .metric_defs
            .get("http_reqs")
            .expect("http_reqs def parsed");
        assert_eq!(def.kind, CanonicalMetricKindTag::Counter);
        assert_eq!(def.contains, "default");
        assert_eq!(stream.sample_counts.get("http_reqs"), Some(&2));
    }

    /// Tolerate malformed lines (skip + log) without aborting — the
    /// conformance harness wants to surface as many findings per run as
    /// possible, not bail on first surprise.
    #[test]
    fn skips_malformed_lines_and_continues() {
        let path = write_temp(
            "malformed_lines.json",
            r#"{"type":"Metric","data":{"name":"X","type":"counter"},"metric":"X"}
NOT JSON
{"metric":"X","type":"Point","data":{"time":"2026-05-12T16:00:00Z","value":1,"tags":{}}}
"#,
        );
        let stream = read_event_stream(&path).unwrap();
        assert_eq!(stream.sample_counts.get("X"), Some(&1));
    }

    /// Tolerant sidecar reader (upstream side): present → populates
    /// SideReliability; absent → zero-drop default. Upstream's writer is
    /// unbounded and never emits a sidecar, so absence is the expected
    /// state and must not flag as unreliable.
    #[test]
    fn sidecar_tolerant_returns_zero_drop_default_when_absent() {
        let stream_path = write_temp(
            "sidecar_present_tolerant.json",
            r#"{"type":"Metric","data":{"name":"X","type":"counter"},"metric":"X"}
"#,
        );
        let mut p = stream_path.as_os_str().to_owned();
        p.push(".diagnostics.json");
        std::fs::write(
            &p,
            r#"{"capacity":1024,"peak_occupancy":42,"dropped_samples":{"total":7,"per_metric":{"X":7}}}"#,
        )
        .unwrap();
        let rel = read_sidecar_tolerant(&stream_path);
        assert_eq!(rel.capacity, 1024);
        assert_eq!(rel.drops_total, 7);
        assert!(rel.error.is_none());

        let missing = write_temp(
            "sidecar_missing_tolerant.json",
            r#"{"type":"Metric","data":{"name":"X","type":"counter"},"metric":"X"}
"#,
        );
        let rel = read_sidecar_tolerant(&missing);
        assert_eq!(rel.drops_total, 0);
        assert!(rel.error.is_none());
    }

    /// Required sidecar reader (k6-rs side): present → Ok; absent →
    /// Err. The k6-rs writer task always emits a sidecar — absence
    /// means the writer never ran or crashed, which is exactly the
    /// reliability failure CG-6 was meant to surface. Tolerating it
    /// would silently mask the bug.
    #[test]
    fn sidecar_required_returns_err_on_missing() {
        let missing = write_temp(
            "sidecar_required_missing.json",
            r#"{"type":"Metric","data":{"name":"X","type":"counter"},"metric":"X"}
"#,
        );
        let result = read_sidecar_required(&missing);
        assert!(
            result.is_err(),
            "k6-rs side absence must surface as Err — got {result:?}"
        );
        let err = result.unwrap_err();
        assert!(err.contains("reading sidecar") || err.contains("No such file"));
    }

    /// Required sidecar reader: present but malformed JSON → Err. A
    /// corrupt sidecar is also evidence we can't trust.
    #[test]
    fn sidecar_required_returns_err_on_parse_failure() {
        let stream_path = write_temp(
            "sidecar_required_corrupt.json",
            r#"{"type":"Metric","data":{"name":"X","type":"counter"},"metric":"X"}
"#,
        );
        let mut p = stream_path.as_os_str().to_owned();
        p.push(".diagnostics.json");
        std::fs::write(&p, "NOT JSON {{{").unwrap();
        let result = read_sidecar_required(&stream_path);
        assert!(
            result.is_err(),
            "malformed sidecar must surface as Err — got {result:?}"
        );
        assert!(result.unwrap_err().contains("parsing sidecar"));
    }

    /// Trend metric defs carry `contains:"time"`; counter defs carry
    /// `contains:"default"`. The adapter must preserve the field
    /// verbatim — the diff is what checks for cross-side disagreement.
    #[test]
    fn metric_def_contains_field_is_preserved() {
        let path = write_temp(
            "contains_field.json",
            r#"{"type":"Metric","data":{"name":"http_req_duration","type":"trend","contains":"time"},"metric":"http_req_duration"}
{"type":"Metric","data":{"name":"http_reqs","type":"counter","contains":"default"},"metric":"http_reqs"}
"#,
        );
        let stream = read_event_stream(&path).unwrap();
        assert_eq!(stream.metric_defs["http_req_duration"].contains, "time");
        assert_eq!(stream.metric_defs["http_reqs"].contains, "default");
    }
}
