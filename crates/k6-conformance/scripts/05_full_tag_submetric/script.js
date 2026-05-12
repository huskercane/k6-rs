import http from 'k6/http';

// CG-3 conformance script: full tag-set preservation.
//
// k6's http.get(url, { tags: { name: 'X' } }) annotates the request's
// samples with that custom tag. Upstream additionally adds the system
// tag `status` to every http sample. CG-3 in k6-rs preserves the full
// `{name:get,status:200}` combination as a real stored submetric; before
// CG-3 the combination was decomposed and lost.
//
// The threshold below targets that full-tag combination — under the
// old engine it would silently match nothing and pass vacuously (lookup
// returns 0.0, which is < 5000). After CG-3 it finds the real
// `http_req_duration{name:get,status:200}` series on both binaries.
export const options = {
  vus: 1,
  iterations: 100,
  thresholds: {
    'http_req_duration{name:get,status:200}': ['p(95)<5000'],
    'http_reqs{name:get,status:200}': ['count==100'],
  },
};

const BASE = __ENV.K6_TEST_URL;

export default function () {
  http.get(`${BASE}/get`, { tags: { name: 'get' } });
}
