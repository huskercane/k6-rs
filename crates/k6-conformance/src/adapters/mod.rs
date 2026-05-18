//! Adapters: turn each runner's raw artifacts into a CanonicalRun.
//!
//! Spike acceptance criterion: the diff layer must have zero branches that
//! depend on which adapter produced a CanonicalRun. If an `if upstream { ... }
//! else { ... }` appears in `src/diff/`, the canonical boundary is leaking and
//! the architecture is wrong.

pub mod json_stream;
pub mod k6rs;
pub mod upstream;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::canonical::{CanonicalCheck, CanonicalGroup, CanonicalRun};

/// Raw artifacts captured from running one binary on one script.
pub struct RunArtifacts {
    /// Path to the line-delimited JSON file from `--out json=FILE`.
    pub out_json_path: std::path::PathBuf,
    /// Path to the JSON file from `--summary-export=FILE`.
    pub summary_export_path: std::path::PathBuf,
    /// Process exit code.
    pub exit_code: i32,
}

/// Adapter trait: each implementation owns parsing one runner's shapes.
pub trait Adapter {
    fn adapt(&self, artifacts: &RunArtifacts) -> Result<CanonicalRun>;
}

/// Helper: read a path as String, with context.
pub(crate) fn read_to_string(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

/// Flatten an upstream-shape `root_group` JSON tree into flat per-group and
/// per-check maps keyed by canonical path. Both adapters use this same
/// walker — k6-rs's summary schema mirrors upstream after the CG-1 bump, so
/// one DFS is enough for both sides.
///
/// The root group itself is deliberately omitted from `groups_out`: its
/// path is always `""` and it always exists on both sides by construction,
/// so diffing it adds no signal.
///
/// Unknown / missing fields are tolerated — a runner that omits `groups: {}`
/// or `checks: {}` simply contributes nothing to that map, not a parse
/// error. The walker is symmetric across adapters: zero adapter-specific
/// branches.
pub(crate) fn collect_root_group_tree(
    root_group: &Value,
    groups_out: &mut BTreeMap<String, CanonicalGroup>,
    checks_out: &mut BTreeMap<String, CanonicalCheck>,
) {
    walk_group(root_group, /* is_root */ true, groups_out, checks_out);
}

fn walk_group(
    group: &Value,
    is_root: bool,
    groups_out: &mut BTreeMap<String, CanonicalGroup>,
    checks_out: &mut BTreeMap<String, CanonicalCheck>,
) {
    let Some(obj) = group.as_object() else { return };

    // Group's own path is what becomes the `group_path` for every check
    // directly under it. Upstream emits the root path as `""`.
    let group_path = obj
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let group_name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let group_id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if !is_root {
        groups_out.insert(
            group_path.clone(),
            CanonicalGroup {
                name: group_name,
                path: group_path.clone(),
                id: group_id,
            },
        );
    }

    if let Some(checks) = obj.get("checks").and_then(Value::as_object) {
        for (_, raw_check) in checks {
            let Some(c) = raw_check.as_object() else {
                continue;
            };
            let name = c
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let path = c
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{group_path}::{name}"));
            // Capture the serialized id (md5 of path on a correct
            // implementation). The diff compares it across runners — if it
            // disagrees with the other side's id for the same path, that's
            // a serialization-layer bug, not a counts bug.
            let id = c
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let passes = c.get("passes").and_then(Value::as_u64).unwrap_or(0);
            let fails = c.get("fails").and_then(Value::as_u64).unwrap_or(0);
            checks_out.insert(
                path,
                CanonicalCheck {
                    name,
                    group_path: group_path.clone(),
                    id,
                    passes,
                    fails,
                },
            );
        }
    }

    if let Some(groups) = obj.get("groups").and_then(Value::as_object) {
        for (_, child) in groups {
            walk_group(child, false, groups_out, checks_out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collect_root_group_tree_fills_both_maps() {
        // CG-2: one DFS over the upstream-shape root_group tree fills both
        // the per-group identity map AND the per-check map. Root group is
        // intentionally excluded from `groups_out` (always present by
        // construction).
        let root = json!({
            "name": "",
            "path": "",
            "id": "d41d8cd98f00b204e9800998ecf8427e",
            "groups": {
                "api": {
                    "name": "api",
                    "path": "::api",
                    "id": "id-api",
                    "groups": {
                        "v2": {
                            "name": "v2",
                            "path": "::api::v2",
                            "id": "id-v2",
                            "groups": {},
                            "checks": {
                                "post ok": {"name": "post ok", "path": "::api::v2::post ok", "id": "x", "passes": 2, "fails": 1}
                            }
                        }
                    },
                    "checks": {
                        "v1 ok": {"name": "v1 ok", "path": "::api::v1 ok", "id": "x", "passes": 1, "fails": 0}
                    }
                },
                "audit": {
                    "name": "audit",
                    "path": "::audit",
                    "id": "id-audit",
                    "groups": {},
                    "checks": {}
                }
            },
            "checks": {
                "is healthy": {"name": "is healthy", "path": "::is healthy", "id": "x", "passes": 1, "fails": 1}
            }
        });

        let mut groups = BTreeMap::new();
        let mut checks = BTreeMap::new();
        collect_root_group_tree(&root, &mut groups, &mut checks);

        // Checks (CG-1 surface still works through the new walker).
        assert_eq!(checks.len(), 3);
        assert_eq!(checks["::is healthy"].group_path, "");
        assert_eq!(checks["::api::v1 ok"].group_path, "::api");
        assert_eq!(checks["::api::v2::post ok"].group_path, "::api::v2");

        // Groups (CG-2 surface): root excluded; every other path included
        // including the group-only `::audit` branch with no checks.
        assert_eq!(groups.len(), 3);
        assert!(
            !groups.contains_key(""),
            "root group must not be in the map"
        );
        let api = &groups["::api"];
        assert_eq!(api.name, "api");
        assert_eq!(api.id, "id-api");
        let v2 = &groups["::api::v2"];
        assert_eq!(v2.path, "::api::v2");
        let audit = &groups["::audit"];
        assert_eq!(audit.name, "audit");
        assert_eq!(audit.id, "id-audit");
    }

    #[test]
    fn missing_groups_or_checks_treated_as_empty() {
        // Tolerate the (legal) case where one of the maps is omitted —
        // upstream might serialize keys with content only; downstream
        // consumers must not panic.
        let root = json!({"name": "", "path": ""});
        let mut groups = BTreeMap::new();
        let mut checks = BTreeMap::new();
        collect_root_group_tree(&root, &mut groups, &mut checks);
        assert!(groups.is_empty());
        assert!(checks.is_empty());
    }
}
