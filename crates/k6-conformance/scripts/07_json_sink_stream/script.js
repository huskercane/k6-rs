import http from 'k6/http';

// CG-6 conformance script: JSON event-stream sink parity.
//
// This script's purpose is the wire format itself, not the metric semantics:
// the goal is for both upstream k6 and k6-rs to emit byte-shape-compatible
// `Metric` definitions + `Point` samples to `--out json=...`, and for the
// per-metric sample COUNTS to agree.
//
// Tag-bucket diff is intentionally NOT exercised here — k6-rs is missing
// several upstream-default system tags (name/url/proto/group/scenario)
// today and the diff would fail every script. The expectations.toml's
// known_drift acknowledges those tag-set gaps.
//
// Shape mirrors 01_http_get on purpose. Existing surfaces (checks,
// groups, thresholds) are already validated by 03/04/05/06; layering them
// here would just add debug surface area when the sink wire format is
// what's under test.
export const options = {
  vus: 1,
  iterations: 10,
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
