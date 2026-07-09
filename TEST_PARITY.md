# Upstream k6 Test Parity

This repository is a Rust implementation of the sibling Go project at
`../k6`. It does not currently port the full upstream Go test suite. Instead,
it has Rust-native unit tests plus a smaller conformance harness that compares
selected `k6` and `k6-rs` behaviors.

Snapshot date: 2026-07-09.

## Current Counts

| Suite | Count | Command |
| --- | ---: | --- |
| Upstream Go test files | 277 | `find ../k6 -name '*_test.go'` |
| Upstream Go `Test*` functions | 1281 | `rg '^func Test' ../k6 -g '*_test.go'` |
| Upstream Go benchmarks | 31 | `rg '^func Benchmark' ../k6 -g '*_test.go'` |
| Rust test annotations | 480 | `rg '#\[(tokio::)?test\]' crates -g '*.rs'` |
| Rust conformance scenarios | 7 | `find crates/k6-conformance/scripts -mindepth 1 -maxdepth 1 -type d` |

Rust test annotations by crate:

| Crate | Tests |
| --- | ---: |
| `k6-js` | 201 |
| `k6-core` | 199 |
| `k6-conformance` | 43 |
| `k6-cli` | 37 |

## Status Definitions

| Status | Meaning |
| --- | --- |
| Covered | Rust has focused tests for the same behavior class. |
| Partial | Rust covers important behavior, but not the upstream breadth. |
| Missing | Upstream has tests and this repo has no obvious equivalent. |
| Not applicable | The upstream subsystem is not implemented or is intentionally different here. |

## Parity Matrix

| Upstream area | Upstream files | Rust coverage | Status | Notes |
| --- | ---: | --- | --- | --- |
| JS runtime, bundling, module loading, event loop, TC39 | 110 in `internal/js` plus `js/common` and `js/promises` | `crates/k6-js/src/runtime.rs`, `vu.rs`, API module tests | Partial | Core VU lifecycle, imports, console, setup/teardown, summary hooks, and some module shims are tested. Upstream TC39, compiler, event-loop, timeout, source-map, and bundling coverage is not fully mirrored. |
| HTTP JS module | 8 in `js/modules/k6/http` | `crates/k6-js/src/api/http.rs`, e2e HTTP tests, conformance `01_http_get` | Partial | GET/POST, headers, JSON, batch, cookies, expected statuses, timing fields, and error classification have tests. Upstream async request, file upload, TLS, request construction, and response-callback breadth is not fully ported. |
| HTML JS module | 5 in `js/modules/k6/html` | `crates/k6-js/src/api/html.rs` | Partial | Selection helpers are tested. Upstream element-generation and serialization breadth is not fully mirrored. |
| Executors and scheduling | 11 in `lib/executor`, 3 in `internal/execution` | `crates/k6-core/src/executor/*`, e2e executor tests | Partial | All major executor types have Rust tests. Upstream scheduling edge cases, execution segments, and externally controlled API behavior need a deeper cross-check. |
| Metrics, thresholds, tags, summaries | 8 in `metrics`, 2 in `internal/metrics`, JS summary tests | `crates/k6-core/src/metrics.rs`, `thresholds.rs`, `summary.rs`, conformance `02` through `06` | Partial | Strong local coverage for metric storage, selectors, thresholds, summary shaping, checks, groups, and tagged submetrics. Not all upstream parser and registry edge cases are proven equivalent. |
| Output backends | 24 in `internal/output`, 8 in `output` | `crates/k6-core/src/output/*`, conformance `07_json_sink_stream` | Partial | JSON, CSV, InfluxDB, Prometheus, DuckDB, and event stream code have unit tests. Upstream cloud output and full output lifecycle behavior are not fully mirrored. |
| CLI commands and config consolidation | 30 in `internal/cmd` | `crates/k6-cli/src/main.rs`, `env.rs`, analysis tests | Partial | Env resolution, `.env`, CLI overrides, linting, memory warnings, and scenario override behavior are tested. Upstream archive, cloud, login, report, new, UI, config consolidation, and command wiring coverage is mostly absent. |
| API routes | 6 in `api/v1`, 1 in `internal/api` | Externally controlled executor API tests only | Partial | `k6-rs` has a small externally controlled REST API, but not the broader upstream API route test suite. |
| Loader, archive, filesystem helpers | 2 in `internal/loader`, archive and `lib/fsext` tests | Import/open-related tests in `k6-js`; config parsing in `k6-core` | Partial | Relative imports and `open()` behavior have local coverage. Upstream loader, archive, path trimming, and virtual filesystem behavior are not fully ported. |
| Networking options and resolvers | 6 in `lib/netext`, 5 in `lib/types` | `crates/k6-core/src/config.rs`, HTTP client tests | Partial | Hosts, block lists, TLS config parsing, local IPs, RPS, and basic HTTP client behavior have tests. Upstream resolver, dialer, trie, and IP block semantics need explicit parity tests. |
| Cloud API and cloud output | 5 in `cloudapi`, 6 in `internal/cloudapi`, 7 in `output/cloud` | No obvious Rust equivalent | Not applicable | This repo does not appear to implement upstream cloud API/login/output behavior. Keep marked not applicable unless cloud support is added. |
| UI, logging, usage, event, secretsource | Several small upstream packages | `crates/k6-cli` analysis tests and `k6-js` secrets tests | Partial | Rust has secrets API tests, but upstream UI forms, logging, usage reporting, system events, and secret source hooks are not mirrored. |

## Existing Conformance Scenarios

`crates/k6-conformance` is the closest direct parity mechanism. Current scripts:

- `01_http_get`
- `02_threshold_p99`
- `03_checks_named`
- `04_groups_nested`
- `05_full_tag_submetric`
- `06_threshold_abort`
- `07_json_sink_stream`

Run the harness with:

```bash
cargo run -p k6-conformance -- run --upstream-bin ../k6/k6 --k6rs-bin target/debug/k6-rs
```

Use `K6_BIN` and `K6RS_BIN` if the binaries live elsewhere.

## Recommended Next Steps

1. Treat `crates/k6-conformance/scripts` as the main parity backlog.
2. Add one conformance script per high-value upstream behavior instead of
   mechanically porting every Go unit test.
3. Prioritize user-visible compatibility first: HTTP module behavior, config
   parsing, executor scheduling, thresholds, summaries, outputs, and JS module
   loading.
4. For Rust-only architecture choices, keep unit tests in the owning crate and
   document why direct upstream parity is not applicable.
5. Re-run the count commands above after large upstream syncs and update this
   file when the snapshot changes.
