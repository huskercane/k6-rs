import http from 'k6/http';
import { check } from 'k6';

const ICON_CACHE = new Set();
const allResults = [];

export const options = {
  vus: 500,
  duration: '8h',
};

export default function() {
  const res = http.get('https://httpbin.org/get');
  ICON_CACHE.add(res.body.substring(0, 32));
  allResults.push({ status: res.status, time: Date.now() });
  check(res, {
    'status is 200': (r) => r.status === 200,
  });
}
