# k6-rs

A Rust load testing tool with partial k6 compatibility, designed for long-duration soak tests with bounded VU and HTTP concurrency.

Existing k6 scripts may need changes. See [Compatibility status](#compatibility-status) for current runtime limitations.

## Why k6-rs?

[k6](https://github.com/grafana/k6) provides the scripting model that k6-rs aims to support. Long soak tests need explicit resource limits, especially when the target slows down and requests remain in flight longer.

k6-rs controls resource use through three mechanisms:

1. **Bounded VU Pool** -- Each VU owns an isolated QuickJS runtime and a coroutine. Many VUs share Tokio loop threads. Arrival-rate executors drop iterations when no VU is available, tracking them as `dropped_iterations`. Each QuickJS heap has a 64 MiB limit; this is a limit, not an allocation at startup.

2. **Capped Response Buffers** -- The default HTTP client retains at most 10 MiB per response, or discards body data when `discardResponseBodies` is enabled. It drains the response to completion before returning the connection to the pool.

3. **Bounded In-Flight Requests** -- A Tokio semaphore limits concurrent HTTP requests to twice each scenario's VU pool size.

These limits do not guarantee a fixed process memory footprint or prevent every out-of-memory condition. Script data, metric cardinality, response buffers, and the configured number of VUs all affect memory use.

## Implementation Highlights

- **JavaScript:** QuickJS via rquickjs, with an isolated runtime per VU.
- **Execution:** coroutines multiplexed across Tokio loop threads.
- **HTTP:** Hyper + rustls by default. Set `K6RS_HTTP_CLIENT=reqwest` to use the fallback client.
- **Script analysis:** static lint warns about potentially unbounded globals (`Set`, `Map`, growing arrays).
- **DuckDB output:** `--out duckdb=file.duckdb` for post-run SQL analysis.

## Compatibility Status

k6 compatibility is partial. Implementing an API name or accepting a configuration field does not establish full upstream behavioral parity. The [conformance harness](crates/k6-conformance) compares selected workloads against upstream k6; [TEST_PARITY.md](TEST_PARITY.md) tracks coverage and gaps.

- **Executors:** six executors are supported (listed below). The compatibility reference pinned in CI is k6 **v2.0.0**, which removed `externally-controlled`. Like that release, `k6-rs run` rejects this executor during configuration validation, before `setup()`, with exit code 104.
- **Execution control:** the opt-in REST server implements the `/v1/status`, metrics, groups, setup, and teardown endpoints retained by k6 v2.0.0, including initial `--paused` execution and REST stop. Live VU updates and pauses after execution starts are rejected, matching v2.0.0. See [REST control API](#rest-control-api-k6-v200) for usage and limitations.
- **Console logging:** `console.log()`, `console.warn()`, and `console.error()` work in VU initialization, iteration functions (including named scenarios), and lifecycle hooks. Logs go to stdout; warnings and errors go to stderr. `--console-output` redirects all three levels from these contexts to one file. Formatting currently converts arguments with JavaScript `String()` and joins them with spaces; full upstream console formatting and additional console methods are not implemented.
- **Console resource limits:** the CLI shares one writer thread and a queue of 1,024 messages. VUs never wait for file or terminal I/O. Messages are limited to 16 KiB of UTF-8 text; overflow drops new messages. A warning at shutdown reports dropped/truncated messages and write failures. Shutdown allows up to two seconds to drain pending console output, then reports a timeout if the destination is still blocked.
- **Shared data:** `SharedArray`, `SharedQueue`, and `SharedCounter` have Rust implementations in `k6-core`, but are not exposed to scripts through JavaScript bindings.
- **Memory monitoring:** heap-growth analysis code exists, but is not wired into the current CLI execution path.

Additional unsupported CLI options are listed under [Not Yet Supported](#not-yet-supported).

### REST control API (k6 v2.0.0)

The server implements these endpoints retained in the [pinned upstream release](https://github.com/grafana/k6/blob/v2.0.0/api/v1/routes.go):

| Endpoint | Upstream capability |
|----------|---------------------|
| `GET /v1/status` | Execution state, active/initialized VUs, pause state, threshold failure status |
| `PATCH /v1/status` | Stop execution; resume execution initially started with `--paused` |
| `GET /v1/metrics`, `GET /v1/metrics/{id}` | Current metric aggregates |
| `GET /v1/groups`, `GET /v1/groups/{id}` | Groups and check results |
| `GET /v1/setup` | Read setup data |
| `PUT /v1/setup` | Replace or clear setup data |
| `POST /v1/setup` | Invoke `setup()` and return its data |
| `POST /v1/teardown` | Invoke `teardown()` |

Like upstream v2.0.0, the server is disabled by default. Enable it on loopback with `--address 127.0.0.1:6565` (or `K6_ADDRESS`); an empty address disables it. `--paused` / `-p` (or `K6_PAUSED=true`) waits before automatic setup and execution. Status responses use the upstream JSON API envelope. Missing metric/group IDs return 404; unsupported methods return 405.

For example, save the ramp script below as `ramp.js`, then run:

```bash
cargo run --release -p k6-rs -- run --address 127.0.0.1:6565 --paused ramp.js
```

In another terminal:

```bash
# Inspect the initially paused test, including its reserved VU capacity.
curl -s http://127.0.0.1:6565/v1/status
# Start setup and execution.
curl -s -X PATCH http://127.0.0.1:6565/v1/status \
  -H 'Content-Type: application/json' \
  -d '{"data":{"type":"status","id":"default","attributes":{"paused":false}}}'
# Inspect observed metrics and groups while the test is running.
curl -s http://127.0.0.1:6565/v1/metrics
curl -s http://127.0.0.1:6565/v1/groups
# Abort the test, interrupting in-flight iterations; teardown still runs.
curl -s -X PATCH http://127.0.0.1:6565/v1/status \
  -H 'Content-Type: application/json' \
  -d '{"data":{"type":"status","id":"default","attributes":{"stopped":true}}}'
```

REST stop returns HTTP 200 and the CLI exits with code 103. Stopping before the initial resume skips setup and teardown. Once execution has started, stopping during setup still runs teardown. Normal completion and the first Ctrl+C retain their existing graceful shutdown behavior.

Live VU scaling and pausing/resuming an already-running standard executor return HTTP 500, as in upstream v2.0.0. A combined resume/scaling request resumes first and then returns the scaling error. `PUT /v1/setup` accepts raw JSON; an empty body clears setup data, while `null` stores explicit JSON null. Lifecycle mutations are serialized; `setupTimeout` and `teardownTimeout` default to 60 seconds. The server closes at the end of the run.

Compatibility limitations: VUs are initialized lazily by this runtime, so `vus-max` reports reserved pool capacity; independent scenarios run concurrently, with `startTime` measured from a shared clock after setup. The execution window includes delayed-start gaps and counts overlapping time once. `vus` aggregates the reserved pools of running scenarios (including ramping/arrival-rate pools), while `vus_max` reports total configured capacity; these are not exact live-VU allocation measurements. Metric values retain the engine's existing HDR percentile approximation and custom-metric/tag limitations. JavaScript exception messages and some malformed-JSON diagnostic text differ from Go. Go runtime diagnostics (`/debug/pprof`, `/debug/vars`, and the process `/metrics` endpoint) are not provided. Lifecycle cancellation interrupts JavaScript, HTTP, and sleep; blocking native WebSocket/gRPC lifecycle calls retain their existing timeout behavior.


REST validation uses `python3 crates/k6-conformance/tests/rest_api.py --bin <binary>` against both the pinned upstream binary and k6-rs. It covers wire formats, initial resume, lifecycle hooks/timeouts, threshold taint, concurrent commands, ramp-to-zero/reactivation status, and interruption of sleeping and CPU-bound iterations.

For a repeatable Linux performance comparison, start `target/release/profiling_server`, then run `python3 profiling/rest_bench.py --baseline /path/to/saved-baseline --output /tmp/rest-perf`. This interleaves the baseline, REST-disabled, and REST-enabled binaries; the last variant polls status at 10 Hz. It records request rate, p95 latency, CPU time, and peak RSS, and rejects runs with failed HTTP requests.

Measured on the development host with Rust 1.99.1, five 3-second release runs per variant at 1 VU (medians; all HTTP requests successful):

| Variant | Requests/s | HTTP p95 (ms) | CPU seconds/run | Peak RSS (MiB) |
|---------|------------|---------------|-----------------|----------------|
| Before runtime changes | 14,615 | 0.058 | 2.36 | 18.62 |
| REST disabled | 14,888 | 0.054 | 2.36 | 19.72 |
| REST enabled, status polled at 10 Hz | 14,767 | 0.054 | 2.39 | 19.95 |

Three-run 10/100-VU comparisons showed similar throughput and CPU use, but both baseline and candidate had a small failed-request rate. A separate 100-VU probe reproduced 52 `send_request failed` responses in each binary, throughout execution. Those runs are not an all-success performance guarantee; the HTTP-path issue remains unresolved. REST adds approximately 1–1.3 MiB of fixed resident memory in the clean comparison.

## Features

### Executors
Six k6 executor types are supported: `constant-vus`, `constant-arrival-rate`, `ramping-vus`, `ramping-arrival-rate`, `per-vu-iterations`, and `shared-iterations`. The `externally-controlled` executor is not supported.

For a scheduled load profile starting at 10 VUs, rising to 100, and returning to 10, save this as `ramp.js`:

```javascript
import { sleep } from 'k6';

export const options = {
  scenarios: {
    load: {
      executor: 'ramping-vus',
      startVUs: 10,
      stages: [
        { duration: '5s', target: 10 },
        { duration: '10s', target: 100 },
        { duration: '10s', target: 10 },
      ],
    },
  },
};

export default function () {
  // Replace with the requests or other work you want to test.
  sleep(0.1);
}
```

Run it with `cargo run --release -p k6-rs -- run ramp.js`. The stages control VUs automatically during a single run; this example does not provide external runtime control. There is no v2.0.0-compatible REST scaling example because that capability was removed upstream.

### Protocols
- **HTTP** -- all methods, `batch()`, `asyncRequest()`, `expectedStatuses()`, CookieJar, cookies param, timeout, `.json()` with dotpath
- **WebSocket** (`k6/ws`) -- `connect()`, Socket with `send`/`close`/`ping`/`on`, interval/timeout timers
- **gRPC** (`k6/net/grpc`) -- `Client` with `connect`/`invoke`/`close`, status codes

### Modules
Module implementations include `k6/http`, `k6/ws`, `k6/net/grpc`, `k6/metrics`, `k6/crypto`, `k6/encoding`, `k6/execution`, `k6/timers`, `k6/html`, `k6/webcrypto`, `k6/experimental/csv`, `k6/experimental/fs`, `k6/experimental/streams`, and `k6/secrets`. Coverage varies by module; this list is not a claim of complete API parity.

### Output Plugins
`--out json=file.json`, `--out csv=file.csv`, `--out influxdb=http://host:8086/db`, `--out prometheus=http://host:9090/api/v1/write`, `--out duckdb=file.duckdb`. Multiple outputs can be used simultaneously.

### Networking Options
`noConnectionReuse`, `insecureSkipTLSVerify`, `tlsVersion`, `maxRedirects`, `userAgent`, `hosts`, `blacklistIPs`, `blockHostnames`, `httpDebug`, `localIPs`, `rps` (global rate limit), proxy via `HTTP_PROXY`/`HTTPS_PROXY`.

### Other
- `setup()` / `teardown()` lifecycle with data sharing
- `handleSummary(data)` for custom summary output
- Custom metrics: `Trend`, `Counter`, `Rate`, `Gauge`
- Thresholds with tag filtering (`http_req_duration{scenario:login}`)
- `check()` and `group()`
- `open()`, `fail()`, `randomSeed()`
- ES module imports (relative `./` paths)
- Extension system via `k6/x/` (loads from `extensions/` directory)
- Ctrl+C graceful shutdown

## Installation

### Pre-built Binaries

Download the latest release for your platform from the [Releases page](https://github.com/huskercane/k6-rs/releases/latest):

| Platform | Asset |
|----------|-------|
| Linux x86_64 | `k6-rs-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `k6-rs-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `k6-rs-<version>-x86_64-apple-darwin.tar.gz` |
| macOS ARM64 (Apple Silicon) | `k6-rs-<version>-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `k6-rs-<version>-x86_64-pc-windows-msvc.zip` |

```bash
# Example: Linux x86_64, replace VERSION with the tag (e.g. v0.1.0)
VERSION=v0.1.0
curl -LO "https://github.com/huskercane/k6-rs/releases/download/${VERSION}/k6-rs-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
tar xzf "k6-rs-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
sudo mv k6-rs /usr/local/bin/
```

### Build from Source

Install Rust using [rustup](https://rustup.rs/). The repository pins Rust 1.99.1
in `rust-toolchain.toml`; rustup selects and installs this toolchain automatically.
The committed `Cargo.lock` records the dependency versions used for validation;
use `--locked` to keep builds on those versions.

```bash
cargo build --release --locked
# Binary at target/release/k6-rs
```

### Profiling (CPU flame graphs)

> **The `http_bridge` Criterion bench no longer exists** — it benchmarked the
> sync execution path and was deleted with it (#6, commit `8acaa1f`). Stale
> `http_bridge-*` executables may still sit in `target/release/deps`; every
> one of them predates the coroutine runtime, so profiling them measures dead
> code. Delete them (`rm target/release/deps/http_bridge-*`) rather than let
> a "pick the newest executable" workflow silently profile one.

Profile the CLI itself. Build with frame pointers so `perf` can unwind
userspace stacks — do NOT use `--call-graph dwarf`: perf's DWARF mode
snapshots at most 64 KB of stack per sample, and the coroutine VU stacks
carry ~252 KB fat frames, so DWARF unwinding stops at the first frame and the
flame graph loses all userspace attribution (leaf libc/kernel frames only).
Frame-pointer walking has no such limit, and corosensei preserves the
frame-pointer chain across coroutine switches. `CFLAGS` covers the QuickJS C
objects. Changing `RUSTFLAGS` invalidates the build cache, so this rebuilds
everything (and the next plain `cargo build` rebuilds again).

```bash
RUSTFLAGS="-C force-frame-pointers=yes" CFLAGS="-fno-omit-frame-pointer" \
  cargo build --release
```

For apples-to-apples profiles across runs, use the canonical workload in
`profiling/bench.js` — the CLI twin of the deleted bench: plain `http.get` of
a tiny JSON body from a zero-latency local server, so the profile isolates
fixed per-iteration overhead rather than target-server or network cost. Keep
the script stable; vary VU count with `--vus` instead of editing it.

```js
// profiling/bench.js
import http from 'k6/http';

export const options = {
  vus: 1,
  duration: '60s',
};

const BASE = __ENV.K6_TEST_URL || 'http://127.0.0.1:8877';

export default function () {
  http.get(`${BASE}/`);
}
```

Start the zero-latency target server (port 8877 by default; pass a port as
the first argument and point `K6_TEST_URL` at it to override):

```bash
cargo run --release -p k6-conformance --bin profiling_server &
```

Run the script under `perf` and render the flame graph:

```bash
perf record -F 997 -g --call-graph fp -o /tmp/k6rs.perf -- \
  target/release/k6-rs run profiling/bench.js

perf record -F 997 -g --call-graph fp -o /tmp/k6rs-1000vu.perf -- \
  target/release/k6-rs run --vus 1000 --duration 60s profiling/bench.js

~/.cargo/bin/flamegraph \
  --perfdata /tmp/k6rs.perf \
  --title 'k6-rs run: profiling/bench.js' \
  -o flamegraph.svg
```

Expect one `[unknown]` frame at the base of each thread's stack (the libc
thread trampoline — system libc is built without frame pointers). Frames
above it resolve normally. If instead the graph is almost all kernel frames
with only leaf libc symbols in userspace, the binary was built without
`force-frame-pointers` — rebuild with the flags above. Thread names in the
graph are truncated to 15 chars by perf.

## Quick Start

```js
// simple_test.js
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
  vus: 10,
  duration: '30s',
};

export default function () {
  const res = http.get('https://test-api.example.com/health');
  check(res, {
    'status is 200': (r) => r.status === 200,
  });
  sleep(1);
}
```

```bash
k6-rs run simple_test.js
```

## Example: Long-Duration Soak Test

This example configures long-running arrival-rate scenarios against two API endpoints. Supply test credentials through `--env USERNAME=... --env TOKEN=...` or a `.env` file. It uses one test identity; distributing identities through `SharedArray` requires JavaScript bindings that are not yet implemented.

```js
// soak_test.js
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const user = { username: __ENV.USERNAME, token: __ENV.TOKEN };

// Custom metrics per endpoint
const loginDuration = new Trend('login_duration');
const loginFailRate = new Rate('login_failures');
const dashboardDuration = new Trend('dashboard_duration');
const dashboardFailRate = new Rate('dashboard_failures');

export const options = {
  discardResponseBodies: true,  // critical for long soak tests

  scenarios: {
    login_flow: {
      executor: 'ramping-arrival-rate',
      startRate: 10,
      timeUnit: '1s',
      preAllocatedVUs: 50,
      maxVUs: 2000,
      stages: [
        { duration: '10m', target: 50 },   // ramp up
        { duration: '7h',  target: 50 },   // sustained load
        { duration: '10m', target: 0 },    // ramp down
      ],
      exec: 'loginScenario',
    },
    dashboard_activity: {
      executor: 'ramping-arrival-rate',
      startRate: 5,
      timeUnit: '1s',
      preAllocatedVUs: 100,
      maxVUs: 3000,
      stages: [
        { duration: '10m', target: 100 },
        { duration: '7h',  target: 100 },
        { duration: '10m', target: 0 },
      ],
      exec: 'dashboardScenario',
    },
  },

  thresholds: {
    'login_duration': ['p(95)<2000'],
    'login_failures': ['rate<0.01'],
    'dashboard_duration': ['p(95)<3000'],
    'dashboard_failures': ['rate<0.01'],
  },
};

export function loginScenario() {
  const res = http.post('https://api.example.com/auth/login', JSON.stringify({
    username: user.username,
    token: user.token,
  }), {
    headers: { 'Content-Type': 'application/json' },
    timeout: '30s',
  });

  loginDuration.add(res.timings.duration);
  loginFailRate.add(res.status !== 200);

  check(res, {
    'login succeeded': (r) => r.status === 200,
  });

  sleep(1);
}

export function dashboardScenario() {
  const res = http.get('https://api.example.com/dashboard/summary', {
    headers: {
      'Authorization': `Bearer ${user.token}`,
    },
    timeout: '60s',
  });

  dashboardDuration.add(res.timings.duration);
  dashboardFailRate.add(res.status !== 200);

  check(res, {
    'dashboard loaded': (r) => r.status === 200,
  });

  sleep(1);
}
```

Run it with output to InfluxDB for Grafana dashboards:

```bash
k6-rs run soak_test.js --out influxdb=http://localhost:8086/k6results
```

Each arrival-rate scenario has a bounded VU pool. When that pool is exhausted, new iterations are dropped and counted in `dropped_iterations`. Measure memory usage with your actual script and response sizes before choosing VU limits for a long run; this example is not a benchmark or a guarantee of memory usage.

If the script accidentally has unbounded globals (like a growing `Set` or `Array`), k6-rs warns you at startup:

```
WARNING: Potential memory leak detected at line 4:
  const cache = new Set()
  → This Set grows unboundedly across iterations.
  Fix: Move inside default function, or use a bounded LRU cache.
```

## CLI Reference

k6-rs aims to support the `k6 run` workflow, but is not yet a fully compatible drop-in replacement. See [Compatibility Status](#compatibility-status) and the unsupported options below before migrating a script.

```bash
k6-rs run [OPTIONS] <SCRIPT>
```

### Execution
| Flag | Short | Description |
|------|-------|-------------|
| `--vus` | `-u` | Number of virtual users |
| `--duration` | `-d` | Test duration (e.g., "30s", "5m") |
| `--iterations` | `-i` | Total iteration limit (shared-iterations) |
| `--stage` | `-s` | Ramping stage as `duration:target` (repeatable) |
| `--config` | | JSON config file (same format as script options) |

### Output
| Flag | Short | Description |
|------|-------|-------------|
| `--out` | `-o` | Output plugin (repeatable): `json=`, `csv=`, `influxdb=`, `prometheus=`, `duckdb=` |
| `--influxdb` | | Shorthand for `--out influxdb=URL` |
| `--summary-export` | | Export end-of-test summary as JSON |

### HTTP & Networking
| Flag | Short | Description |
|------|-------|-------------|
| `--insecure-skip-tls-verify` | | Skip TLS certificate verification |
| `--no-connection-reuse` | | Disable keep-alive connections |
| `--no-vu-connection-reuse` | | Don't reuse connections between iterations |
| `--max-redirects` | | Follow at most N redirects |
| `--user-agent` | | User agent string |
| `--http-debug` | | Log HTTP requests/responses ("" or "full") |
| `--rps` | | Global rate limit (requests/sec, 0=unlimited) |
| `--throw` | `-w` | Treat HTTP errors as exceptions |
| `--discard-response-bodies` | | Don't save HTTP response bodies |
| `--blacklist-ip` | | Block IP ranges, CIDR (repeatable) |
| `--block-hostnames` | | Block hostnames, wildcards (repeatable) |
| `--local-ips` | | Source IP pool (comma-separated) |
| `--dns` | | DNS config: `ttl=5m,select=random,policy=preferIPv4` |

### Lifecycle & Runtime
| Flag | Short | Description |
|------|-------|-------------|
| `--no-setup` | | Skip `setup()` |
| `--no-teardown` | | Skip `teardown()` |
| `--no-thresholds` | | Skip threshold evaluation |
| `--no-summary` | | Skip end-of-test summary |
| `--console-output` | | Redirect VU and lifecycle console messages to a file (subject to the console resource limits above) |
| `--env` | `-e` | Set env variable as `VAR=value` (repeatable) |
| `--tag` | | Add tag to all samples as `name=value` (repeatable) |

### Environment Variables and `.env` File

**This is a k6-rs extension not available in k6.**

k6-rs automatically loads environment variables from a `.env` file in the script's directory. Variables set via `--env` / `-e` flags take priority over `.env` values.

```bash
# These are equivalent:
k6-rs run script.js -e HOST=localhost -e PORT=8080

# Or use a .env file in the same directory as script.js:
# .env
HOST=localhost
PORT=8080
```

When a `.env` file is found, k6-rs prints what it loaded:
```
  reading 2 environment variable(s) from .env file
```

#### Secret Provider Integration (`cmd:` prefix)

Values starting with `cmd:` are executed as shell commands, letting you pull secrets from any provider without storing them in plaintext. The command's stdout (trimmed) becomes the value.

```env
# .env — no secrets in this file
DB_HOST=localhost
DB_PASS="cmd:security find-generic-password -s myapp -a dbuser -w"
API_KEY="cmd:op read op://vault/api/credential"
TOKEN="cmd:vault kv get -field=token secret/app"
```

This works with secret managers that support **non-interactive** (sessionless or pre-authenticated) CLI access:

| Provider | Example | How it works without a password |
|----------|---------|-------------------------------|
| macOS Keychain | `cmd:security find-generic-password -s myapp -w` | Unlocked at login |
| Linux (GNOME Keyring) | `cmd:secret-tool lookup service myapp key password` | Unlocked at login |
| 1Password CLI | `cmd:op read op://vault/item/field` | Session from `op signin` or biometric unlock |
| HashiCorp Vault | `cmd:vault kv get -field=password secret/db` | Token cached by `vault login` |
| AWS SSM | `cmd:aws ssm get-parameter --name /app/secret --with-decryption --query Parameter.Value --output text` | IAM role or `aws configure` |
| Azure Key Vault | `cmd:az keyvault secret show --name mykey --vault-name myvault --query value -o tsv` | Session from `az login` |

**Important:** `cmd:` only works with providers that can return secrets without prompting. If a provider needs a password or interactive login, the command will fail — k6-rs runs commands with stdin closed, so there is no way to type a password. Ensure your provider session is active before running the test:

```bash
# Pre-authenticate, then run your test
op signin              # 1Password
vault login            # HashiCorp Vault
az login               # Azure
aws sso login          # AWS

k6-rs run script.js
```

A 30-second timeout prevents the test from hanging if a command unexpectedly blocks.

Output masks commands to avoid leaking secrets:
```
  reading 3 environment variable(s) from .env file
  resolving 2 environment variable(s) via shell commands
    DB_PASS ← cmd:***
    API_KEY ← cmd:***
```

### Not Yet Supported

These k6 flags are not implemented. If you need any of these, open an issue.

| Flag | Reason |
|------|--------|
| `--linger` / `-l` | Requires API server keep-alive |
| `--system-tags` | System tag filtering not implemented |
| `--summary-trend-stats` | Custom trend stat selection not implemented |
| `--summary-time-unit` | Custom time unit display not implemented |
| `--execution-segment` | Distributed execution not supported |
| `--execution-segment-sequence` | Distributed execution not supported |
| `--compatibility-mode` | Not applicable (QuickJS, not goja) |
| `--type` / `-t` | Archive format not supported |
| `--include-system-env-vars` | All system env vars are available by default |
| `--batch` | `http.batch()` concurrency limit not enforced |
| `--batch-per-host` | Per-host batch limit not enforced |
| `--min-iteration-duration` | Minimum iteration pacing not implemented |
| `--traces-output` | OpenTelemetry tracing not supported |
| `--no-usage-report` | No telemetry to disable |

## Architecture

```
crates/
  k6-core/         # Shared config, scheduling models, metrics, thresholds, outputs, data primitives
  k6-js/           # QuickJS bindings, coroutine VUs, executor pools, k6 module implementations
  k6-cli/          # Binary: CLI argument parsing, wiring, progress display
  k6-conformance/  # Black-box comparisons against upstream k6
```

The workspace includes unit tests, JS/runtime integration tests, and upstream conformance scenarios. Run `cargo test --workspace` for the Rust test suite. See [TEST_PARITY.md](TEST_PARITY.md) for conformance commands and coverage notes.

## License

AGPL-3.0
