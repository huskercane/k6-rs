import http from 'k6/http';
import { sleep } from 'k6';

// Executor parity: constant-vus.
//
// A fixed pool of 5 VUs runs the default function back-to-back for a fixed
// 3s window. Iteration count is time-bound, not fixed — so if the loop ran
// flat out it would diverge between engines (QuickJS per-iteration overhead
// differs from Go, the same effect documented in 01_http_get). To keep the
// count a meaningful cross-engine signal we RATE-LIMIT each iteration with a
// 100ms sleep, so throughput is gated by wall-clock (~10 iters/VU/s) rather
// than engine speed: ~5 VUs * 3s * 10/s ~= 150 iterations on both. The
// defining invariant this locks is the VU gauge (exactly 5, held for the
// window); the iteration count gets a modest band for boundary effects.
export const options = {
  scenarios: {
    cvus: {
      executor: 'constant-vus',
      vus: 5,
      duration: '3s',
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
  sleep(0.1);
}
