import http from 'k6/http';

// Executor parity: shared-iterations.
//
// A fixed pool of iterations (50) is SHARED across the VUs (5) — whichever
// VU is free grabs the next iteration until the pool drains. The defining
// invariant is COUNT, not timing: exactly 50 iterations run in total,
// regardless of how the scheduler distributes them across the 5 VUs or how
// long each takes. That makes `iterations` and `http_reqs` count parity an
// exact signal — any drift means the executor mis-counted the shared pool.
export const options = {
  scenarios: {
    shared: {
      executor: 'shared-iterations',
      vus: 5,
      iterations: 50,
      maxDuration: '30s',
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
