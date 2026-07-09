import http from 'k6/http';
import { sleep } from 'k6';

// Executor parity: ramping-vus.
//
// The active VU count follows a staged ramp: 0 -> 5 over 2s, hold, then
// 5 -> 0 over 2s. As with constant-vus the iteration count is time-bound, so
// the 100ms sleep rate-limits each iteration to keep the total comparable
// across engines rather than gated by per-iteration overhead. What this
// locks is the ramp SHAPE — VUs climb then drain on the same schedule — and
// a total iteration count within a band. The gauge trajectory is the real
// signal; the count band absorbs where each engine happens to be mid-iter at
// a stage boundary.
export const options = {
  scenarios: {
    rvus: {
      executor: 'ramping-vus',
      startVUs: 0,
      stages: [
        { target: 5, duration: '2s' },
        { target: 5, duration: '1s' },
        { target: 0, duration: '2s' },
      ],
      gracefulRampDown: '0s',
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
  sleep(0.1);
}
