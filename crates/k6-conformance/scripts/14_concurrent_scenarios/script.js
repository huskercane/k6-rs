import { check, sleep } from 'k6';
import http from 'k6/http';

export const options = {
  scenarios: {
    early: { executor: 'per-vu-iterations', vus: 1, iterations: 1, exec: 'early' },
    later: { executor: 'per-vu-iterations', vus: 2, iterations: 1, exec: 'later', startTime: '400ms' },
  },
  thresholds: { checks: ['rate==1'], iterations: ['count>=3'] },
};

export function setup() { return { start: Date.now() }; }
export function early(data) {
  check(Date.now() - data.start, { 'early starts promptly': t => t < 350 });
  http.get(`${__ENV.K6_TEST_URL}/get`);
  sleep(1);
}
export function later(data) {
  // Both delayed VUs must start while early is still sleeping, independently
  // of scenario map order. A serial dispatcher fails this check.
  check(Date.now() - data.start, { 'later overlaps early': t => t >= 350 && t < 900 });
  http.get(`${__ENV.K6_TEST_URL}/get`);
  sleep(1);
}
