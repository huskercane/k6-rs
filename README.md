# k6-rs

A k6-compatible load testing tool written in Rust, designed to run long-duration soak tests without running out of memory.

## Why k6-rs?

[k6](https://github.com/grafana/k6) is an excellent load testing tool, but its Go runtime can OOM during sustained high-load soak tests. When a target server slows down under a constant-arrival-rate workload, VUs pile up, response bodies accumulate, and Go's GC can't reclaim memory fast enough. An 8-hour soak test with thousands of VUs will often crash partway through.

k6-rs solves this with a **fixed-memory architecture** built on three pillars:

1. **Pre-allocated VU Pool** -- All JavaScript contexts (QuickJS via rquickjs) are created at startup and borrowed/returned per iteration. When the pool is exhausted, iterations are dropped (tracked as `dropped_iterations`), not queued in memory. Memory usage is predictable: `max_vus x ~4MB + 50MB base`.

2. **Lazy Response Bodies** -- Response bodies are not retained unless the script reads them. Bodies are dropped after `check()`. A configurable buffer cap (10MB default) prevents any single response from blowing up memory. `discardResponseBodies` is fully supported.

3. **Bounded In-Flight Requests** -- A tokio semaphore caps concurrent TCP connections at `max_vus * 2`, preventing unbounded connection growth when the target is slow.

## What's Different from k6

| Area | k6 (Go) | k6-rs |
|------|---------|-------|
| **Memory model** | GC-dependent, can OOM on long soak tests | Fixed-memory, pre-allocated VU pool |
| **JS engine** | goja (Go) | QuickJS via rquickjs (~4MB/context) |
| **Async runtime** | goroutines | tokio |
| **HTTP** | Go net/http | reqwest + rustls-tls |
| **Script analysis** | None | Static lint warns about unbounded globals (`Set`, `Map`, growing arrays) with line numbers and fix suggestions |
| **Memory monitoring** | None | Runtime heap sampling per VU, linear regression to detect memory growth before hitting limits |
| **SharedQueue** | Not available | Lock-free `crossbeam::ArrayQueue` for auth token pool patterns (take/put, borrow/return) |
| **SharedCounter** | Not available | `AtomicU64` for unique ID assignment across VUs |
| **DuckDB output** | Not built-in | `--out duckdb=file.duckdb` for post-hoc SQL analysis |

### New Shared Data Objects

k6-rs adds two new shared data primitives alongside the standard `SharedArray`:

- **`SharedQueue`** -- A bounded, lock-free queue for patterns like distributing pre-generated auth tokens across VUs. Each VU takes a token, uses it, and puts it back. No mutex contention.
- **`SharedCounter`** -- An atomic counter for generating unique IDs across VUs without synchronization overhead.

## Features

### Executors
All six k6 executors: `constant-vus`, `constant-arrival-rate`, `ramping-vus`, `ramping-arrival-rate`, `per-vu-iterations`, `shared-iterations`, plus `externally-controlled` with a REST API on port 6565.

### Protocols
- **HTTP** -- all methods, `batch()`, `asyncRequest()`, `expectedStatuses()`, CookieJar, cookies param, timeout, `.json()` with dotpath
- **WebSocket** (`k6/ws`) -- `connect()`, Socket with `send`/`close`/`ping`/`on`, interval/timeout timers
- **gRPC** (`k6/net/grpc`) -- `Client` with `connect`/`invoke`/`close`, status codes

### Modules
`k6/http`, `k6/ws`, `k6/net/grpc`, `k6/metrics`, `k6/crypto`, `k6/encoding`, `k6/execution`, `k6/timers`, `k6/html`, `k6/data` (SharedArray), `k6/webcrypto`, `k6/experimental/csv`, `k6/experimental/fs`, `k6/experimental/streams`, `k6/secrets`

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

```bash
cargo build --release
# Binary at target/release/k6-rs
```

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

## Real-World Example: 8-Hour Soak Test

This is the type of workload that crashes k6 but runs to completion with k6-rs. Multiple scenarios hit different API endpoints at sustained rates, with thousands of pre-generated user tokens distributed via `SharedArray`.

```js
// soak_test.js
import http from 'k6/http';
import { check, sleep } from 'k6';
import { SharedArray } from 'k6/data';
import { Trend, Rate } from 'k6/metrics';

// Pre-generated auth tokens loaded at init (read-only, shared across all VUs)
const users = new SharedArray('users', function () {
  return JSON.parse(open('./users.json'));  // e.g., 1000 user tokens
});

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
  const user = users[Math.floor(Math.random() * users.length)];

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
  const user = users[Math.floor(Math.random() * users.length)];

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

With k6 (Go), this test OOMs after a few hours when the target API slows down and VUs accumulate. With k6-rs, memory stays flat at ~20GB (`5000 VUs x 4MB`) for the entire 8-hour run, and if VUs are exhausted, iterations are gracefully dropped instead of crashing.

If the script accidentally has unbounded globals (like a growing `Set` or `Array`), k6-rs warns you at startup:

```
WARNING: Potential memory leak detected at line 4:
  const cache = new Set()
  → This Set grows unboundedly across iterations.
  Fix: Move inside default function, or use a bounded LRU cache.
```

## Architecture

```
crates/
  k6-core/    # Engine: VU pool, executors, metrics, thresholds, output plugins, shared data
  k6-js/      # JS runtime: QuickJS bindings, k6 module implementations
  k6-cli/     # Binary: CLI argument parsing, wiring, progress display
```

**320 tests** across the workspace covering all executors, metrics, thresholds, lifecycle, HTTP, WebSocket, gRPC, crypto, encoding, HTML parsing, output plugins, and more.

## License

AGPL-3.0
