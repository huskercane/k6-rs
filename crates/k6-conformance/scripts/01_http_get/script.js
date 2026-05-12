import http from 'k6/http';

export const options = {
  vus: 1,
  iterations: 10,
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`);
}
