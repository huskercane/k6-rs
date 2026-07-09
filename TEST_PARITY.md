# Upstream k6 Test Parity

This repository is a Rust implementation of the sibling Go project at
`../k6`. The goal is to eventually mirror the upstream test suite for every
subsystem k6-rs actually implements. We get there **conformance-scenario
first** (black-box behavioral diff against a real upstream `k6` binary) and
back-fill Rust unit tests for edge cases the harness can't observe.

Snapshot date: 2026-07-09.

## Current Counts

| Suite | Count | Command |
| --- | ---: | --- |
| Upstream Go test files | 277 | `find ../k6 -name '*_test.go'` |
| Upstream Go `Test*` functions | 1281 | `rg '^func Test' ../k6 -g '*_test.go'` |
| Upstream Go benchmarks | 31 | `rg '^func Benchmark' ../k6 -g '*_test.go'` |
| Rust test annotations | 536 | `rg '#\[(tokio::)?test\]' crates -g '*.rs'` |
| Rust conformance scenarios | 13 | `find crates/k6-conformance/scripts -mindepth 1 -maxdepth 1 -type d` |

Rust test annotations by crate:

| Crate | Tests |
| --- | ---: |
| `k6-js` | 238 |
| `k6-core` | 216 |
| `k6-conformance` | 43 |
| `k6-cli` | 39 |

## Scope: what we do NOT mirror

Matching all 1281 upstream functions is the wrong target — roughly 290 of them
test subsystems k6-rs deliberately does not own. These are struck up front so
the parity backlog reflects real, reachable work. Re-evaluate only if the
underlying decision changes (e.g. cloud support is added).

| Struck area | ~Funcs | Why it is out of scope |
| --- | ---: | --- |
| `internal/js/tc39`, `tc55`, `compiler` (+ sourcemap/babel/bundler subset of `internal/js` root) | ~124 | **The engine is rquickjs (QuickJS).** ECMAScript spec conformance is QuickJS's responsibility and is validated by its own test262 runs. These upstream tests exercise goja + the babel-based compiler; re-porting them re-tests QuickJS. Keep only a thin smoke set proving k6-rs's own module resolution + init-context. |
| `cloudapi`, `internal/cloudapi`, `output/cloud` | 85 | No cloud subsystem in k6-rs. |
| `internal/js/modules/k6/websockets` | 51 | k6-rs implements the older `k6/ws` (26 upstream funcs), not the newer experimental `websockets` API. Mirror `k6/ws`; strike `websockets`. |
| `internal/cmd` cloud / login / launcher / archive tests | ~27 | No such subcommands (only a `login` stub). |

Deferred (mirror once the feature lands, not before):

| Deferred area | ~Funcs | Blocked on |
| --- | ---: | --- |
| `internal/js/modules/k6/data` (SharedArray) | 11 | SharedArray not yet implemented in k6-rs. |

Applicable, reachable universe ≈ **~990 functions**. The correctness-critical
core (executors, metrics/thresholds, HTTP) is only ~180 of those.

## Status Definitions

| Status | Meaning |
| --- | --- |
| Covered | Rust has focused tests / conformance scenarios for the same behavior class. |
| Partial | Rust covers important behavior, but not the upstream breadth. |
| Missing | Upstream has tests and this repo has no obvious equivalent. |
| Not applicable | Struck above — not implemented or intentionally delegated (e.g. to QuickJS). |

## Parity Matrix

Priority reflects blast radius for a load tester: "if this is wrong, every
result is silently wrong" outranks module breadth.

| Upstream area | Applicable funcs | Rust coverage | Status | Priority |
| --- | ---: | --- | --- | --- |
| Executors + scheduling | 74 (`lib/executor` 53, `internal/execution` 21) | 33 executor unit tests + conformance `08`–`13` (all 6 non-external executors); attempted-vs-dropped accounting | Covered (behavioral) | **T1 — in progress** |
| Metrics, thresholds, tags, summaries | 68 (`metrics` 54, `internal/metrics` 14) | `k6-core/src/metrics.rs`, `thresholds.rs`, `summary.rs`, conformance `02`–`06`; `===` threshold operator, URL-like tag-value selector parsing, `dropped_iterations` emission | Partial (broadening) | **T1** |
| HTTP JS module | 35 (`js/modules/k6/http`) | `k6-js/src/api/http.rs`, e2e HTTP tests, conformance `01`; response callbacks + `expectedStatuses`, form-urlencoded/multipart bodies, `http.file` | Partial (broadening) | **T1** |
| Output backends (non-cloud) | ~100 (`internal/output` 96 minus cloud, `output` 48) | `k6-core/src/output/*`, conformance `07`; line-protocol escaping (influx), RFC-4180 quoting (csv/duckdb), remote-write series naming + label filtering (prometheus) | Partial (broadening) | T2 |
| Networking options + resolvers | 53 (`lib/netext` 34, `lib/types` 19) | `k6-core/src/config.rs`, HTTP client tests; DNS select/policy/ttl validation, extended duration units (`ns/us/µs/ms/s/m/h/d`) | Partial (broadening) | T2 |
| CLI run + config consolidation | ~141 (`internal/cmd` minus struck) | `k6-cli/src/main.rs`, `env.rs`, analysis tests; file→script→CLI precedence with scenario-metadata preservation, `--dns` flag validation | Partial (broadening) | T2 |
| WebSocket (`k6/ws`) | 26 | `k6-js/src/api/ws.rs` | Partial | T3 |
| gRPC | 21 | `k6-js/src/api/grpc.rs` | Partial | T3 |
| Execution JS module | 14 | `k6-js/src/api/execution.rs`; zero-based `__ITER`, VU/scenario iteration-info override globals | Partial (broadening) | T3 |
| crypto / webcrypto / encoding | 17 | `k6-js/src/api/{crypto,webcrypto,encoding}.rs` | Partial | T3 |
| Event loop + task queue | 13 (`internal/js/eventloop` 11, `taskqueue` 2) | runtime/event-loop tests in `k6-js` | Partial | T3 |
| API routes | 11 (`api/v1`, `internal/api`) | externally controlled executor API tests | Partial | T4 |
| Loader, archive, fsext | 16 (`internal/loader` 10, `lib/fsext` 6) | import/`open()` tests in `k6-js` | Partial | T4 |
| HTML JS module | 9 | `k6-js/src/api/html.rs` | Partial | T4 |
| UI, event, usage, secretsource | ~14 | `k6-cli` analysis + `k6-js` secrets tests | Partial | T4 |
| JS runtime / TC39 / compiler / bundler | — | delegated to QuickJS | Not applicable | — |
| Cloud API + cloud output | — | not implemented | Not applicable | — |

## Existing Conformance Scenarios

`crates/k6-conformance` is the primary parity mechanism. Current scripts:

- `01_http_get`
- `02_threshold_p99`
- `03_checks_named`
- `04_groups_nested`
- `05_full_tag_submetric`
- `06_threshold_abort`
- `07_json_sink_stream`
- `08_exec_shared_iterations` — exact total-iteration count parity
- `09_exec_per_vu_iterations` — exact `vus * iterations` product parity
- `10_exec_constant_arrival_rate` — target-rate count parity (150/150)
- `11_exec_constant_vus` — VU gauge held, rate-limited count band
- `12_exec_ramping_vus` — staged VU ramp shape + count
- `13_exec_ramping_arrival_rate` — staged arrival-rate integral (the OOM soak executor)

Run the harness with:

```bash
cargo run -p k6-conformance -- run --upstream-bin ../k6/k6 --k6rs-bin target/debug/k6-rs
```

Use `--filter <substr>` to run a subset. `K6_BIN` / `K6RS_BIN` if the binaries
live elsewhere. Build the upstream oracle first: `cd ../k6 && go build -o k6 .`.

## Bugs Found By The Harness

- **Ramping-executor iteration under-dispatch (fixed 2026-07-09).** Scaffolding
  `12`/`13` surfaced a stable ~8% (ramping-arrival-rate) / ~12% (ramping-vus)
  iteration deficit vs upstream. Root causes: `ramping_arrival_rate.rs` slept
  `1/instantaneous-rate` between single dispatches, under-integrating the ramp;
  `ramping_vus.rs` truncated (`as u32`) the interpolated VU count, holding one
  VU short across each ramp. Fixed by integrating the arrival curve
  (catch-up-to-integral) and rounding the VU count; k6-rs now matches upstream
  within <1% / ~2%. This mattered because the 8-hour OOM soak workload runs on
  ramping-arrival-rate — it was applying ~8% less load than the equivalent
  upstream config. Regression-locked by `expected_arrivals_integrates_the_ramp`
  and `ramp_matches_integral_count` unit tests.

## Parity Fixes Surfaced While Adding Tests

Writing the Tier-1/Tier-2 tests exposed several behaviors that diverged from
upstream; each was fixed and regression-locked (2026-07-09):

- **dropped_iterations never emitted.** The metric had no production caller —
  shared/per-vu executors only tracked a summary field. Now emitted per scenario
  (`main.rs`), and counted as `total − attempted` (an errored-but-started
  iteration is not misfiled as dropped).
- **Object HTTP bodies defaulted to JSON.** Upstream sends an object body as
  `x-www-form-urlencoded`, or `multipart/form-data` when it contains an
  `http.file()`. Both encodings are now implemented; JSON requires an explicit
  `JSON.stringify()`, matching upstream.
- **Response callbacks.** `setResponseCallback` / per-request `responseCallback`
  / `expectedStatuses` with argument validation; `responseCallback: null`
  disables `http_req_failed` emission (upstream semantic).
- **Prometheus remote-write naming.** Counter → `_total` only (rate is derived
  at query time), Rate → `_rate` suffix, Trend stats → `_<stat>` suffix series;
  empty-valued labels dropped and labels sorted.
- **InfluxDB line-protocol escaping** (measurement/tag/field keys and tag
  values) and **RFC-4180 CSV quoting** (csv + duckdb) for payloads containing
  commas, spaces, `=`, or quotes.
- **DNS config validation** (`select`/`policy` enums, `ttl` duration-or-`inf`)
  and an **extended duration parser** (`ns/us/µs/ms/s/m/h/d`, fractional
  components, negative rejected).
- **`__ITER` is zero-based** to JS, matching upstream; execution-info VU/scenario
  iteration globals honor scheduler-supplied overrides.

## Known Issues

- **Sub-ms timing-band fragility in `02`–`07`.** On fast/quiet machines these
  micro-scripts fail their trend/rate bands: upstream runs sub-ms per iteration
  and k6-rs's QuickJS-vs-Go per-iteration overhead makes the *ratio* swing
  3–4× even though absolute times are all sub-millisecond (the effect
  `01_http_get` documents and neutralizes by ignoring trends/rates). Findings
  are purely `[drift]` on trend/rate metrics — counts, checks, thresholds,
  group structure, and exit codes all still match. The durable fix (tracked,
  not yet done) is to restructure these to longer-duration or
  real-latency scripts where per-iteration overhead amortizes, per the
  `01_http_get` rationale — **not** to widen the bands.

## Recommended Next Steps

Ordered by the priority column above.

1. **Finish T1.** Executors are behaviorally covered (`08`–`13`); port the
   21 `internal/execution` scheduling edge cases as unit tests, then close out
   metrics/thresholds and the remaining HTTP module breadth (async request true
   concurrency is gated on the async-runtime epic — see `ASYNC_RUNTIME_PLAN.md`).
2. **T2 outputs are now unit-locked** (escaping/naming per backend). Next: a
   live-backend conformance path (or a golden-file diff) for influx/prometheus
   line output, since the harness only covers the JSON sink (`07`) today.
3. **T2 netext depth.** DNS/duration parsing is covered; still open — resolver
   select/rotate behavior, blocklist/CIDR matching, and dialer parity
   (`lib/netext`), the 8h-soak risk surface.
4. **T2 cmd/config.** Consolidation precedence is locked; extend to the
   `cmd_run_test.go` end-to-end surface once executors/config settle.
5. **Method per area:** anything observable in output (counts, metric values,
   threshold aborts, formats) → conformance scenario; internal error strings /
   boundary rejections → ported Rust unit test.
6. Keep this file honest: re-run the count commands after large upstream syncs
   and after adding tests, and update the snapshot date.
