pub mod api;
/// VU scheduler + yield primitive: `HostOp`/`OpDone`, the coroutine driver
/// (`drive_vu`), and the spawn entry (`spawn_vu_hard`). The async-runtime core.
pub mod vu_sched;
/// The production coroutine VU: long-lived per-VU coroutine, bootstrap-once API,
/// yielding host fns, typed iteration outcomes.
pub mod coroutine_vu;
/// Pool-of-loops spawn model: coroutine VUs sharded across N loop threads, one
/// executor per `ExecutorType`.
pub mod pool;
pub mod http_client;
pub mod hyper_client;
pub mod runtime;
pub mod vu;
