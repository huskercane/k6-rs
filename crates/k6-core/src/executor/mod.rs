//! Executor scheduling primitives. The per-VU spawn model now lives in the
//! coroutine pool (`k6_js::pool`); what remains here is the JS-free, shareable
//! scheduling math both the CLI wiring and the pool consume.

/// The arrival-rate curve integral (constant + ramping arrival rate).
pub mod arrival;
/// The ramping-VUs active-count schedule.
pub mod vu_ramp;
