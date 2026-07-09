import http from 'k6/http';

// Executor parity: ramping-arrival-rate.
//
// This is the executor the real OOM soak workload uses (ramping-arrival-rate,
// 7900 maxVUs in the reference test), so parity here matters most for the
// project's actual target. The open-model arrival rate ramps 0 -> 50/s over
// 2s, holds 1s, then 50 -> 0/s over 2s. The iteration total is the integral
// of that rate curve (~50 + 50 + 50 ~= 150), deterministic up to boundary
// rounding, drawn from a VU pool sized so nothing drops. A narrow band keeps
// the count honest while absorbing tail rounding.
export const options = {
  scenarios: {
    rar: {
      executor: 'ramping-arrival-rate',
      startRate: 0,
      timeUnit: '1s',
      preAllocatedVUs: 30,
      maxVUs: 30,
      stages: [
        { target: 50, duration: '2s' },
        { target: 50, duration: '1s' },
        { target: 0, duration: '2s' },
      ],
    },
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
