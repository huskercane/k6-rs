import http from 'k6/http';

// CG-4 conformance script: threshold lifecycle with abortOnFail.
//
// `http_reqs` accumulates rapidly under sustained load — within a second
// it's well above 5. The threshold `count<5` is immediate-failure; with
// `abortOnFail: true` and `delayAbortEval: '500ms'` upstream's periodic
// evaluator (and k6-rs's CG-4 equivalent) cancels the run after 500ms of
// continuous failure. Without CG-4, the run would complete the full 10s
// duration and only fail at end-of-run threshold eval (still exit 99,
// but visibly different wall-clock behavior and total iteration count).
//
// Both binaries should:
//   - exit 99 (threshold failed),
//   - well before the 10s natural end (because of abort),
//   - with similar iteration counts (both abort at roughly the same
//     elapsed time once the threshold has been failing for 500ms).
export const options = {
  vus: 1,
  duration: '10s',
  thresholds: {
    'http_reqs': [
      { threshold: 'count<5', abortOnFail: true, delayAbortEval: '500ms' },
    ],
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
