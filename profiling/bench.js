// Canonical profiling workload — the CLI twin of the (deleted, #6)
// `http_bridge` criterion bench: plain http.get of a tiny JSON body from a
// zero-latency local server, so profiles isolate fixed per-iteration
// overhead. Keep this script stable so flame graphs stay comparable
// across runs; vary VU count via `k6-rs run --vus N` instead of editing it.
//
// Target server: cargo run --release -p k6-conformance --bin profiling_server
import http from 'k6/http';

export const options = {
  vus: 1,
  duration: '60s',
};

const BASE = __ENV.K6_TEST_URL || 'http://127.0.0.1:8877';

export default function () {
  http.get(`${BASE}/`);
}
