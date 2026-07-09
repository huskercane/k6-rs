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

## What tokio buys beyond parity

Same semantics as upstream, plus a narrow efficiency edge: tokio's epoll-based
tasks scale to very high in-flight-request counts more cheaply than goroutine
stacks. For a fixed-memory 8-hour soak (the project's north star) that is a real,
if secondary, win. The primary value is faithful async behavior, not raw speed.

## Target Architecture

- `AsyncRuntime` + `AsyncContext` (rquickjs `futures` feature).
- **One VU = one thread with a current-thread tokio runtime + `LocalSet`.** The
  `Ctx` stays `!Send`, so all JS execution and promise resolution happens on the
  VU's own thread; async I/O (HTTP, timers) is spawned onto that thread's local
  set and driven by the current-thread runtime. This preserves k6's
  "single-threaded JS per VU, concurrent I/O within a VU" model exactly.
- Rust `async` host functions map to JS promises via rquickjs; `ctx.spawn()`
  (or `async_with!`) drives them. `asyncRequest` becomes a real async host fn
  that awaits the HTTP client future and resolves the promise on completion.
- Timers re-implemented on `tokio::time` so `setTimeout` yields instead of
  blocking; the event loop advances other work while a timer is pending.

## Blast Radius

- `runtime.rs`: `create_runtime`/`create_context` and `drain_pending_jobs`
  (→ driven by the async executor). 18 files call these or `ctx.with`.
- **Every `ctx.with(|ctx| {...})` call site** (heaviest: `vu.rs` ~27,
  `http.rs` ~27) becomes `async_with!`/`.with().await` or moves inside a spawned
  future. This is the bulk of the mechanical work.
- `vu.rs` `run_iteration`: sync → async; VUs stop being `spawn_blocking` and run
  on their per-VU current-thread runtime.
- All six executors: their `spawn_blocking` VU-driving model changes to async
  task spawning (keep the arrival-curve / dropped-iteration logic intact —
  that is orthogonal and already correct).
- `api/timers.rs`: reimplement on `tokio::time`.
- `api/ws.rs`, `api/grpc.rs`: candidates to become genuinely streaming/async
  afterward (follow-on, not required for the core migration).

## Migration Phases (incremental, each independently testable)

- **Phase 0 — spike.** Stand up an `AsyncContext` alongside the sync one behind a
  cargo feature; prove a single async host fn resolves a JS promise from an
  awaited Rust future. No production wiring.
- **Phase 1 — VU on async.** Move `QuickJsVu::run_iteration` to async on a
  per-VU current-thread runtime + `LocalSet`; keep host APIs sync-shimmed.
  Executors call it via the new async path. Gate: all existing tests pass with
  the VU running on the async loop (behavior identical).
- **Phase 2 — timers non-blocking.** Reimplement `setTimeout`/`setInterval` on
  `tokio::time`. Add a conformance script proving a pending timer does not block
  other event-loop work.
- **Phase 3 — real asyncRequest.** Make `asyncRequest` a true async host fn.
  Add a conformance script where `Promise.all([...])` of N requests overlaps in
  wall-clock (measure: total ≈ max(single) not sum).
- **Phase 4 — cleanup.** Remove the sync runtime + the `asyncRequest` stub;
  delete the feature gate.

## Tests / Conformance To Add

- `asyncRequest` concurrency: `Promise.all` of N requests against a fixture with
  a fixed per-request delay; assert wall-clock ≈ one request, not N.
- Timer non-blocking: `setTimeout(f, 200)` then synchronous work; assert the
  loop progressed rather than sleeping the VU.
- Regression: every existing VU/http/executor test must stay green through
  Phase 1 (the migration must be behavior-preserving for sync code).

## Risks / Open Questions

- **`!Send` `Ctx`** dictates one-thread-per-VU; confirm the fixed-memory VU pool
  model tolerates a current-thread runtime per VU at the target VU counts
  (7900 maxVUs in the reference soak). May need a small pool of executor threads
  each hosting many VUs' local sets rather than a thread per VU.
- rquickjs async executor + our `Backpressure` interplay (ordering, fairness).
- Cancellation: `CancellationToken` must interrupt an awaited request mid-flight
  and still return VUs to the pool cleanly.
- Keep the executor arrival-curve integral and dropped-iteration accounting
  (already correct) untouched by the spawn-model change.
