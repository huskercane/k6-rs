import http from 'k6/http';

// P0 conformance script: exercises p(99) end-to-end.
// Enough iterations for p99 to be statistically meaningful, and a threshold
// that actually queries the real p99 from the histogram. summaryTrendStats
// opts upstream k6 into emitting p(99) in --summary-export; k6-rs always
// emits it.
export const options = {
  vus: 1,
  iterations: 100,
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(90)', 'p(95)', 'p(99)'],
  thresholds: {
    'http_req_duration': ['p(99)<5000'],
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
