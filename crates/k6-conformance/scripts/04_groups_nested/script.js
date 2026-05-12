import http from 'k6/http';
import { check, group } from 'k6';

// CG-2 conformance script: nested groups + group-only branch.
//
// Exercises three invariants in one round-trip:
//   1. Nested groups produce a real tree on both sides (api > v1, api > v2).
//   2. Checks recorded inside nested groups attach to the correct deepest node.
//   3. A group-only branch (`audit`) with no inner check still appears in
//      root_group.groups — proves group_register is called on entry, not as
//      a side effect of check/duration recording.
//
// Diff expectation: zero MissingGroup / MissingCheck / *IdMismatch findings.
// Per-group durations are intentionally NOT diffed in CG-2 (upstream's
// summary-export doesn't emit per-group duration submetrics; deferred to
// CG-3 + sink-stream parity work).
export const options = {
  vus: 1,
  iterations: 100,
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  group('api', function () {
    group('v1', function () {
      const r = http.get(`${BASE}/get`);
      check(r, { 'v1 ok': (r) => r.status === 200 });
    });
    group('v2', function () {
      const r = http.get(`${BASE}/get`);
      check(r, { 'v2 ok': (r) => r.status === 200 });
    });
  });
  group('audit', function () {
    // Group-only: no check, no metric recording. If group_register
    // regressed back to "side effect of duration recording", this group
    // would still survive (record_group_duration also registers). If it
    // regressed back to "side effect of check_add", this group would
    // silently vanish. The dedicated __group_enter hook prevents both.
  });
}
