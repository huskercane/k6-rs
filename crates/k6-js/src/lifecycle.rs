//! REST lifecycle calls use isolated QuickJS contexts on blocking workers.
//! No context crosses threads, and calls are serialized by the control surface.
use crate::{
    coroutine_vu::VuSpec,
    vu::{QuickJsVu, prepare_script_with_dir},
};
use k6_core::{
    backpressure::Backpressure,
    control::{HookFuture, Lifecycle},
    metrics::BuiltinMetrics,
    traits::HttpClient,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub struct JsLifecycle<C> {
    pub setup_timeout: Duration,
    pub teardown_timeout: Duration,
    pub spec: VuSpec,
    pub client: Arc<C>,
    pub metrics: BuiltinMetrics,
}
impl<C: HttpClient + 'static> Lifecycle for JsLifecycle<C> {
    fn invoke(&self, setup: bool, data: Option<String>, cancel: CancellationToken) -> HookFuture {
        let spec = self.spec.clone();
        let client = self.client.clone();
        let metrics = self.metrics.clone();
        let handle = tokio::runtime::Handle::current();
        let timeout = if setup {
            self.setup_timeout
        } else {
            self.teardown_timeout
        };
        Box::pin(async move {
            let interrupted = cancel.clone();
            let _cancel_on_drop = cancel.clone().drop_guard();
            let client = Arc::new(CancelClient {
                client,
                cancel: cancel.clone(),
            });
            let mut task = tokio::task::spawn_blocking(move || {
                let script = prepare_script_with_dir(&spec.script, spec.script_dir.as_deref());
                let mut vu = QuickJsVu::new_full_with_control(
                    0,
                    &script,
                    &spec.env,
                    handle,
                    client,
                    Backpressure::new(1),
                    Some(metrics),
                    spec.script_dir,
                    spec.console,
                    Some(cancel),
                )?;
                if setup {
                    vu.run_setup()
                } else {
                    if let Some(data) = data {
                        vu.set_setup_data(&data)?;
                    }
                    vu.run_teardown()?;
                    Ok(None)
                }
            });
            tokio::select! {
                result = &mut task => result?,
                _ = tokio::time::sleep(timeout) => {
                    interrupted.cancel();
                    let _ = task.await;
                    anyhow::bail!("{}() execution timed out after {:?}",if setup {"setup"} else {"teardown"},timeout)
                }
            }
        })
    }
}

struct CancelClient<C> {
    client: Arc<C>,
    cancel: CancellationToken,
}
impl<C: HttpClient> HttpClient for CancelClient<C> {
    async fn send(
        &self,
        req: k6_core::traits::HttpRequest,
    ) -> anyhow::Result<k6_core::traits::HttpResponse> {
        tokio::select! {
            result = self.client.send(req) => result,
            _ = self.cancel.cancelled() => anyhow::bail!("execution interrupted"),
        }
    }
}
