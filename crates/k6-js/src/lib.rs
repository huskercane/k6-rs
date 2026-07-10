pub mod api;
/// Phase 0 spike for the async-runtime migration. Throwaway; gated on `async-spike`.
#[cfg(feature = "async-spike")]
pub mod async_spike;
/// Phase 0.5 spike: B2 stackful-coroutine suspension. Throwaway; gated on `b2-spike`.
#[cfg(feature = "b2-spike")]
pub mod b2_spike;
/// Phase 1b: VU scheduler + yield primitive (production, un-gated).
pub mod vu_sched;
/// Phase 1b: production coroutine VU (async-runtime graduation, step 3).
pub mod coroutine_vu;
/// #5: pool-of-loops spawn model (coroutine VUs on N loop threads).
pub mod pool;
/// Phase 1b (#2): yield primitive + scheduler + driver loop composition. Gated on `b2-spike`.
#[cfg(feature = "b2-spike")]
pub mod vu_loop;
pub mod http_client;
pub mod hyper_client;
pub mod runtime;
pub mod vu;
