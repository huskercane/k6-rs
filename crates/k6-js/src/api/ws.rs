use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rquickjs::{Ctx, Function, IntoJs, Object, Value};

use k6_core::metrics::BuiltinMetrics;

/// Register WebSocket low-level functions for the k6/ws JS shim.
///
/// Low-level Rust functions:
/// - `__ws_open(url, timeout_ms)` → session_id (string) or throws
/// - `__ws_send(session_id, data)` → sends text message
/// - `__ws_send_binary(session_id, data_b64)` → sends binary (base64-encoded)
/// - `__ws_ping(session_id)` → sends ping frame
/// - `__ws_close(session_id)` → closes connection
/// - `__ws_recv(session_id, timeout_ms)` → blocks, returns JSON event `{type, data?}`
///
/// The JS shim (`ws_shim.js`) provides the full `ws.connect(url, params, callback)` API
/// with Socket object, event handlers, intervals, and timeouts.
pub fn register(
    ctx: &rquickjs::Ctx<'_>,
    handle: tokio::runtime::Handle,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    let sessions: Arc<Mutex<HashMap<String, WsSession>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // __ws_open(url, timeout_ms) → session_id
    {
        let h = handle.clone();
        let m = metrics.clone();
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_open",
            Function::new(
                ctx.clone(),
                move |url: String, timeout_ms: f64| -> rquickjs::Result<String> {
                    let m = m.clone();
                    let sess = Arc::clone(&sess);
                    let h2 = h.clone();

                    let result = h.block_on(async {
                        ws_open_impl(&url, timeout_ms, m.as_ref(), sess, h2).await
                    });

                    match result {
                        Ok(id) => Ok(id),
                        Err(e) => Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            &e.to_string(),
                        )),
                    }
                },
            )?,
        )?;
    }

    // __ws_send(session_id, data)
    {
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_send",
            Function::new(
                ctx.clone(),
                move |id: String, data: String| -> rquickjs::Result<()> {
                    let sessions = sess.lock().unwrap();
                    if let Some(session) = sessions.get(&id) {
                        let _ = session.cmd_tx.send(WsCommand::Send(data));
                        Ok(())
                    } else {
                        Err(rquickjs::Error::new_from_js_message(
                            "string",
                            "string",
                            "WebSocket session not found",
                        ))
                    }
                },
            )?,
        )?;
    }

    // __ws_ping(session_id)
    {
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_ping",
            Function::new(ctx.clone(), move |id: String| -> rquickjs::Result<()> {
                let sessions = sess.lock().unwrap();
                if let Some(session) = sessions.get(&id) {
                    let _ = session.cmd_tx.send(WsCommand::Ping);
                    Ok(())
                } else {
                    Err(rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        "WebSocket session not found",
                    ))
                }
            })?,
        )?;
    }

    // __ws_close(session_id)
    {
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_close",
            Function::new(ctx.clone(), move |id: String| -> rquickjs::Result<()> {
                let sessions = sess.lock().unwrap();
                if let Some(session) = sessions.get(&id) {
                    let _ = session.cmd_tx.send(WsCommand::Close);
                    Ok(())
                } else {
                    Err(rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        "WebSocket session not found",
                    ))
                }
            })?,
        )?;
    }

    // __ws_recv(session_id, timeout_ms) → native JS event object
    {
        let h = handle.clone();
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_recv",
            Function::new(
                ctx.clone(),
                move |id: String, timeout_ms: f64| -> rquickjs::Result<JsWsEvent> {
                    let evt_rx = {
                        let sessions = sess.lock().unwrap();
                        match sessions.get(&id) {
                            Some(session) => session.evt_rx.clone(),
                            None => {
                                return Ok(JsWsEvent(WsEvent::Close));
                            }
                        }
                    };

                    let timeout = if timeout_ms > 0.0 {
                        std::time::Duration::from_millis(timeout_ms as u64)
                    } else {
                        std::time::Duration::from_secs(60)
                    };

                    let result = h.block_on(async {
                        let mut rx = evt_rx.lock().await;
                        tokio::time::timeout(timeout, rx.recv()).await
                    });

                    match result {
                        Ok(Some(evt)) => Ok(JsWsEvent(evt)),
                        Ok(None) => Ok(JsWsEvent(WsEvent::Close)),
                        Err(_) => Ok(JsWsEvent(WsEvent::Timeout)),
                    }
                },
            )?,
        )?;
    }

    // __ws_cleanup(session_id) — remove session from map
    {
        let sess = Arc::clone(&sessions);
        ctx.globals().set(
            "__ws_cleanup",
            Function::new(ctx.clone(), move |id: String| -> rquickjs::Result<()> {
                let mut sessions = sess.lock().unwrap();
                sessions.remove(&id);
                Ok(())
            })?,
        )?;
    }

    // Register the JS shim
    ctx.eval::<(), _>(include_str!("ws_shim.js"))?;

    Ok(())
}

struct WsSession {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<WsCommand>,
    evt_rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<WsEvent>>>,
}

#[derive(Debug)]
enum WsCommand {
    Send(String),
    Ping,
    Close,
}

#[derive(Debug)]
enum WsEvent {
    Message(String),
    BinaryMessage(Vec<u8>),
    Ping,
    Pong,
    Close,
    Error(String),
    Timeout,
}

/// Wrapper for converting WsEvent into a native JS object via `IntoJs`.
struct JsWsEvent(WsEvent);

impl<'js> IntoJs<'js> for JsWsEvent {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        match self.0 {
            WsEvent::Message(text) => {
                obj.set("type", "message")?;
                obj.set("data", text)?;
            }
            WsEvent::BinaryMessage(data) => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                obj.set("type", "binaryMessage")?;
                obj.set("data", b64)?;
            }
            WsEvent::Ping => {
                obj.set("type", "ping")?;
            }
            WsEvent::Pong => {
                obj.set("type", "pong")?;
            }
            WsEvent::Close => {
                obj.set("type", "close")?;
            }
            WsEvent::Error(msg) => {
                obj.set("type", "error")?;
                obj.set("data", msg)?;
            }
            WsEvent::Timeout => {
                obj.set("type", "timeout")?;
            }
        }
        Ok(obj.into_value())
    }
}

async fn ws_open_impl(
    url: &str,
    timeout_ms: f64,
    metrics: Option<&BuiltinMetrics>,
    sessions: Arc<Mutex<HashMap<String, WsSession>>>,
    handle: tokio::runtime::Handle,
) -> Result<String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let tags: Vec<(String, String)> = vec![("url".to_string(), url.to_string())];

    let connect_start = std::time::Instant::now();
    let timeout = if timeout_ms > 0.0 {
        std::time::Duration::from_millis(timeout_ms as u64)
    } else {
        std::time::Duration::from_secs(60)
    };

    let ws_stream =
        tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url)).await;

    let ws_stream = match ws_stream {
        Ok(Ok((stream, _response))) => stream,
        Ok(Err(e)) => {
            let connecting_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
            if let Some(m) = metrics {
                m.record_ws_connecting(connecting_ms, &tags);
            }
            anyhow::bail!("WebSocket connection failed: {e}");
        }
        Err(_) => {
            let connecting_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
            if let Some(m) = metrics {
                m.record_ws_connecting(connecting_ms, &tags);
            }
            anyhow::bail!("WebSocket connection timed out");
        }
    };

    let connecting_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
    if let Some(m) = metrics {
        m.record_ws_connecting(connecting_ms, &tags);
    }

    let (mut write, mut read) = ws_stream.split();

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WsCommand>();
    let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();

    let session_id = format!("ws_{}", rand::random::<u64>());

    // Spawn read task
    let evt_tx_clone = evt_tx.clone();
    handle.spawn(async move {
        while let Some(msg) = read.next().await {
            let event = match msg {
                Ok(Message::Text(text)) => WsEvent::Message(text.to_string()),
                Ok(Message::Binary(data)) => WsEvent::BinaryMessage(data.to_vec()),
                Ok(Message::Ping(_)) => WsEvent::Ping,
                Ok(Message::Pong(_)) => WsEvent::Pong,
                Ok(Message::Close(_)) => {
                    let _ = evt_tx_clone.send(WsEvent::Close);
                    break;
                }
                Err(e) => {
                    let _ = evt_tx_clone.send(WsEvent::Error(e.to_string()));
                    break;
                }
                _ => continue,
            };
            if evt_tx_clone.send(event).is_err() {
                break;
            }
        }
    });

    // Spawn write task
    handle.spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                WsCommand::Send(text) => {
                    if write.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                WsCommand::Ping => {
                    if write.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
                WsCommand::Close => {
                    let _ = write.send(Message::Close(None)).await;
                    break;
                }
            }
        }
    });

    // Store session
    {
        let mut map = sessions.lock().unwrap();
        map.insert(
            session_id.clone(),
            WsSession {
                cmd_tx,
                evt_rx: Arc::new(tokio::sync::Mutex::new(evt_rx)),
            },
        );
    }

    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use k6_core::metrics::BuiltinMetrics;

    #[test]
    fn ws_metrics_recorded() {
        let metrics = BuiltinMetrics::new();

        let tags = vec![("url".to_string(), "ws://localhost".to_string())];
        metrics.record_ws_connecting(15.5, &tags);
        metrics.record_ws_session(1500.0, &tags);
        metrics.record_ws_msg_sent(&tags);
        metrics.record_ws_msg_sent(&tags);
        metrics.record_ws_msg_received(&tags);
        metrics.record_ws_ping(2.5, &tags);

        assert_eq!(metrics.registry.counter_get("ws_sessions"), 1);
        assert_eq!(metrics.registry.counter_get("ws_msgs_sent"), 2);
        assert_eq!(metrics.registry.counter_get("ws_msgs_received"), 1);

        let connecting = metrics.registry.trend_stats("ws_connecting").unwrap();
        assert!((connecting.avg - 15.5).abs() < 0.1);

        let session = metrics.registry.trend_stats("ws_session_duration").unwrap();
        assert!((session.avg - 1500.0).abs() < 1.0);

        let ping = metrics.registry.trend_stats("ws_ping").unwrap();
        assert!((ping.avg - 2.5).abs() < 0.1);
    }

    #[test]
    fn ws_event_into_js_message() {
        use super::{JsWsEvent, WsEvent};
        let rt = crate::runtime::create_runtime().unwrap();
        let ctx = crate::runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let evt = JsWsEvent(WsEvent::Message("hello world".to_string()));
            let val: rquickjs::Value = rquickjs::IntoJs::into_js(evt, &ctx).unwrap();
            let obj = val.into_object().unwrap();
            let typ: String = obj.get("type").unwrap();
            let data: String = obj.get("data").unwrap();
            assert_eq!(typ, "message");
            assert_eq!(data, "hello world");
        });
    }

    #[test]
    fn ws_event_into_js_close() {
        use super::{JsWsEvent, WsEvent};
        let rt = crate::runtime::create_runtime().unwrap();
        let ctx = crate::runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let evt = JsWsEvent(WsEvent::Close);
            let val: rquickjs::Value = rquickjs::IntoJs::into_js(evt, &ctx).unwrap();
            let obj = val.into_object().unwrap();
            let typ: String = obj.get("type").unwrap();
            assert_eq!(typ, "close");
        });
    }

    #[test]
    fn ws_event_into_js_error() {
        use super::{JsWsEvent, WsEvent};
        let rt = crate::runtime::create_runtime().unwrap();
        let ctx = crate::runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let evt = JsWsEvent(WsEvent::Error("connection reset".to_string()));
            let val: rquickjs::Value = rquickjs::IntoJs::into_js(evt, &ctx).unwrap();
            let obj = val.into_object().unwrap();
            let typ: String = obj.get("type").unwrap();
            let data: String = obj.get("data").unwrap();
            assert_eq!(typ, "error");
            assert_eq!(data, "connection reset");
        });
    }

    #[test]
    fn ws_event_into_js_handles_special_chars() {
        use super::{JsWsEvent, WsEvent};
        let rt = crate::runtime::create_runtime().unwrap();
        let ctx = crate::runtime::create_context(&rt).unwrap();

        ctx.with(|ctx| {
            let evt = JsWsEvent(WsEvent::Message(r#"he said "hello""#.to_string()));
            let val: rquickjs::Value = rquickjs::IntoJs::into_js(evt, &ctx).unwrap();
            let obj = val.into_object().unwrap();
            let data: String = obj.get("data").unwrap();
            assert_eq!(data, r#"he said "hello""#);
        });
    }
}
