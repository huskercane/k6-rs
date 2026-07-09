import http from 'k6/http';

// Executor parity: per-vu-iterations.
//
// Each VU runs its OWN fixed count of iterations (10), independently. With
// 5 VUs that is exactly 5 * 10 = 50 iterations total — a deterministic
// product, distinct from shared-iterations where the pool is global. Count
// parity is again exact; any drift means the per-VU loop over- or
// under-ran.
export const options = {
  scenarios: {
    per_vu: {
      executor: 'per-vu-iterations',
      vus: 5,
      iterations: 10,
      maxDuration: '30s',
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
