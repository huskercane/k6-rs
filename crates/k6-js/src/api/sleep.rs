use std::time::Duration;

use anyhow::Result;
use rquickjs::{Ctx, Function};

/// Register the k6 `sleep(seconds)` function.
///
/// Bridges to `tokio::time::sleep` via `Handle::block_on`.
/// This blocks the current thread (spawn_blocking) but yields the
/// tokio runtime so other tasks can proceed.
pub fn register(ctx: &Ctx<'_>, handle: tokio::runtime::Handle) -> Result<()> {
    let globals = ctx.globals();

    globals.set(
        "sleep",
        Function::new(ctx.clone(), move |seconds: f64| {
            let dur = Duration::from_secs_f64(seconds);
            handle.block_on(tokio::time::sleep(dur));
        })?,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;

    // Tests run in spawn_blocking to simulate real VU execution context
    // (block_on requires not being on an async thread).

    #[tokio::test]
    async fn sleep_basic() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();

            ctx.with(|ctx| {
                register(&ctx, handle).unwrap();

                let start = std::time::Instant::now();
                ctx.eval::<(), _>("sleep(0.05)").unwrap();
                let elapsed = start.elapsed();

                assert!(
                    elapsed >= Duration::from_millis(40),
                    "sleep was too short: {elapsed:?}"
                );
                assert!(
                    elapsed < Duration::from_millis(200),
                    "sleep was too long: {elapsed:?}"
                );
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sleep_zero() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();

            ctx.with(|ctx| {
                register(&ctx, handle).unwrap();
                ctx.eval::<(), _>("sleep(0)").unwrap();
            });
        })
        .await
        .unwrap();
    }
}
