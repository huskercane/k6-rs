import http from 'k6/http';
import { check } from 'k6';

// CG-1 conformance script: per-check identity.
//
// Two named checks. `status is 200` is deterministic-pass (every iteration
// hits /get which returns 200). `body contains marker` is deterministic-fail
// (the fixture's /get body has no such marker). The harness will diff the
// per-check passes/fails between upstream k6 and k6-rs by check identity.
//
// Iterations is 100 so the per-check counts are integer-stable AND the
// surrounding timing/rate noise is small enough to fit within the same
// tolerance band the harness uses for 02_threshold_p99. The diff for the
// actual CG-1 surface (per-check passes/fails) is exact-match regardless of
// iteration count.
export const options = {
  vus: 1,
  iterations: 100,
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  const res = http.get(`${BASE}/get`);
  check(res, {
    'status is 200': (r) => r.status === 200,
    'body contains marker': (r) => r.body && r.body.indexOf('CG1-MARKER-NOT-PRESENT') !== -1,
  });
}
