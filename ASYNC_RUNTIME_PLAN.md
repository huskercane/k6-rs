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
  runtime. **Update (2026-07-10):** this handoff is capturable by **option A**
  (co-locate I/O on the VU thread) *without* the loop migration — see the
  suspension-mechanism section. So it is a strong motivation for **A as a
  standalone optimization**, but it is **orthogonal to the north-star soak** and
  must not be counted as soak progress; the memory win needs B2, not this.
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

### The deeper constraint: sync `http.get()` can't yield on a stackless engine (2026-07-10)

Removing `block_on` is necessary but not sufficient. The load-bearing problem,
found while starting the http conversion: **QuickJS is stackless, so a
synchronous, un-awaited JS call (`const res = http.get(url)`) physically cannot
suspend the VU and hand the loop to another VU.** Only an `await` point yields.
So hosting many VUs per loop thread (the north star) forces a decision about how
a *synchronous-looking* blocking call suspends. Three options, very different
risk:

- **A — co-locate I/O, keep sync VUs (orthogonal optimization, NOT the north
  star).** Give each VU thread its own current-thread runtime + client and drive
  the request locally (`local_rt.block_on(send)` on a non-worker thread — legal).
  Removes the **futex handoff** (the measured ~17.8%) with sync `http.get`
  preserved and zero script changes. But it keeps a thread per VU, so it does
  **not** advance the fixed-memory soak. Ship it like the `max_blocking_threads`
  band-aid — a real perf win, but stop it masquerading as soak progress.
- **B1 — async VUs + AST await-transpile. REJECTED as the default.** Inject
  `await` on `http.*`/`sleep`/… and async-ify exec fns. Beyond the chaining
  hazard (`(await http.get(u)).json()`), it is **unsound in general**:
  `arr.map(http.get)` / `forEach(u => http.get(u))` needs whole-program dataflow
  ("does this callback transitively hit a blocking host fn?"), undecidable with
  dynamic dispatch. Ship-able only as best-effort-with-silent-divergence — near
  disqualifying for a **conformance-first** project. Upstream has no transpile.
- **B2 — stackful coroutine per in-flight iteration. RECOMMENDED.** The faithful
  port of upstream: k6 = each VU is a goroutine (stackful, M:N over threads);
  `http.get()` blocks the goroutine, the scheduler runs others. Rust analog: run
  each VU iteration on a stackful coroutine (`corosensei`/generator). The sync
  host fn **yields the coroutine** to the loop scheduler instead of `block_on`;
  the scheduler polls the future on tokio and resumes the coroutine when ready.
  `http.get()` stays synchronous in the script — no `await`, no transpile, exact
  k6 semantics — yet the thread yields to other VUs. That *is* pool-of-loops.
  Cost: one userspace stack per concurrently-suspended iteration (tunable, lazily
  committed, no kernel thread object, no 512 ceiling — far below thread-per-VU),
  and one contained `unsafe` integration risk (switching C-stacks under QuickJS)
  — vs B1's unbounded risk spread across every user script forever. For a
  conformance-first project, contained-in-our-runtime wins decisively.

  *(Degenerate B3 — keep sync VUs on `spawn_blocking` + the raised thread cap,
  put only genuinely-async work on loops — helps async-heavy scripts but never
  moves the reference soak, which uses sync `http.get`. Not a general answer.)*

**Consequence for phasing.** The host-fn work splits into two buckets by whether
the script already awaits it:

- **Already-awaited (mechanical, do now):** `http.asyncRequest`, and later
  promise-returning timers. Scripts `await` these, so a true async host fn (the
  `Async` adaptor, driven by the per-VU `drive()` task) is a drop-in. No
  suspension mechanism needed.
- **Sync-blocking (gated on the suspension decision):** `http.get`/`request`,
  `sleep`, sync `ws`/`grpc`. These do **not** move until B2 is proven. This is
  where the north star lives; it is *not* a mechanical site conversion.

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

Revised 2026-07-10 (twice). The "convert 4 `block_on` sites, mostly mechanical"
framing is **falsified**: only the already-awaited surface is mechanical; the
sync-blocking calls (`http.get`, `sleep`) need a suspension mechanism that is an
unmade architectural decision (A / B1 / B2 above), not a site conversion. So the
cutover is now gated on a **B2 spike** and split by bucket.

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
- **Phase 0.5 — B2 suspension spike (the new real gate for the north star).**
  Before any sync-blocking host fn moves: prove a stackful coroutine
  (`corosensei` or similar) can run a QuickJS iteration such that a
  synchronous-looking host fn **yields the loop to another VU on one thread** and
  resumes correctly. Must verify: (i) rquickjs `set_max_stack_size` SP checks
  survive a non-default stack base; (ii) panic-unwind soundness across the stack
  switch; (iii) the `unsafe` blast radius is contained. Decide B1 vs B2 on the
  result (B1 recorded as generally unsound — the default is B2). No production
  wiring. **This is what to spike next, not a transpiler.**
- **Phase 1a — Already-awaited surface (mechanical, in progress).** Make
  `http.asyncRequest` a true async host fn (the `Async` adaptor) on the async
  runtime, replacing the `Promise.resolve().then` stub; later, promise-returning
  timers. No suspension mechanism needed — scripts already `await`. *Landed so
  far:* `register_async_request` + `__http_request_async` + shared
  `build_http_request`/`finish_http_response` helpers, proven end-to-end on the
  async foundation (`async_http_request_resolves_on_async_loop`). Sync
  `__http_request` and every `http.get` caller untouched; all tests green.
- **Phase 1b — Sync-blocking cutover (gated on Phase 0.5).** With B2 proven, move
  `run_iteration` onto the loop and convert the sync-blocking host fns
  (`http.get`/`request`, `sleep`, sync `ws`/`grpc`) to coroutine-yielding — the
  remaining `block_on` sites (http 3, sleep 3, grpc 2, ws 2). Gate: all existing
  VU / http / sleep / ws / grpc / executor tests green with no sync runtime in
  the tree. This is the risk-bearing landing. **Cutover checklist:**
  - **`asyncRequest` parity (silent-divergence trap):** `__http_request_async`
    resolves a *raw* `JsHttpResponse`. The production `http.asyncRequest` wrapper
    must re-apply `__wrap_response` (`.json()`/`.html()`/`.cookies`) **and** the
    per-VU cookie jar — the old stub got both free via `__http.request`. Parity
    bar: `http_async_request_resolves_response` asserts `res.json().ok`. (Pinned
    as `TODO(cutover)` at `register_async_request`.)
  - **Canonical async host-fn pattern (copy for #2/#3):** read every `Value<'js>`
    into **owned** data *before* the `.await`, so the future captures nothing
    borrowed from `'js` and nothing `!Send` (see `__http_request_async`). Holding
    a `Ctx`/`Value` across the await is the mistake that makes the borrow checker
    fight the conversion.
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
