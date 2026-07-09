import http from 'k6/http';

// Executor parity: constant-arrival-rate.
//
// Open model: the executor STARTS a new iteration at a fixed rate (50/s for
// 3s = a target of 150), independent of how long each takes, drawing from a
// pre-allocated VU pool. With 20 pre-allocated VUs and sub-ms localhost
// requests the pool never saturates, so both engines should launch the full
// ~150 iterations with zero dropped_iterations. The count is deterministic
// up to one time-unit's boundary rounding, so it gets a small relative band
// rather than `exact`.
export const options = {
  scenarios: {
    car: {
      executor: 'constant-arrival-rate',
      rate: 50,
      timeUnit: '1s',
      duration: '3s',
      preAllocatedVUs: 20,
      maxVUs: 20,
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
