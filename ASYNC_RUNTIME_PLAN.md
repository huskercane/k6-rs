# Async Runtime Migration Plan

This plan tracks migrating k6-rs from the current **synchronous** QuickJS
runtime to an **async** one (rquickjs `AsyncRuntime`/`AsyncContext` + tokio), so
the async k6 JS surface behaves like upstream: truly non-blocking
`http.asyncRequest`, non-blocking timers, and concurrent in-VU promises.

Status: **proposed** — not started. This is an epic, tracked separately from the
Tier-1 parity work so it gets its own design + conformance pass.

## Motivation

Upstream k6's per-VU event loop is single-threaded (goja), but I/O runs off the
loop (a goroutine) and results are marshaled back via `RegisterCallback`. That
is what makes a single VU's `Promise.all([http.asyncRequest(...), ...])` run
concurrently, and what makes `setTimeout` non-blocking.

k6-rs today is fully synchronous:

- `runtime.rs` builds a `rquickjs::Runtime` (not `AsyncRuntime`); promises drain
  via `execute_pending_job()`.
- Each VU runs inside `tokio::task::spawn_blocking`; the QuickJS `Ctx` is
  `!Send`, pinned to that blocking thread.
- `api/timers.rs` implements `setTimeout` with `std::thread::sleep` — it
  **blocks the whole VU**.
- `http.asyncRequest` is a `Promise.resolve().then(() => sync request)` stub —
  result-correct, but it runs when its microtask drains, so `Promise.all` is
  sequential, not concurrent.

Two concrete gaps vs upstream (and vs real JS semantics):

1. `asyncRequest` is not actually async (no in-VU I/O concurrency).
2. `setTimeout`/`setInterval` block the VU instead of yielding.

A third, structural gap surfaced while reviewing this plan against the code
(2026-07-10): `main.rs` is a bare `#[tokio::main]`, so `max_blocking_threads`
defaults to **512**. Every executor drives VUs via `spawn_blocking`, and each
in-flight iteration holds its blocking thread for the *whole* iteration (the
`handle.block_on` I/O wait included). Backpressure defaults to `max_vus * 2`
(15800 at 7900 VUs) so it never binds first. Net effect: arrival-rate executors
can never have more than ~512 iterations in flight regardless of maxVUs, and
constant/ramping-vus (one long-lived `spawn_blocking` loop per VU) leave every
VU past the 512th **queued and never started**. This is a **live bug, not just
epic motivation**: the reference soak (7900 VUs) is currently broken above 512
VUs today. **File and land the band-aid now, independently of this epic** —
raise `max_blocking_threads` (or configure the runtime `Builder`) sized to
maxVUs. Caveat to record on that ticket: the band-aid trades the ceiling for
memory — ~2 MB stack × 7900 threads ≈ tens of GB — which fights the fixed-memory
goal, so it is a stopgap. Moving VUs onto async loops removes the ceiling
structurally *without* the stack cost, which is a more concrete argument for this
epic than the flamegraph below.

## What tokio buys beyond parity

Same semantics as upstream, plus a narrow efficiency edge: tokio's epoll-based
tasks scale to very high in-flight-request counts more cheaply than goroutine
stacks. For a fixed-memory 8-hour soak (the project's north star) that is a real,
if secondary, win. The primary value is faithful async behavior, not raw speed.

## Profiling Evidence (2026-07-10)

Profiled the `http_get_iteration/hyper` criterion bench (in-process Axum server,
`perf record -F 997 --call-graph dwarf`, 60 s / 86k samples). Two independent
observations bear on this epic:

- **Sync→async bridge handoff ≈ 17.8% of on-CPU time in `futex`**, split across
  both threads: the sync VU thread (`http_bridge`) blocks in `futex_wait` (~8%)
  while the single `tokio-rt-worker` performs the I/O and wakes it
  (`futex_wake`, ~10%). This is the per-request wait/wake pair inherent to
  running the VU on a `spawn_blocking` thread and marshaling the request to the
  runtime. Running the VU *on* the async loop (this epic's Phase 1) removes the
  handoff, and with it this cost. **This is the strongest quantitative
  motivation for the migration to date.**
- The worker also spends ~14.6% parked in `epoll_wait`→`schedule_hrtimeout` —
  i.e. it is *not* CPU-saturated. Throughput on this bench is gated by handoff
  latency, not compute, which is exactly what the async model addresses.

**Caveats — do not over-read this capture:**

- The bench's server is **in-process**, so ~13% of the "send" cost is the
  loopback RX softirq delivered inline under `writev`. That work leaves the box
  against a real target; do **not** optimize the TCP path based on it.
- This particular SVG was **poorly symbolized on the userspace side** (release
  bench binary under DWARF unwinding — zero `hyper`/`quickjs`/`k6_*` frames
  resolved; stacks bottom out in libc/kernel). Kernel-side costs (futex, epoll,
  tcp) are reliable; *attribution to specific Rust code is not*. Before using a
  flamegraph to justify or validate the migration, **rebuild the bench with
  frame pointers / full debuginfo** (e.g. `RUSTFLAGS="-C force-frame-pointers=yes"`
  and `debug = true` on the bench profile) and re-capture, or the userspace
  hotspots stay invisible.

### Landed independently (not blocked on this epic)

Two per-iteration costs surfaced by earlier (well-symbolized) profiles were
fixed 2026-07-10 and are **not** part of the async migration:

- `vu.rs` `run_iteration` read setup data via `ctx.eval("<source string>")`
  every iteration — re-parsing + re-compiling on each pass (a QuickJS
  `js_parse_*`/`__JS_NewAtom` hotspot). Replaced with a direct `globals.get`.
- `metrics.rs` `record_http_request_*` rebuilt a `BTreeMap` + reformatted the
  canonical key for each of 9 tagged metrics per request. Now canonicalizes the
  shared tag set once and appends each name.

Combined, these cut the hyper iteration path **~20%** (criterion,
p < 0.05). They reduce the *allocation/compile* share of the profile but do
**not** touch the futex handoff above — that remains this epic's target.

## Target Architecture

**Sharding model: pool-of-loops (decided 2026-07-10, not thread-per-VU).**

- `AsyncRuntime` + `AsyncContext` (rquickjs `futures` feature, added alongside
  the existing `parallel`).
- **N executor threads (N ≈ cores), each a current-thread tokio runtime +
  `LocalSet` hosting many VUs.** `!Send` `Ctx` only requires a VU be pinned to
  *a* thread — not its *own* thread — so one `LocalSet` cooperatively schedules
  many `!Send` contexts at their await points.
- **Keep one isolated `AsyncRuntime` per VU — do NOT collapse to one runtime per
  thread.** `!Send` constrains *thread pinning*, not *heap sharing*: you can host
  7900 separate `AsyncRuntime`s (each its own heap + its own `set_memory_limit`)
  across ~cores threads via `LocalSet`s and still get the full thread-count win.
  Collapsing to ~cores runtimes would break the two isolation properties the code
  deliberately maintains today — per-VU runtime isolation (`vu.rs:16`) and the
  per-VU **64 MB** cap (`runtime.rs:12`) — turning that cap into a per-thread-
  group budget where one VU's leak starves its neighbors. For a fixed-memory
  8-hour soak that per-VU bound is exactly the property you cannot quietly drop.
- **So the win is thread stacks, not runtime count.** At 7900 maxVUs: pool-of-
  loops = ~cores threads but **still 7900 isolated runtimes**, versus thread-per-
  VU = 7900 threads + 7900 runtimes. The 7900 JS heaps exist under *either* model
  (that is the inherent VU-pool cost); pool-of-loops saves ~7900 OS-thread stacks
  (tens of GB at default stack size) and the scheduler pressure of 7900 threads.
  That stack saving — not fewer heaps — is why thread-per-VU was rejected.
- Rust `async` host functions map to JS promises via rquickjs; `ctx.spawn()`
  (or `async_with!`) drives them. `asyncRequest` becomes a real async host fn
  that awaits the HTTP client future and resolves the promise on completion.
- Timers re-implemented on `tokio::time` so `setTimeout` yields instead of
  blocking; the event loop advances other work while a timer is pending.

### Critical constraint: no sync `block_on` once the VU is on a loop

Today's I/O host fns (`http.rs`, `ws.rs`, `grpc.rs`, `sleep.rs`) call
`handle.block_on(...)`. That is legal *only* because VUs run on `spawn_blocking`
threads, which are not runtime workers. The moment a VU runs on a current-thread
runtime, `Handle::block_on` **panics** ("cannot block within an async context"),
and `block_in_place` is not an escape hatch (it needs a multi-thread runtime,
which this is not). Therefore **the VU cannot move onto the loop while HTTP stays
synchronous** — the I/O host fns must go async *in the same step*. This collapses
the plan's old "Phase 1 = VU on async with sync-shimmed host APIs" gate, which is
unreachable. See the revised phases below.

## Blast Radius

- `runtime.rs`: `create_runtime`/`create_context` and `drain_pending_jobs`
  (→ driven by the async executor). 18 files call these or `ctx.with`.
- **Every `ctx.with(|ctx| {...})` call site** (87 production `.with(` sites in
  `k6-js`; heaviest `vu.rs` 29, `http.rs` 27) becomes `.with().await` /
  `.async_with(async |ctx| ...).await` or moves inside a spawned future. This is
  the bulk of the mechanical work. (Note: `grep '.with('` currently returns 92 —
  the extra 6 are the throwaway `async_spike` module, deleted at Phase 1.)
- `vu.rs` `run_iteration`: sync → async; VUs stop being `spawn_blocking` and run
  on their per-VU current-thread runtime.
- All six executors: their `spawn_blocking` VU-driving model changes to async
  task spawning (keep the arrival-curve / dropped-iteration logic intact —
  that is orthogonal and already correct).
- `api/timers.rs`: reimplement on `tokio::time`.
- `api/ws.rs`, `api/grpc.rs`: candidates to become genuinely streaming/async
  afterward (follow-on, not required for the core migration).

## Migration Phases

Revised 2026-07-10 after code review. **These are not four independently
shippable landings.** The block_on constraint plus the "no `#[cfg]` fork —
branch-wide switch" decision force Phase 1 to be **big-bang**: VU-on-loop + all
four `block_on` host fns + every `.with(` site convert together, with no sync
fallback in the tree. Phases 2–4 layer scale, timers, and conformance on top of
that single cutover. Treat Phase 1 as the risk-bearing landing; the rest are
follow-ups, not small increments.

- **Phase 0 — Spike + decide (the real gate).** Behind a throwaway cargo
  feature, stand up `AsyncRuntime`/`AsyncContext` and prove: (a) a Rust async
  host fn resolves a JS promise from an awaited future; (b) **one `LocalSet` on
  one thread hosts ≥2 VUs, each with its own isolated `AsyncRuntime` + intact
  64 MB `set_memory_limit`, making overlapping async calls** — this validates
  pool-of-loops *and* per-VU isolation together; (c) **name the driver** — decide
  who advances each VU's `AsyncRuntime` job queue on the shared thread (e.g.
  `spawn_local` a per-runtime driver future that awaits `rt.idle()` / the
  executor), since "N independent runtimes cooperatively driven on one thread"
  is the actual mechanism under test. Re-symbolize the bench here
  (`-C force-frame-pointers=yes` + `debug=true`) so go/no-go rests on a clean
  capture, not inferred kernel frames. Output: confirmed mechanism + isolation on
  a handful of VUs, plus a measured **per-VU RSS/thread delta** extrapolated to
  7900 — Phase 0 does **not** measure a real 7900-VU number (a small spike
  structurally can't); that validation is Phase 2's soak gate. The feature gate
  does **not** outlive this phase (`#[cfg]`-ing 87 `.with(` sites into sync+async
  variants is unmaintainable).
- **Phase 1 — Big-bang cutover: VUs on loops + all four `block_on` host fns
  async.** Move `run_iteration` async onto a current-thread runtime + `LocalSet`,
  and convert **all four** blocking host fns in the same step: `http`
  (`request`/`asyncRequest`), **`sleep`** (in nearly every k6 script — the gate
  is unreachable without it), **`ws`**, and **`grpc`**. All `block_on` call sites
  removed (http 3, sleep 3, grpc 2, ws 2 = 10). Gate: all existing VU / http /
  sleep / ws / grpc / executor tests green with no sync runtime in the tree.
- **Phase 2 — Scale to N loops + wire all six executors.** Shard the VU pool
  across N current-thread runtimes; keep the arrival-curve integral and
  dropped-iteration accounting untouched. Gate: 7900-VU soak holds **bounded RSS
  *and* thread count** (new test — the north star demands it), and >512
  concurrent VUs all make progress (regression-locks the blocking-pool ceiling).
- **Phase 3 — timers non-blocking.** Reimplement `setTimeout`/`setInterval` on
  `tokio::time`. Conformance script: a pending timer yields the loop to other
  work rather than sleeping the VU.
- **Phase 4 — concurrency conformance + cleanup.** `Promise.all([...])` of N
  requests overlaps in wall-clock (total ≈ max(single), not sum). Remove the
  sync runtime and the `Promise.resolve().then` stub. Follow-on, not in this
  epic: streaming `ws`/`grpc`.

### Phase 0 — Results (2026-07-10) ✅ mechanism + isolation proven

Spike landed behind the throwaway `async-spike` feature
(`crates/k6-js/src/async_spike.rs`, deleted at Phase 1). `cargo test -p k6-js
--features async-spike async_spike` — 3 tests green:

- **(a) mechanism** — a Rust `async` host fn (`Function::new(ctx, Async(|n| async
  {...}))`) resolves a JS `Promise` from an awaited `tokio::time::sleep`;
  `Promise.all([...])` of three 100 ms delays inside one VU overlaps (~100 ms,
  not 300 ms). rquickjs `futures` feature added alongside `parallel`.
- **(b) pool-of-loops + isolation** — two VUs, **each its own `AsyncRuntime` +
  independent 64 MB `set_memory_limit`**, run on **one** current-thread runtime +
  `LocalSet`; their two 100 ms calls overlap (<180 ms, i.e. concurrent not
  serial). Isolation confirmed two ways: a global set in VU1 is invisible in VU2,
  and exhausting VU1's (2 MB) heap does **not** affect VU2's — the per-VU cap is
  genuinely per-runtime, not per-thread-group. This validates the pool-of-loops
  design *and* kills the "collapse to ~cores runtimes" temptation.
- **(c) the driver — NAMED and CONFIRMED**: one `rt.drive()` future per VU
  runtime, `spawn_local`'d onto the shared `LocalSet`. That `DriveFuture` is what
  advances each runtime's spawned host futures + JS microtasks while the VU
  awaits. This is the mechanism Phase 1/2 build on: N loop threads, each a
  current-thread runtime hosting many (`AsyncContext`, `drive()`-task) pairs.
- **`block_on` panic — PROVEN, not just asserted**: `handle_block_on_panics_
  once_vu_runs_on_a_runtime` runs `Handle::block_on` from within a current-thread
  runtime and panics ("within a runtime"). This is the linchpin of the big-bang
  Phase 1 rationale, now demonstrated: the sync host fns cannot survive the
  VU-on-loop move.
- **Per-VU JS heap — measured on a BOOTSTRAPPED runtime (lower bound, not a
  budget)**: an *empty* interpreter is ~0.11 MB, but that is not a VU. A runtime
  that runs `vu.rs`'s dependency-free bootstrap (console/encoding/crypto/
  execution/html/secrets/csv/fs/streams/webcrypto) + a small script measures
  **~0.23 MB — 2.1× the empty interpreter** → ~**1.8 GB** partial JS-heap floor
  at 7900 VUs. Still a **lower bound**: it excludes the http/ws/grpc/metrics
  handlers and real user scripts, so the true per-VU heap is higher. Do **not**
  quote the empty-interpreter figure as "the VU heap." Real RSS at 7900 VUs is
  Phase 2's soak gate — a small spike structurally can't produce it.
- **Not yet done (deferred, not blocking Phase 1):** re-symbolizing the hyper
  bench with frame pointers. Track separately; the mechanism gate above does not
  depend on it.

## Tests / Conformance To Add

- `asyncRequest` concurrency: `Promise.all` of N requests against a fixture with
  a fixed per-request delay; assert wall-clock ≈ one request, not N.
- Timer non-blocking: `setTimeout(f, 200)` then synchronous work; assert the
  loop progressed rather than sleeping the VU.
- Regression: every existing VU/http/executor test must stay green through
  Phase 1 (the migration must be behavior-preserving for sync code).

## Risks / Open Questions

- **Sharding — RESOLVED to pool-of-loops** (N loops, each hosting many `!Send`
  contexts, **each VU keeping its own isolated `AsyncRuntime` + 64 MB limit** —
  loops share threads, not heaps). Phase 0 (b) validates the *mechanism* and that
  per-VU isolation survives on a handful of VUs, and measures per-VU overhead to
  extrapolate; the real 7900-VU RSS/thread number is measured at Phase 2's soak
  gate, not Phase 0.
- **`block_on`-in-runtime panic — RESOLVED**: http/ws/grpc/sleep go async in
  Phase 1; no sync shim survives the VU-on-loop move (see Target Architecture).
- rquickjs async executor + our `Backpressure` interplay (ordering, fairness).
  Note the 512 blocking-pool ceiling this epic removes is *separate* from
  Backpressure — do not conflate; Backpressure's `max_vus * 2` default stays.
- Cancellation: `CancellationToken` must interrupt an awaited request mid-flight
  and still return VUs to the pool cleanly (easier under async — drop the future
  — but the guard drop must be async-safe and not double-count the iteration).
- Keep the executor arrival-curve integral and dropped-iteration accounting
  (already correct) untouched by the spawn-model change.
