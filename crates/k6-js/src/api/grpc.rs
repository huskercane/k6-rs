use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use rquickjs::Function;

use k6_core::metrics::BuiltinMetrics;

/// Register the gRPC module matching k6/net/grpc API.
///
/// Provides:
/// - `grpc.Client()` constructor
/// - `client.connect(address, params)` — connect to gRPC server
/// - `client.invoke(method, request, params)` — unary RPC call
/// - `client.close()` — close connection
///
/// Also provides status codes: `grpc.StatusOK`, `grpc.StatusCancelled`, etc.
pub fn register(
    ctx: &rquickjs::Ctx<'_>,
    handle: tokio::runtime::Handle,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    let connections: Arc<Mutex<HashMap<String, GrpcConnection>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // __grpc_connect(address, params_json) → connection_id
    {
        let h = handle.clone();
        let conns = Arc::clone(&connections);
        ctx.globals().set(
            "__grpc_connect",
            Function::new(
                ctx.clone(),
                move |address: String, params_json: String| -> rquickjs::Result<String> {
                    let conns = Arc::clone(&conns);
                    let h2 = h.clone();

                    let result = h.block_on(async {
                        grpc_connect_impl(&address, &params_json, conns, h2).await
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

    // __grpc_invoke(conn_id, method, request_json, metadata_json) → response JSON
    {
        let h = handle.clone();
        let m = metrics.clone();
        let conns = Arc::clone(&connections);
        ctx.globals().set(
            "__grpc_invoke",
            Function::new(
                ctx.clone(),
                move |conn_id: String,
                      method: String,
                      request_json: String,
                      metadata_json: String|
                      -> rquickjs::Result<String> {
                    let conns = Arc::clone(&conns);
                    let m = m.clone();

                    let result = h.block_on(async {
                        grpc_invoke_impl(&conn_id, &method, &request_json, &metadata_json, conns, m.as_ref())
                            .await
                    });

                    match result {
                        Ok(json) => Ok(json),
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

    // __grpc_close(conn_id)
    {
        let conns = Arc::clone(&connections);
        ctx.globals().set(
            "__grpc_close",
            Function::new(
                ctx.clone(),
                move |conn_id: String| -> rquickjs::Result<()> {
                    let mut map = conns.lock().unwrap();
                    map.remove(&conn_id);
                    Ok(())
                },
            )?,
        )?;
    }

    // Register the JS shim with gRPC status codes and Client constructor
    ctx.eval::<(), _>(include_str!("grpc_shim.js"))?;

    Ok(())
}

struct GrpcConnection {
    endpoint: String,
    channel: tonic::transport::Channel,
    metadata: HashMap<String, String>,
}

async fn grpc_connect_impl(
    address: &str,
    params_json: &str,
    connections: Arc<Mutex<HashMap<String, GrpcConnection>>>,
    _handle: tokio::runtime::Handle,
) -> Result<String> {
    let params: serde_json::Value = serde_json::from_str(params_json).unwrap_or_default();

    // Normalize address — add http:// if no scheme
    let endpoint = if address.contains("://") {
        address.to_string()
    } else {
        // Check if plaintext (default for non-TLS)
        let plaintext = params
            .get("plaintext")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if plaintext {
            format!("http://{address}")
        } else {
            format!("https://{address}")
        }
    };

    let channel = tonic::transport::Endpoint::from_shared(endpoint.clone())?
        .connect()
        .await?;

    // Extract metadata from params
    let mut metadata = HashMap::new();
    if let Some(meta) = params.get("metadata").and_then(|v| v.as_object()) {
        for (k, v) in meta {
            if let Some(s) = v.as_str() {
                metadata.insert(k.clone(), s.to_string());
            }
        }
    }

    let conn_id = format!("grpc_{}", rand::random::<u64>());

    {
        let mut map = connections.lock().unwrap();
        map.insert(
            conn_id.clone(),
            GrpcConnection {
                endpoint,
                channel,
                metadata,
            },
        );
    }

    Ok(conn_id)
}

async fn grpc_invoke_impl(
    conn_id: &str,
    method: &str,
    request_json: &str,
    metadata_json: &str,
    connections: Arc<Mutex<HashMap<String, GrpcConnection>>>,
    metrics: Option<&BuiltinMetrics>,
) -> Result<String> {
    use tonic::codec::ProstCodec;

    let (channel, default_metadata) = {
        let map = connections.lock().unwrap();
        let conn = map
            .get(conn_id)
            .ok_or_else(|| anyhow::anyhow!("gRPC connection not found: {conn_id}"))?;
        (conn.channel.clone(), conn.metadata.clone())
    };

    let tags: Vec<(String, String)> = vec![("method".to_string(), method.to_string())];
    let start = Instant::now();

    // Parse the method path — should be "/package.Service/Method"
    let method_path = if method.starts_with('/') {
        method.to_string()
    } else {
        format!("/{method}")
    };

    // Build request metadata
    let mut meta = tonic::metadata::MetadataMap::new();
    for (k, v) in &default_metadata {
        if let Ok(val) = v.parse() {
            let _ = meta.insert(
                k.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>()
                    .unwrap(),
                val,
            );
        }
    }

    // Parse per-call metadata
    if !metadata_json.is_empty() && metadata_json != "{}" {
        if let Ok(call_meta) = serde_json::from_str::<HashMap<String, String>>(metadata_json) {
            for (k, v) in &call_meta {
                if let Ok(val) = v.parse() {
                    let _ = meta.insert(
                        k.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>()
                            .unwrap(),
                        val,
                    );
                }
            }
        }
    }

    // Encode request as raw bytes (JSON → prost_types::Struct → bytes)
    // For generic gRPC invocation, we send raw bytes
    let request_bytes = request_json.as_bytes().to_vec();

    // Create a generic gRPC client
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await?;

    let codec: ProstCodec<prost_types::Value, prost_types::Value> = ProstCodec::default();
    let path: tonic::codegen::http::uri::PathAndQuery = method_path.parse()?;

    // Convert JSON to prost Value
    let request_value = json_to_prost_value(request_json)?;
    let mut req = tonic::Request::new(request_value);
    *req.metadata_mut() = meta;

    let response = client
        .unary(req, path, codec)
        .await;

    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

    if let Some(m) = metrics {
        m.record_grpc_request(duration_ms, &tags);
    }

    match response {
        Ok(resp) => {
            let status_code = 0; // OK
            let body = resp.into_inner();
            let body_json = prost_value_to_json(&body);

            Ok(serde_json::json!({
                "status": status_code,
                "message": body_json,
                "headers": {},
                "trailers": {},
            })
            .to_string())
        }
        Err(status) => {
            let code = status.code() as i32;
            let message = status.message().to_string();

            Ok(serde_json::json!({
                "status": code,
                "message": null,
                "error": message,
                "headers": {},
                "trailers": {},
            })
            .to_string())
        }
    }
}

fn json_to_prost_value(json: &str) -> Result<prost_types::Value> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    Ok(serde_to_prost(&v))
}

fn serde_to_prost(v: &serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match v {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
            values: arr.iter().map(serde_to_prost).collect(),
        }),
        serde_json::Value::Object(map) => Kind::StructValue(prost_types::Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), serde_to_prost(v)))
                .collect(),
        }),
    };

    prost_types::Value { kind: Some(kind) }
}

fn prost_value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;

    match &v.kind {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::json!(*n),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => {
            let map: serde_json::Map<String, serde_json::Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use k6_core::metrics::BuiltinMetrics;

    #[test]
    fn grpc_metrics_recorded() {
        let metrics = BuiltinMetrics::new();
        let tags = vec![("method".to_string(), "/test.Svc/Call".to_string())];

        metrics.record_grpc_request(50.0, &tags);
        metrics.record_grpc_request(75.0, &tags);

        assert_eq!(metrics.registry.counter_get("grpc_reqs"), 2);

        let stats = metrics.registry.trend_stats("grpc_req_duration").unwrap();
        assert_eq!(stats.count, 2);
        assert!((stats.avg - 62.5).abs() < 0.1);
    }

    #[test]
    fn json_to_prost_roundtrip() {
        use super::{json_to_prost_value, prost_value_to_json};

        let json_str = r#"{"name":"test","count":42,"active":true,"tags":["a","b"]}"#;
        let prost = json_to_prost_value(json_str).unwrap();
        let back = prost_value_to_json(&prost);

        let original: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert_eq!(back["name"], original["name"]);
        assert_eq!(back["active"], original["active"]);
        assert_eq!(back["tags"], original["tags"]);
    }
}
