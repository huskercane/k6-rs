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
- **Phase 0.5 — B2 suspension spike. DONE ✅ (2026-07-10) — B2 CONFIRMED VIABLE.**
  `crates/k6-js/src/b2_spike.rs` behind the throwaway `b2-spike` feature
  (`corosensei 0.3.4`). 4 proofs green:
  - **mechanism** — a sync host fn (`__host_fetch`) called from plain sync JS (no
    `await`) yields the coroutine; the async scheduler awaits a tokio future and
    resumes; the script uses the value synchronously. Yielder reached via a
    pointer captured in the host-fn closure (no thread-local → no cross-coroutine
    staleness).
  - **cross-VU concurrency on ONE thread** — two VUs' sync fetches overlap.
  - **(i) stack checks intact** — deep JS recursion on the coroutine stack trips
    QuickJS `RangeError`, **no segfault**. Works because sync `Context::with`
    calls `update_stack_top()` on entry (`context/base.rs:122`), re-anchoring the
    256 KB limit onto the 1 MB coroutine stack. **No custom FFI needed.**
  - **(ii) panic-unwind** — a panic in the coroutine propagates through `resume`,
    catchable, no abort/UB.
  - **(iii) contained unsafe** — the entire `unsafe` surface is one `YielderPtr`
    newtype (`Send`/`Sync` asserted for the same-thread pointer, needed only
    because the `parallel` feature requires host-fn closures `Send`).
  **Decision locked: B2** (B1 stays rejected as generally unsound).

## The unified Phase 1b model (decided 2026-07-10) — coroutine-hosts-its-own-loop

Resolves the open question above. The two suspensions have **opposite borrow
behaviour**, and that asymmetry *is* the design:

- **Sync `http.get()` yield** parks the coroutine **holding** the `ctx.with`
  borrow — the Rust stack is suspended *inside* the `with` closure (the host fn
  runs there).
- **JS `await asyncRequest`** does the opposite: QuickJS unwinds its stack back
  to the `eval` caller and returns a *pending promise*, so `ctx.with` **returns
  and releases** the borrow. Multiple ops can be in flight; the promise settles
  later.

**The load-bearing invariant** (a scheduler that borrowed a VU's `Context` to
settle a promise while that VU is parked mid-`http.get` would be a re-entrant
`with` = double borrow = UB):

> **The scheduler owns only `(coroutines, futures)`. It NEVER borrows a VU's
> `Context`. Each coroutine drives its own job queue, inside its own `ctx.with`
> borrows.**

Under that rule `run_iteration` (inside the coroutine) *is* the event loop for
its own VU:

```
let script_promise = ctx.with(|ctx| eval(default_fn));   // returns pending promise, borrow RELEASED
loop {
    ctx.with(|ctx| { settle_completed_ops(); drain_pending_jobs(); }); // borrow released each pass
    if settled(script_promise) { break; }
    yield_to_scheduler();   // between with-blocks: borrow NOT held → clean
}
```

Everything falls out of this one loop:

- **Sync `http.get()`** = the **one-op degenerate case**: yield holding the
  borrow, scheduler awaits the single future, resumes. Nobody else ever touches
  that `Context` while parked, so borrow-held is harmless.
- **`Promise.all([asyncRequest, asyncRequest])` overlap** = the **multi-op
  case**: both futures registered before the `await`, both progress on the
  scheduler, settled by the driver loop when it regains control.
- **Cross-VU concurrency** is automatic: VU-A parked mid-`http.get` (borrow on
  Context-A) never impedes VU-B's coroutine on Context-B.
- **Faithful to goja:** an `asyncRequest()` then a sync `http.get()` — the async
  I/O progresses during the park, but its promise callback doesn't run until the
  coroutine returns to its driver loop. Exactly "separate goroutine does the I/O,
  callback waits for the loop to turn."

**Substrate exists on the sync path** (verified): `Ctx::promise() -> (Promise,
resolve, reject)` mints a promise whose resolver the scheduler holds;
`Runtime::execute_pending_job()` drains the `.then` callbacks. No `AsyncContext`.

**This supersedes Phase 1a's substrate.** The production VU body is a **sync
`Context`**, so `AsyncContext`/`create_async_*`/`spawn_driver` are **not** the VU
path — `asyncRequest` becomes a plain sync host fn that mints a promise
(`ctx.promise()`) and registers its future with the scheduler. What Phase 1a
*earned* survives: the **sync-prep-then-owned-future** discipline and the
`build_http_request`/`finish_http_response` helpers. `register_async_request` on
`AsyncContext` + `spawn_driver` are now spike-scaffolding, folded into cleanup.
**Phase 1a (done):** `http.asyncRequest` as a true async host fn +
`build_http_request`/`finish_http_response` helpers. Under the unified model the
`AsyncContext` substrate is superseded (see above); the helpers + discipline are
what carry forward.

### Graduation into `QuickJsVu` — coroutine/Context lifetime (decided 2026-07-10)

**Long-lived per-VU coroutine — bootstrap ONCE, loop iterations inside, yield
between them.** NOT per-iteration coroutines. The deciding constraint is one the
`vu_loop` harness hides: **host fns capture the yielder pointer**, which is
per-coroutine-run. A persistent `Context` with a per-iteration coroutine would
force a **per-VU mutable yielder slot** (updated each run) — *within-VU*
indirection + the re-bootstrap cost, NOT a correctness dead end. (Precisely: this
is **not** the cross-VU staleness the captured-pointer design killed — that was a
*thread-local* yielder shared by all VUs on a pool-of-loops thread; a per-VU slot
is never cross-VU stale.) The decisive reason for long-lived stands on its own:
**capture-the-yielder-once + bootstrap-once** (bootstrapping the full k6 API per
iteration is the per-iteration-re-eval regression class already fixed once —
catastrophic at 7900 VUs). So: bootstrap (runtime + context + whole API) at
coroutine start; then `loop { run one iteration (driver loop, yielding for I/O);
yield IterationBoundary }`; Context/globals/cookie-jar persist by construction.

**The #4↔#5 seam (R1 — resolve before writing code).** "Service I/O yields" is
**async** (awaiting tokio futures), so it is NOT the sync `run_iteration` trait
method — a sync fn awaiting = `block_on` = the loop-thread panic we've architected
around. The async **`drive_vu`** (executor-spawned onto the loop, #5) resumes the
coroutine and services `AwaitOne`/`AwaitPending`. The sync per-iteration body
(eval default fn + turn the JS event loop via **coroutine yields**, not tokio
awaits) runs *inside* the coroutine — invoked by the coroutine, never by the
executor as a blocking call. `IterationBoundary` is the handoff: `drive_vu`
returns/loops there. Do not let `run_iteration` become the executor's sync entry
that drives the async scheduler (R2 = illegal = block_on-on-loop panic).

**Long-lived's new costs (graduation spec):**
- **Per-iteration state reset (silent-if-wrong):** one reused `Context` means
  fresh globals are NOT free. **Persist** (per-VU, upstream-correct): user
  module-scope vars, cookie jar. **Reset at `IterationBoundary`** (or bleed):
  driver bookkeeping — `__done`/`__ret`/`__resolvers` and Rust-side
  `outstanding`/`registered`/`completed`/`async_meta`. Unreset `__done` →
  iteration N+1 terminates instantly; stale `__resolvers`/`async_meta` →
  mis-resolution or an 8-hour leak. Prefer Rust-side per-iteration locals
  re-init'd each `RunNext` over JS globals (smaller bleed surface). Test: iter 2
  starts clean while a module counter + a jar cookie persist.
- **Per-iteration catch boundary:** a host-fn panic now unwinds the WHOLE VU
  coroutine, killing all its remaining iterations — a VU silently dropped at
  hour 3 thins load without failing loudly. Wrap each iteration's eval in a catch
  boundary so a script/host error ends that iteration and loops on (or count +
  log whole-VU death — but for the soak, prefer the boundary).
- **Free win for #5 cancellation:** `IterationBoundary` is a clean cancel point
  (no borrow held, no I/O in flight). Two-tier shutdown: cancel at the boundary
  (drain in-flight, stop between iterations — preferred, no `force_unwind`) and
  `force_unwind` mid-`AwaitOne` only on a hard deadline. Makes the scary
  borrow-held `force_unwind` (the #5 gate) the exception, not the rule.

Three more graduation gates: (2) the async path must run the **request-side cookie merge**
(`__buildCookieHeader`) + `Set-Cookie` extract, not just `__wrap_response` —
`asyncGet` bypasses `__http.request` today; (3) `http.batch` needs a
**yield-and-wait-for-all** variant (register N ops, park until all complete) — it
is neither `AwaitOne` nor `asyncRequest`; (4) watch **native stack** — full
bootstrap + deep script + Rust frames now run on the 1 MB coroutine stack (B2
proved re-anchoring with a trivial script only); stack size becomes a tuning knob
+ a per-VU memory line at 7900.

### Phase 1b — the unified cutover (critical path)

Delivered **north-star-first**: sync `http.get` (which the reference soak uses)
lands before in-VU `asyncRequest` overlap, all on **one** driver-loop code path
(sync `http.get` is its one-op degenerate case, so deferring overlap is *not* a
shortcut — it would build a special case you later rip out).

- **1b-gate — m1/m2 mini-spike. DONE ✅ (2026-07-10) — substrate CONFIRMED.**
  In `b2_spike` (6 proofs green). **(m1)** an `async` fn's top-level `await` on
  the sync path returns a *pending* promise (borrow released); a scheduler-held
  resolver + `drain_pending_jobs` settles it to completion. **(m2)** two promises
  minted via `ctx.promise()` behind one `Promise.all` settle **out of order, with
  the second op still in flight**, across borrow boundaries, job queue intact.
  Confirms the unified model is buildable on a sync `Context` — no `AsyncContext`.
- **1b-1 — yield primitive + scheduler. DONE ✅ (2026-07-10, composition proven).**
  `crates/k6-js/src/vu_loop.rs` (still `b2-spike`-gated; wires to production
  `QuickJsVu` in #3). Real composition — a live coroutine yields, a real tokio
  future completes on the scheduler, the coroutine resumes, the driver loop
  resolves promises + drains jobs — with **both invariants** enforced: **I1** the
  scheduler owns only `(coroutines, futures)`, never a `Context`; **I2**
  (queue-don't-resolve) completions push to a per-VU Rust queue, resolved ONLY by
  the coroutine's driver loop inside its own borrow. 3 tests green, incl. the
  headline hazard `composition_async_completes_during_sync_park`: an async op
  completes *during* a sync-fetch park (borrow held) → queued, not resolved →
  resolved after the park (`s50|a10`, no double-borrow). Two delivery modes over
  one yield primitive: `AwaitOne` (sync, direct resume) / `AwaitPending` (async,
  driver-loop-applied).
- **1b-2 — per-VU driver loop.** `run_iteration` becomes the coroutine's own
  event loop (settle ops → drain jobs → yield). Convert sync-blocking host fns
  (`http.get`/`request`, `sleep`, sync `ws`/`grpc`) to yield; `asyncRequest`
  becomes a sync host fn that mints a promise + registers its future. Remaining
  `block_on` sites removed (http 3, sleep 3, grpc 2, ws 2).
- **1b-3 — executors: spawn-model swap.** `spawn_blocking` → spawn/drive VU
  coroutines on the loop thread(s). *Not* an await-cascade through 87 `.with(`
  sites — the VU body stays sync `Context`, so those stay `.with`. This is the
  scope that **shrank** under B2.
- **Cutover checklist (carried forward):**
  - **`asyncRequest` parity (silent-divergence trap):** re-apply `__wrap_response`
    (`.json()`/`.html()`/`.cookies`) **and** the per-VU cookie jar — the old stub
    got both free via `__http.request`. Parity bar:
    `http_async_request_resolves_response` asserts `res.json().ok`. (`TODO(cutover)`
    pinned at `register_async_request`.)
  - **Remove superseded scaffolding:** `AsyncContext`/`create_async_*`/
    `spawn_driver` + the `async-spike`/`b2-spike` modules & features. Keep the
    `build_http_request`/`finish_http_response` helpers + the sync-prep discipline.
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

## ws/grpc streaming on the coroutine VU — design note (2026-07-10, review-before-build)

ws/grpc do NOT fit the request/response yield (`AwaitOne`/`AwaitAll`). Forcing
them in would be wrong. But — key finding — they need **no new `Yield`
vocabulary**; the streaming is a **JS-level nested loop**, and each event is a
*repeatable* one-op yield.

**Current sync model (`ws.rs`):** `__ws_open(url)` block_on's the handshake and
spawns a background connection task holding `cmd_tx` (JS→socket) + `evt_rx`
(socket→JS). The `ws.connect(url, fn)` JS shim runs a **recv loop**:
`fn(socket)`, then `while(open){ evt = __ws_recv(id, timeout); dispatch evt to
socket.on(...) handlers }` until Close. `__ws_recv` block_on's `evt_rx.recv()`.

**Coroutine mapping:**
- **The nested driver loop is the JS recv loop** (`ws.connect`'s shim), running
  *inside* the coroutine's iteration, under its `ctx.with` borrow. It is NOT a
  second Rust driver loop.
- `__ws_open` → `AwaitOne(HostOp::WsConnect(url))` → scheduler does the handshake
  + spawns the connection task → resumes with a session handle, stored in a
  **per-VU Rust session registry** (extend `Shared`: `HashMap<id,{cmd_tx,evt_rx}>`).
- `__ws_recv` → `AwaitOne(HostOp::WsRecv(id))` → scheduler awaits that session's
  `evt_rx` (with timeout) → resumes with the event. **Repeatable** — the recv
  loop calls it each turn. So ws reuses `AwaitOne`; only the op vocabulary grows
  (`HostOp::WsConnect`/`WsRecv`, `OpDone::WsSession`/`WsEvent`).
- `__ws_send`/`ping`/`close` → **non-yielding**: push a `WsCommand` onto the
  session's `cmd_tx` (Rust channel), return.

**I2 holds, new event source:** socket events queue in `evt_rx` (Rust channel),
dispatched into JS **only** by the coroutine's recv loop under its own borrow.
`drive_vu` only *awaits* the channel and resumes — it never touches a `Context`
(I1). `run_op`'s access to the session registry is to channels, not a `Context`,
so I1 is preserved.

**Re-entrancy — the reviewer's question:** the recv loop is **sequential** (recv
→ dispatch one handler to completion → recv), so `socket.on` handlers never
overlap. A handler MAY itself yield (e.g. `http.get` inside `on('message')`) —
that's a **nested** `AwaitOne` on the coroutine stack; the recv-loop frame is
preserved across it and resumes cleanly. No re-entrant `with`.

**grpc-streaming follows the same mold:** open stream → repeatable "await next
message" (`AwaitOne(GrpcRecv)`), same session-registry + recv-loop shape.

**Open decisions for the build:** (a) the connection task — `spawn_local` on the
loop thread (its channels are `Send`, but keeping it local avoids cross-thread
wakeups and fits I3); (b) recv timeout semantics (map tokio `timeout` →
`WsEvent::Timeout` as today); (c) how the session registry threads through
`Shared` vs a sibling `Rc` (leaning: a `ws` field on `VuShared`, since it's per-VU
and the scheduler + coroutine both reach it same-thread); (d) whether `HostOp`
grows ws/grpc variants or gains a generic `Boxed(future)` escape hatch to avoid
`vu_sched` learning every protocol — the variant approach is simpler now, the
generic one keeps `vu_sched` protocol-agnostic (decide at build).

## #5 executor cutover — design note (2026-07-10, review-before-build)

The last big structural piece: drive coroutine VUs from the executors on
pool-of-loops threads. Two things to nail — the spawn model and the cancellation
shape.

### The mismatch

Current executors (`ramping_arrival_rate.rs` etc.) hold `Arc<VuPool<QuickJsVu>>`
and, per arrival, `pool.try_acquire_owned()` → `spawn_blocking(|| guard.vu_mut()
.run_iteration())` — one sync iteration per blocking task, VU returned to pool.
Under the coroutine model this is illegal: the VU body must run on a `LocalSet`
(`spawn_local`), never `spawn_blocking` (its yielding host fns can't `block_on`).
And the VU is a **long-lived coroutine** (bootstrap once), not a re-acquired
per-iteration object.

### Spawn model — proposed

- **N loop threads** (N ≈ cores): each `std::thread` running a current-thread
  tokio runtime + `LocalSet`. VUs are **sharded across loops** at startup and
  pinned (`!Send` Ctx). Each VU is one long-lived `drive_vu` coroutine
  `spawn_local`'d on its loop, parked at `IterationBoundary` between iterations.
- **The `control` hook goes async.** Today it's `FnMut(u32)->bool` (a static
  count). For arrival-rate a parked VU must *wait* for an arrival — an async wait,
  not a sync predicate. So `drive_vu` at `IterationBoundary` does
  `match next_action().await { RunNext, Stop }`, where `next_action` is fed by the
  executor's scheduler (a per-VU channel or a shared arrival source).
- **Arrival-curve + dropped-iteration accounting stays in the scheduler, intact.**
  The executor keeps its expected-arrivals integral (the ~8%-bias fix). The
  mapping: maintain an **idle-VU count** (++ when a VU parks at
  `IterationBoundary`, -- when it starts an iteration). Per arrival: if idle > 0,
  signal one idle VU `RunNext`; else `record_dropped()` — exactly the current
  `try_acquire_owned() → None` semantics, just expressed over parked coroutines.
- **The 6 executors map by their `next_action` source:** arrival-rate (permit per
  arrival to an idle VU, drop if none); constant/ramping-vus (each VU loops
  continuously until the deadline — `next_action` = RunNext until Stop);
  per-vu/shared-iterations (a shared remaining-iterations counter — RunNext while
  count-- > 0, else Stop).
- **`IterationOutcome` → executor:** each iteration returns `Completed{value}` |
  `Errored{message}`; the executor counts completed vs failed, drives thresholds,
  and logs `Errored` (folding in the coroutine_vu init-`eprintln!`).
- **`VirtualUser` trait:** likely dissolves for the coroutine path — the executor
  spawns `drive_vu` + a `next_action` source, not a sync `run_iteration()`. The
  arrival-curve logic (no Context, no VU body) can live on the scheduler thread
  or a coordinator; only the VU bodies are loop-pinned.

### Cancellation shape — two-tier (force_unwind is the exception)

- **Tier 1 — graceful, at `IterationBoundary` (preferred, no `force_unwind`).**
  Duration elapses / cancel fires → the scheduler feeds `Stop` at each VU's next
  `IterationBoundary`. VUs finish their current iteration and stop between
  iterations — the clean cancel point (no borrow held, no I/O in flight). This is
  the common path.
- **Tier 2 — hard deadline, `force_unwind` mid-op (rare).** Only if a VU is still
  mid-iteration after a grace period. Gates: (a) the injected unwind runs the
  `ctx.with` guard's `Drop` (releasing the borrow) and drops Context→Runtime in
  order; (b) rquickjs `with` doesn't `catch_unwind`-swallow the injected unwind;
  (c) the cancelled iteration is accounted **exactly once** (not both
  completed-and-dropped). **ws specific:** a `force_unwind` mid-`__ws_recv` leaves
  the read task parked in `read.next().await` holding the socket — store the two
  `spawn_local` `JoinHandle`s in `WsSession` and `abort()` them in `__ws_cleanup`
  (and on unwind), don't rely on drop-propagation. At 7900 VUs × per-iteration
  connect/close over 8h a leaked reader is a fixed-memory regression.

### Before the soak

Tune `COROUTINE_STACK_SIZE` (512 KiB now) by measuring the deepest native frame
under the OOM-reference script's **fat-frame** case (a JS frame near the 256 KB
JS limit doing native-heavy work — deep `JSON.parse`/regex/host-fn chains), NOT
the thin-recursion guard. Confirm rquickjs captures `stack_top` on the coroutine
stack.

### Open decisions for review
- (i) **`next_action` transport:** per-VU `mpsc` (executor addresses a specific
  idle VU) vs a shared MPMC arrival source (idle VUs compete). Per-VU is simpler
  to reason about for drop accounting; shared is less bookkeeping. Leaning per-VU.
- (ii) **Sharding:** static round-robin at startup vs work-stealing. Static keeps
  `!Send` pinning trivial and matches the fixed-memory model; leaning static.
- (iii) **Where the arrival scheduler runs:** on a loop thread (as a `spawn_local`
  task) vs a dedicated coordinator thread. It touches no Context, so either works;
  a coordinator keeps the loop threads purely VU-bodies.

### #5 design — review resolution (sharpened, approved)

- **Idle set is SINGLE-WRITER, coordinator-owned — not a shared atomic counter.**
  Protocol: VU at `IterationBoundary` → send `{my_id}` on a shared idle-signal
  channel, then await its per-VU `RunNext` channel. The **coordinator is the sole
  owner** of the idle queue (adds on idle-signal, removes on dispatch). Per
  arrival: pop an idle id → `RunNext` to that VU; queue empty → `record_dropped()`.
  No atomic, no TOCTOU. **Drop-accounting is provably exact: drop iff the idle
  queue is empty at the arrival instant = the old `try_acquire_owned() → None`.**
  This is the highest-leverage correctness point of the cutover.
- **Coordinator is REQUIRED (not a preference):** a single coordinator has the
  GLOBAL idle view; the global view is what makes the arrival curve correct. A
  per-loop scheduler only sees its shard → per-shard drops → arrival-curve bias
  (reintroduces the ~8% the integral fix killed). Cost = one cross-thread wakeup
  per iteration-START (per-iteration, not per-I/O — I/O stays local; acceptable).
- **Sharding static because VUs are `!Send`** (work-stealing is impossible — can't
  migrate a Ctx). Load-balancing happens at DISPATCH: the coordinator hands each
  arrival to any globally-idle VU regardless of shard, so a hot loop's VUs stay
  busy and stop receiving work — self-balancing without moving anything. FIFO idle
  queue; round-robin pin at startup.
- **Three separate lanes — do NOT route `IterationOutcome` through the
  coordinator.** Coordinator owns ONLY arrivals + drops. Each VU records
  completed/failed to the (Send+Sync) metrics registry LOCALLY on its loop thread;
  the threshold engine reads the registry. Funneling outcomes through the
  coordinator makes it a serial bottleneck for no reason.
- **Preserve the integral fix by REUSE, not re-derivation.** The coordinator
  drives arrivals off the SAME curve-integration code the sync executor uses; only
  the per-arrival ACTION changes (signal-idle-VU vs acquire-permit). Test: arrival
  instants identical to the sync path on a fixed curve.
- **Cancellation — a FOURTH gate: interrupted ≠ errored.** A force-unwound
  iteration never reaches the `IterationOutcome` publish; it must be accounted as
  **interrupted** (a shutdown artifact), NOT script `Errored` (a real failure that
  counts toward error thresholds). Else a passing run spuriously trips an
  error-rate threshold in its final second. k6 counts interrupted iterations
  separately — match that. Graceful-stop deadline maps to k6's
  `gracefulStop`/`gracefulRampDown` (default 30s): `Stop` at boundaries until it
  expires, `force_unwind` only VUs still mid-iteration past it. Gate (a)'s concrete
  mechanism = the ws `JoinHandle::abort()` in `__ws_cleanup`; wire ws-abort + the
  force_unwind gate test together.
- **#5 resolves #7:** with the executor spawning `drive_vu` + a `next_action`
  source, there is no `run_iteration()` and thus no `VirtualUser` trait on the
  coroutine path. Do NOT design a thin sync trait — let the coroutine spawn model
  make it dead and delete it at #6. Consider #5+#7 together.

**Build in SLICES (each a review boundary):** (1) spawn model + ONE executor
green; (2) remaining executors; (3) cancellation (two-tier + 4 gates + ws-abort);
(4) fat-frame stack measurement + 7900 soak.

### #5 slice 1 — review findings (carried forward)

- **[soak-critical, → cancellation/soak slice] Loop-thread panic accounting.**
  `pool.rs` `let _ = h.join()` swallows loop-thread panics; the loop thread carries
  live `.expect()`s (`create_runtime`/`create_context`/`bootstrap_api`). One VU
  panic kills the whole loop thread → all its sharded VUs stop → `RunSummary`
  silently undercounts and reports GREEN. Realistic trigger: `Context` alloc
  failing under memory pressure at 7900 VUs — the exact soak condition. Blast
  radius ≈ `num_vus/cores` (hundreds), invisible. Min fix: check `join()` →
  failed/degraded run + log, never silent undercount. Better: per-VU fault
  isolation. **Gates soak-result trustworthiness.** (task #9)
- **[test gap — CLOSED this slice] Count-only-`Completed` untested at pool level.**
  Added `errored_iterations_are_not_counted_but_their_requests_are`: `http.get`
  then throw every 3rd iter ⇒ `http_reqs` (all attempts) > `iterations_completed`
  (completed only) ⇒ the discriminating branch is exercised. Slice 2 adds the
  arrival-rate completed/dropped divergence test.
- **[conformance, → separate] Failed-iteration metrics vs upstream.**
  Skipping `iteration_duration` + `iterations` on a throw is now cemented on BOTH
  internal paths but UNVERIFIED against upstream k6 (which likely still emits both
  and surfaces the error separately). Two paths agreeing ≠ correct. Route through
  the conformance harness (sometimes-throwing default fn) as a field-level
  known_drift candidate. (task #10)

### #5 slice 2 — design note: arrival-rate coordinator (the teeth)

**Async control seam — a port, not a closure signature.** `drive_vu`'s control
becomes a trait `IterationControl { async fn next(&mut self, completed: u32) ->
bool }` (native async-fn-in-trait; the returned future may borrow `&mut self`
across `.await`, which a `FnMut(u32) -> impl Future` signature cannot express).
A **blanket impl for `FnMut(u32) -> bool`** keeps constant-vus + every existing
test caller unchanged (a ready future). The coordinator path gets a struct
`ArrivalControl` whose `next` awaits its per-VU RunNext channel. Two real adapters
+ a test seam ⇒ the abstraction earns its keep (not a one-impl trait).

**Curve reuse, not re-derivation.** Extract the arrival integral into a JS-free
`k6_core::executor::arrival::ArrivalCurve` (timeline + `expected_arrivals` +
`interpolate_rate`), and refactor the sync `RampingArrivalRateExecutor` to consume
it (its tests stay green ⇒ reuse proven, zero behavior change). The coroutine
coordinator consumes the SAME `ArrivalCurve`, so the k-th arrival's scheduled
position (the integral-crossing point) is identical by construction — arrival-
instant parity is structural, not re-implemented. Constant-arrival-rate is just
`ArrivalCurve::constant` (one flat stage), so ONE coordinator serves both.

**The coordinator (one, global — single-writer idle set).**
- Channels: a shared `idle_tx` (tokio unbounded; every VU clones it) → coordinator's
  `idle_rx`; per-VU `run_next` (tokio unbounded, unit message = RunNext, channel
  close = Stop) — coordinator holds the `Vec<Sender>` by id, each VU its own rx.
- `ArrivalControl::next`: `(if n>=1 tally the just-finished iteration); idle_tx.send(my_id);
  run_next_rx.recv().await.is_some()`. The VU is in the idle set **iff** parked here
  = available; dispatched (RunNext) ⇒ removed = busy; re-enters only on the next
  park. The VU only ever SENDS its id — it never touches the queue.
- Coordinator loop (its own thread; sync + `blocking_recv`): FIRST collect all N
  startup idles so the "pool full at t=0" equivalence holds (no startup skew), THEN
  start the clock. Each pass: drain `idle_rx.try_recv()` into the idle `VecDeque`
  (sole writer); `target = curve.expected_arrivals(elapsed)`; catch-up
  `while dispatched+1 <= target { dispatched+=1; match idle.pop_front() { Some(id)
  => run_next[id].send(()), None => dropped+=1 } }`; sleep to ≈ next arrival; break
  at `total_duration`/cancel; on exit drop the `Vec<Sender>` ⇒ every VU's recv
  returns None ⇒ graceful Stop at its next boundary.
- **Drop-accounting equivalence:** the catch-up loop is structurally identical to
  the sync executor's `while … { try_acquire → run | record_dropped }` —
  `idle.pop_front()==None` ⟺ `try_acquire_owned()==None` ⟺ pool exhausted ⟺ drop,
  at the SAME integral instants. That correspondence IS the correctness proof.
- Join order: coordinator thread (returns having dropped senders) → loop threads
  (VUs see None, finish current iteration, stop). No force-unwind yet (slice 3).

**Tests (the review bars):** (a) drop-accounting — slow VUs ⇒ drops, fast VUs +
ample pool ⇒ zero drops; (b) integral parity — fast VUs, completed lands on the
curve integral, mirroring the sync `ramp_matches_integral_count`; (c) completed vs
dropped divergence (finding #2 at arrival-rate — the three counts pull apart);
(d) ArrivalCurve extraction regression-lock (identical `expected_arrivals` values).

### #5 slice 3 — design note: cancellation + the accounting buckets finalize

Slice-2 review sharpened three things; folded in here.

**Conservation is four-way, not two.** A dispatched iteration that THROWS consumed
an arrival slot but is invisible today (not completed, not dropped). That hides the
load-test-critical distinction between "couldn't keep up" (dropped = capacity) and
"kept up but erroring under load" (errored = app health). Add BOTH lanes so:
`completed + dropped + errored + interrupted == integral`. `errored` +
`interrupted` are the same accounting surface, so they finalize together here.
- **errored** — counted in the control port's `Errored` branch (both constant-vus
  and arrival paths), same place `completed` is tallied.
- **interrupted** — a force-unwound iteration never reaches the boundary/publish,
  so it is counted where the unwind happens (drive_vu's hard-cancel arm), and
  attributed as **interrupted, NOT errored** — else a shutdown trips an error
  threshold in the run's final second. Matches k6's separate interrupted count.

**Claim precision (slice-2 finding 2).** The coordinator↔sync correspondence is
**conservation-identical, split-approximate**: the sync semaphore returns a permit
synchronously, while the coordinator learns idle one `try_recv`-drain quantum late
(≤ one sleep, typ. ≤10 ms), so the completed/dropped SPLIT can diverge by a
marginal-VU lag under saturation. Negligible at 7900 VUs (genuine exhaustion
dominates); the tests lock conservation, not the split, which is why they're robust.
Do NOT carry "structurally identical" into soak analysis.

**Startup is a deadlock, not an undercount (slice-2 finding 3).** In the arrival
path a VU that dies before its first idle report (e.g. `DefaultStack::new().expect()`
OOM at 7900 VUs — the soak condition) leaves the coordinator's
`for _ in 0..num_vus { blocking_recv }` waiting forever → senders never dropped →
every survivor parks forever = full-run deadlock. Fix: make startup cancel- AND
timeout-aware — poll `try_recv` with a bounded deadline, and on cancel/timeout
**proceed-degraded** with the VUs that did report (logged), or return empty if none.

**force_unwind is a DISCOVERED RISK — spike before relying on it.** corosensei
`force_unwind` unwinds the coroutine from its `suspend` point via a panic. Our
coroutines suspend INSIDE a native http fn called from QuickJS's C interpreter, so
the unwind drives a Rust panic THROUGH QuickJS C frames. `unwind` is a default
corosensei feature and no profile sets `panic=abort`, so it's available; on Linux
x86-64 CFI unwind tables usually let a Rust panic pass through cleanup-free C
frames, but this is platform-fragile and has NEVER been exercised here. So:

- **3a (safe, this pass):** errored lane + three-way conservation
  (`completed+dropped+errored==integral`, `interrupted==0` — graceful stop loses no
  dispatched iteration) + cancel/timeout-aware startup. Closes findings 1 & 3.
- **3b (spike-gated):** a focused test — force_unwind a coroutine parked mid
  `http.get` (C frames on stack) and mid `sleep`; confirm clean teardown (Context
  drop, no abort) on the soak platform. If SAFE → hard-deadline tier + interrupted
  lane + ws `JoinHandle::abort` + four-way conservation. If UNSAFE → redesign: the
  hard deadline is END-OF-RUN, so abandon still-in-flight VUs (count interrupted,
  stop joining, summary, process-exit reclaims) rather than unwind through C.

### #5 slice 3b — force_unwind spike RESULT (2026-07-10): SAFE on soak platform

Ran two `#[ignore]` spikes (`coroutine_vu` tests): resume a coroutine until it
parks mid `http.get` / mid `sleep` (QuickJS C frames live on the coroutine stack),
then `coro.force_unwind()`. BOTH reach `coro.done()` cleanly — the Rust unwind
panic passes through the cleanup-free QuickJS C frames and drops the Context
without aborting. Holds in **debug and release**, stable across repeated runs, on
Linux x86-64 (the soak platform). Conclusion: the hard-cancellation tier CAN use
`force_unwind`; no redesign needed. Kept as regression locks (`#[ignore]`, run
explicitly) — flip to non-ignored only if we ever gain a non-x86-64/Windows target
where SEH/DWARF differences could reintroduce the risk.

### #5 slice 3b — hard-cancellation tier: BUILT (force_unwind validated safe)

- `HardStop { token, interrupted }` + `spawn_vu_hard`/`drive_vu`: a hard-cancel
  arm on each op-select (AwaitOne/AwaitAll/AwaitPending). When it fires (the
  coroutine is suspended at a yield ⇒ `force_unwind` is safe), the VU is unwound
  and its in-flight iteration counted **interrupted, not errored**. `spawn_vu`
  stays a graceful-only forwarder (`HardStop::never()`), so the 10 test/vu_loop
  callers are unchanged.
- Executors: a `graceful_stop` watchdog arms the hard token that long after
  graceful stop begins (arrival: coordinator returns; constant-vus: deadline or
  cancel), so a VU stuck mid-op past the deadline is force_unwound and the join
  can't hang. `interrupted` flows to `RunSummary`.
- ws-abort gate: `WsSession` now owns the read/write `JoinHandle`s and aborts them
  on `Drop`, so a force_unwind (Context → registry → session drop) can't orphan a
  read task parked on `read.next()`. No-op on the graceful-close / `__ws_cleanup`
  paths.
- Tests: four-way conservation with hung I/O (`completed+dropped+errored+
  interrupted==integral`, interrupted the hung VUs, dropped the rest); constant-vus
  hung VU force_unwound not a deadlock; ws-blocked VU force_unwound cleanly through
  the production executor (server thread joins ⇒ client torn down). All stable.
- **interrupted ≠ errored is locked**: the hung-client tests assert
  `errored==0, interrupted==N` — a graceful/hard shutdown cannot inflate the error
  count or trip an error threshold in the run's final second.

Slice 3 COMPLETE. Remaining before soak: remaining VU-based executors
(ramping-vus / per-vu / shared-iterations / externally-controlled) on the
coroutine model, the `main.rs` cutover, fat-frame stack measurement, 7900 soak.
Task #9 (general loop-thread-panic undercount, distinct from the startup deadlock
closed in 3a) still open for the soak slice.

### #5 slice 3b — review findings (scope corrections + soak gates)

- **CLAIM SCOPE (finding 1): the hard tier reclaims I/O-hung VUs, NOT CPU-hung.**
  `force_unwind` needs a suspend point; a CPU-bound iteration (`while(true){}`, long
  compute with no `await`/`http`/`sleep`) never yields, so `drive_vu` is blocked
  inside `coro.resume()`, never reaches a select, never sees the hard token —
  `token.cancel()` fires into the void and the loop-thread join hangs forever
  (wedging that thread + all VUs sharded onto it). Neither tier can recover it:
  corosensei can't preempt a running coroutine and the graceful hook is only
  consulted at a boundary the spin never reaches. So "a hung VU can't hang the
  join" holds **only for I/O-hung VUs**. Complete fix = QuickJS
  `Ctx::set_interrupt_handler` at JS back-edges → throw on the hard token (k6/goja's
  approach). **SOAK-CRITICAL, task #11 — resolve or document as an explicit
  limitation before the 7900 soak.**
- **SPIKE RE-VERIFICATION (finding 2):** the `force_unwind_*` spikes are `#[ignore]`
  ⇒ CI never runs them ⇒ a corosensei/rquickjs/toolchain bump that regresses
  unwind-through-C stays green and surfaces as a soak abort at the first hard-stop.
  **Pre-soak gate, task #12** — isolated subprocess CI job (`-- --ignored`
  fail-loud) or a re-run-on-bump checklist.
- **Acknowledged (no block):** grpc force_unwind-mid-invoke untested but lower risk
  (unary, no background tasks to orphan; Context drop closes the tonic channel).
  The ws-abort test proves clean teardown but doesn't isolate abort-wired-vs-not
  (end-of-run runtime drop closes the socket anyway); the true lock would
  force_unwind a ws VU MID-run with other VUs live and assert the read task is gone
  while the runtime survives — low priority, the `Drop` is obviously correct.

### #5 slice 4 — remaining executors on the coroutine model

- **4a — per-vu-iterations + shared-iterations** via a shared `run_vus_on_loops`
  skeleton (extracted on the third VU-count executor). Policy lives in the control:
  per-vu caps at `n < quota`; shared CAS-claims a shared `AtomicU32` budget. Both
  reuse the HardStop watchdog + errored/interrupted lanes. dropped = never-started
  iterations; four-lane conservation locked.
- **4b — ramping-vus.** Active-count schedule extracted to `k6_core::executor::
  vu_ramp::VuRampSchedule` (round-not-truncate; sync executor refactored onto it,
  reuse proven). `RampingControl`: a VU is active iff `my_index < desired`; a
  controller thread drives `desired` along the schedule then fires the graceful
  stop. All `max_vus` coroutines pre-allocated (fixed memory); only `desired` run
  at once, deactivating highest indices first (matches sync scale-down). The
  index-aware control needed `run_vus_on_loops` to pass the global VU id to the
  control factory (others ignore it).

Coverage: constant-vus, constant/ramping-arrival-rate, per-vu, shared, ramping-vus
all on the coroutine model. **externally-controlled** (runtime REST-API VU control)
deferred — not used by the OOM-reference soak; slot it during the main.rs cutover
or stub it. NEXT: main.rs cutover (wire pool.rs, retire the sync spawn_blocking
path) + fat-frame stack measurement + 7900 soak. Pre-soak gates: tasks #9, #11, #12.

### #5 slice 4 — review findings (pre-soak sequence)

- **F1 (teeth, task #13):** `RampingControl` polls idle VUs on a 20 ms sleep —
  O(idle × time) wakeups, ~400k/s at 7900-VU ramp, ~3–5% CPU/loop-thread that
  competes with load gen and skews latency. Convert to a `watch<u32>` broadcast of
  `desired` (wake only on ramp change). Pre-soak, after the cutover.
- **F2 (review-only, acknowledged):** the ramping tests lock schedule-following +
  prompt-cancel, but the active-count SHAPE (only `desired` run at peak, highest
  indices deactivate first) is review-verified, not test-verified (`completed>0`
  proves work, not the curve). To lock: a script recording `__VU` into a
  timestamped shared set → assert max-concurrent-distinct ≈ peak + high-index-stops
  -first. Low priority.
- **F3 (cutover):** externally-controlled → a LOUD error stub ("unsupported on the
  async runtime"), never a silent no-op, so a config using it fails visibly.

**Pre-soak sequence (agreed):** cutover (with the F3 loud stub) → F1 poll→watch
(#13) → #11 CPU-interrupt handler → #12 spike re-verify → #9 loop-thread-panic
undercount → fat-frame stack measurement → 7900 soak.

### #5 slice 5 — main.rs cutover: COROUTINE RUNTIME IS LIVE

- **5a** — coroutine VU feature parity via `VuSpec { script, script_dir, env,
  setup_data, exec_fn }`: bootstrap sets __VU/__ITER/__ENV/__k6_setup_data and
  resolves the exec fn once (__k6_exec); the raw script is prepared WITH script_dir
  (local imports). Threaded through the pool as `impl Into<VuSpec>` ⇒ zero test
  churn. Console stays a stub (observability → #6).
- **5b** — main.rs runs every scenario through `k6_js::pool::run_*` on
  spawn_blocking (the pool owns loop threads). Four-lane summary in the CLI
  (errored + interrupted + dropped); externally-controlled fails LOUD (F3). Sync
  create_vus removed; obsolete 512-ceiling test retired. setup()/teardown() still
  on the one-off sync QuickJsVu (retire at #6).
- Smoke-validated end-to-end: __ENV, setup() data, arrival-rate four-lane
  conservation (33 completed + 17 errored = 50 arrivals, shown in CLI), loud stub.
  Full workspace green INCLUDING the conformance suite (diffs vs upstream k6) —
  upstream parity holds on the coroutine runtime.

**Executor cutover (task #5) COMPLETE.** Remaining before the 7900 soak (all
tracked): F1 poll→watch (#13) → #11 CPU-interrupt handler → #12 spike re-verify →
#9 loop-thread-panic undercount → fat-frame stack measurement → soak. Then #6
(delete the sync path: QuickJsVu/sync executors/vu_pool/setup-teardown-on-sync,
+ console on the coroutine path).

### #5 cutover review — resequenced pre-soak plan (slice-5 review)

Live cutover confirmed GO (diff-read): cancellation survives the spawn_blocking
swap (concurrent signal handler → cancel token → pool observes it → blocking task
returns); four-lane summary stays on the right side of the conformance line
(errored/interrupted = eprintln only, dropped emits the metric); conformance
green on the coroutine runtime. Non-blocking notes: scenarios run sequentially
(PRE-EXISTING — sync path did the same inline await; conformance-tracker item, not
a cutover regression); second Ctrl-C `process::exit(130)` hard-kills, bypassing
teardown + summary flush (intended k6 "force" semantics).

**RESEQUENCED** (staff-lens: #11 is the soak-BLOCKER, #13 is fidelity/cuttable;
fat-frame is the fixed-memory premise and could invalidate the budget → measure
early, not last):
  #13 poll→watch (#13) + fat-frame stack measurement (#14) — in parallel, next
  → #11 CPU-bound interrupt handler (soak-blocker, NON-cuttable)
  → #12 spike re-verify → #9 loop-thread-panic → 7900 soak
  → #6 delete sync path + console on coroutine path

### Pre-soak progress (2026-07-10)

- **#13 DONE ✅ (8073a6f) — ramping poll→watch<u32> (F1).** Inactive VUs park on
  `desired_rx.changed()` (select'd vs the stop token), waking ONLY on a ramp change
  — zero idle wakeups (was ~400k timer-fires/s at 7900-VU ramp). Controller
  broadcasts only on an actual change; `borrow_and_update` prevents missed/spurious
  wakes. Stable ×5 + CLI smoke (0→6 VUs).
- **#14 DONE ✅ (050fe20) — fat-frame stack measurement: 512KB budget VALIDATED.**
  Worst-case C-stack high-water = **~252 KB** (deep recursion + `crypto.sha256`
  every frame), right at VU_MAX_STACK — QuickJS's anchored check caps JS+native
  recursion at that budget regardless of frame fatness (heavy I/O runs on the
  scheduler, off-stack, I1). **260 KB headroom (>2×)** under the 512KB allocation
  ⇒ 512KB × 7900 ≈ 4GB stacks, the designed budget, holds. Coupling locked by
  `fat_frame_recursion_traps_rangeerror_before_native_overflow`; number by the
  `#[ignore]` `measure_fat_frame_c_stack_highwater`.

**NEXT = #11 (CPU-bound interrupt handler)** — the soak-BLOCKER. Then #12 spike
re-verify → #9 loop-thread-panic → 7900 soak → #6 delete sync path.

### #14 measurement — caveats (keep honest)

- **Probe under-counts by one native leaf frame:** `__sp()` reads at `rec()` ENTRY,
  before `crypto.sha256` runs, so the ~252KB excludes the sha256 Rust frame at the
  deepest level (the frame closest to the guard). True peak = 252KB + one host-fn
  frame (a few KB). Immaterial vs 260KB headroom — but the HEADROOM is the safety,
  not the exact 252. Do not quote 252 as "the ceiling."
- **INVARIANT the 512KB budget rests on:** NO synchronous host fn may recurse
  unboundedly in native code. `sha256` is native-heavy but iterative (shallow-
  framed); the budget holds because I1 keeps heavy work (async I/O) on the
  SCHEDULER, off the coroutine stack. A sync host fn that recursed deeply in native
  C would consume coroutine C-stack BELOW QuickJS's anchor, uncaught by this
  measurement. We have none today (and QuickJS guards its own JSON.parse/regex
  recursion). If one is ever added: RE-MEASURE. State it now, don't rediscover at
  7900×.

- **#11 DONE ✅ (53468d2) — CPU-bound interrupt handler (soak-BLOCKER closed).**
  `build_coroutine_vu_spec` installs rquickjs `set_interrupt_handler` (checked at JS
  back-edges) wired to the hard-stop token → throws to unwind a runaway
  `while(true){}` that force_unwind can't reach. Fired by the watchdog on its own OS
  thread (a wedged loop thread can't block the trigger). NEW
  `IterationOutcome::Interrupted`: a hard-stop-cut iteration classifies interrupted,
  NOT errored (the driver loop bails vs re-interrupt spin), routed to the SAME
  interrupted counter as force_unwind — a shutdown throw can't trip an error
  threshold. SCOPE: JS loops only; native-C hangs (ReDoS regex, hung sync host fn)
  never return to the interpreter, so neither tier breaks them → they fall to the
  process-level double-Ctrl-C exit(130) backstop. CLI smoke: runaway while(true) →
  0 iters, "interrupted: 2", exit 0. KNOWN: a spinning VU still wedges its
  loop-thread-mates UNTIL the hard deadline (single-thread-per-loop); the interrupt
  bounds that to graceful_stop.

**NEXT = #12 (spike re-verify CI gate) → #9 (loop-thread-panic undercount) → 7900
soak → #6 (delete sync path).**

### SOAK-WATCH: cooperative-scheduling tax (loop-mate starvation) — design characteristic, not a fix

Sharpened from #11's "known": a spinning VU wedging its loop-mates is NOT limited
to `while(true)`. ANY heavy SYNCHRONOUS per-iteration compute — a large
`JSON.parse`, a crypto/hash call, a big string/regex op — blocks EVERY co-located
VU on that loop thread for its full duration, because `drive_vu` is stuck inside
`coro.resume()` and can't service them. This is the flip side of the fidelity win
#13 bought (cooperative single-thread-per-loop). At 7900 VUs over ~cores loops,
each loop hosts ~500–1000 VUs, so one 50ms heavy iteration stalls ~1000 VUs'
progress by 50ms → **latency-tail inflation in the reported numbers**.

No cheap fix (sync JS can't yield without a suspend point — the reason #11 exists;
more loop threads = more stacks = defeats the memory budget). So: make it
OBSERVABLE, not eliminated.
1. **Pre-soak:** check whether the OOM-reference script has heavy synchronous
   per-iteration compute (large-body parse, crypto, hashing). Flat http-loop ⇒
   never bites.
2. **During soak:** watch for latency-tail spikes that CORRELATE across VUs on the
   SAME loop thread (vs. tracking backend behavior) — the signature of loop-mate
   starvation vs. a real server-side tail. A per-loop-thread iteration-latency
   histogram is worth adding if instrumenting the soak anyway.
Soak-observability watch item, tracked here — not a #12 blocker.
