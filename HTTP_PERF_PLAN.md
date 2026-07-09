# k6-rs HTTP per-request overhead — optimization plan

Goal: close k6-rs's **fixed per-request overhead** vs upstream k6. This is a
CPU-efficiency win for the high-VU / fixed-memory goal, NOT a correctness issue.

## OUTCOME (2026-07-09) — done, all three workstreams
**The 286 µs `http_req_waiting` anomaly was a DEBUG-BUILD ARTIFACT.** Re-measured
in `--release`, `http_req_waiting` is 39–74 µs, not 286 µs. The baseline table
below (debug) overstates every absolute by ~4–7×. Always measure `--release`.

Release, 1 VU, 50k iters, zero-latency localhost (`srv.go`):
| metric | upstream v1.8.0 | reqwest (old default) | hyper (new default) | hyper + bridge fix (final) |
|---|---|---|---|---|
| `http_req_waiting` | ~55 µs | 73 µs | 39 µs | ~37 µs |
| `http_req_duration` | 63 µs | 82 µs | 62 µs | **59 µs** (below upstream) |
| `iteration_duration` | 87 µs | 133 µs | 111 µs | **104 µs** |

What shipped:
1. **Hyper is now the default HTTP client** (`crates/k6-cli/src/main.rs`).
   `K6RS_HTTP_CLIENT=reqwest` is the escape hatch. Hyper matches upstream on
   `http_req_duration` (62 vs 63 µs) where reqwest carried ~20 µs/req extra, and
   it models upstream's `sending`/`connecting` phases that reqwest's high-level
   API hides. Conformance already defaulted to hyper, so it was well-validated.
2. **Request bridge de-JSON'd** (`crates/k6-js/src/api/http.rs`): headers + tags
   now pass as native JS objects (iterated via `props()` in Rust) instead of a
   per-request `JSON.stringify` → `serde_json::from_str` round-trip that ran even
   for plain `http.get(url)`. Response body decode moved to `String::from_utf8`
   (no copy on the valid-UTF-8 fast path). Micro-bench delta: hyper −3.3%,
   reqwest −6.5%.
3. **Criterion bench harness added** (`crates/k6-js/benches/http_bridge.rs`,
   `cargo bench -p k6-js --bench http_bridge`) — benchmarks `run_iteration`
   against an in-process server for both clients so the A/B stays
   regression-locked.

Net: localhost iteration 133 → 104 µs (−22%), within 17 µs of upstream, beating
the plan's <150 µs stretch goal. Remaining ~21 µs glue is QuickJS call (~4 µs) +
metrics recording + residual allocs — diminishing returns. Regression-lock:
504 workspace tests pass, conformance shows **0 structural findings**
(`01_http_get` PASS; drift-only FAILs are k6-rs being *faster* than upstream).

--- original plan (debug-era numbers below; kept for history) ---

## Critical scope caveat (read first)
This overhead ONLY bites low-latency / high-RPS-per-VU targets. Under real
network latency it fully amortizes — measured: both engines ~230 req/s at
~42 ms/req. So: worth doing for throughput-per-core, but do NOT let it block
feature work, and always report gains as "fixed overhead", not "k6-rs is slow".

## Measured baseline (2026-07-09, default reqwest client, 1 VU, zero-latency localhost)
| stage | upstream k6 v1.8.0 | k6-rs | k6-rs share |
|---|---|---|---|
| pure JS loop (empty fn) | 0.75 µs | 12 µs | ~2% |
| HTTP round-trip (`http_req_duration`) | 63 µs | 332 µs | ~66% |
| bridge/glue (`iteration_duration − http_req`) | 24 µs | ~160 µs | ~32% |
| **total iteration** | 87 µs | 504 µs | 100% |

The 332 µs is almost all `http_req_waiting` = **286 µs** (connecting/blocked/
sending all 0 → connection reuse works). Anomalous on a zero-latency server.

## Non-goals (do not touch)
- **QuickJS / JS engine** — it's ~2% (12 µs). Swapping it is wasted effort.
- **Response marshalling** — already `IntoJs` field-by-field, no JSON round-trip
  (done 2026-03-30). Do not re-do it.

## Workstream 1 — the 286 µs `http_req_waiting` anomaly (highest value, ~57% of iter)
Hypotheses, cheapest-signal first:
1. **reqwest vs hyper**: bench used DEFAULT reqwest. Re-run with
   `K6RS_HTTP_CLIENT=hyper` and compare — one command, high signal. Hyper may
   already be leaner (this is why the hyper backend exists).
2. **per-request `block_on` sync↔async hop + backpressure semaphore** at
   `crates/k6-js/src/api/http.rs:159-162`. Each request hops sync JS thread →
   async runtime → back, acquiring a semaphore. Profile whether this hop adds
   latency between send and first-byte.
3. **timer boundary**: verify k6-rs's `waiting` window matches upstream's
   definition (send-complete → first-response-byte). If response-body read or
   marshalling is being counted inside `waiting`, it's partly mismeasurement —
   compare `Timings` construction (`crates/k6-core/src/traits.rs:68-78`) and the
   two clients' send impls (`http_client.rs:246-361`, `hyper_client.rs:480+`).
Tooling: `cargo flamegraph` (or `perf record`) on `k6-rs run -u 1 -i 50000`
against the local server below. Target: `http_req_waiting` < 50 µs on localhost.

## Workstream 2 — request-direction bridge + allocations (~160 µs glue)
- **headers + tags JSON round-trip**: `JSON.stringify` in JS (`api/http.rs:355`,
  `:361`) → `serde_json::from_str` in Rust (`:116`, `:124`). Replace with native
  value passing (build the header list on the Rust side from an `rquickjs`
  object) or avoid stringify for the common empty-headers case.
- **`from_utf8_lossy` body copy** at `api/http.rs:206` — allocates a String per
  response; keep bytes / avoid copy where the script doesn't read the body.
- **per-request allocations** assembling `HttpRequest` (`api/http.rs:151-157`).
Target: iteration-minus-http_req_duration < 60 µs.

## Workstream 3 — benchmark harness (none exists today)
No criterion / `benches/` in the workspace. Add one so gains are measurable and
regression-locked. Either a `criterion` micro-bench of `HttpClient::send` +
marshalling, or commit the repro assets below plus a runner script.

## Reproduction assets (inline so they survive)
Fast zero-latency server — `srv.go`, run `go run srv.go` (listens :8798):
```go
package main
import "net/http"
func main() {
    http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) { w.Write([]byte(`{"ok":true}`)) })
    http.ListenAndServe("127.0.0.1:8798", nil)
}
```
Scripts:
```js
// empty.js — pure JS overhead
export default function () {}
// bench_go.js — http path
import http from 'k6/http';
export default function () { http.get('http://127.0.0.1:8798/'); }
```
Commands (build once: `cargo build --bin k6-rs`; upstream at `$HOME/go/bin/k6`):
```
target/debug/k6-rs run -u 1 -i 200000 empty.js      # ~12µs iteration_duration
target/debug/k6-rs run -u 1 -i 50000  bench_go.js   # ~504µs; note http_req_waiting
K6RS_HTTP_CLIENT=hyper target/debug/k6-rs run -u 1 -i 50000 bench_go.js  # A/B
$HOME/go/bin/k6 run --quiet -u 1 -i 50000 bench_go.js                     # baseline
```
(NB: build --release for real numbers; debug inflates absolute times.)

## Acceptance / regression-lock
- After EACH change: `cargo test --workspace` green, and the conformance suite
  (`cargo run --bin k6-conformance -- run`) shows ZERO structural findings
  (`01_http_get` still PASS). Only timing/rate drift may remain.
- Success = k6-rs iteration overhead materially closer to upstream on localhost
  (stretch: < 150 µs total vs 504 µs today), with no conformance regressions.
- Re-measure with `--release`, not debug.
