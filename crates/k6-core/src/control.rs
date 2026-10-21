//! k6 v2.0.0 REST control surface. No JavaScript or executor implementation lives here.
use crate::{
    metrics::{GroupSnapshot, MetricsRegistry},
    thresholds::Threshold,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, Response},
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub type HookFuture = Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send>>;
/// Implementations create and use their runtime on its owning thread.
pub trait Lifecycle: Send + Sync {
    fn invoke(&self, setup: bool, data: Option<String>, cancel: CancellationToken) -> HookFuture;
}

struct Execution {
    status: u32,
    paused: bool,
    started: Option<Instant>,
    ended: Option<Instant>,
    stopped: bool,
}

pub struct Control {
    execution: Mutex<Execution>,
    pub active_vus: AtomicU32,
    pub initialized_vus: AtomicU32,
    pub cancel: CancellationToken,
    /// REST stop is an immediate abort, unlike the first Ctrl+C graceful stop.
    pub abort: CancellationToken,
    resume: tokio::sync::Notify,
    executor: String,
    scenario: String,
    pub registry: Arc<MetricsRegistry>,
    thresholds: HashMap<String, Vec<Threshold>>,
    tainted_metrics: Mutex<HashMap<String, bool>>,
    setup_data: Mutex<Option<String>>,
    /// Serialize lifecycle mutations, including automatic setup/teardown.
    lifecycle_lock: tokio::sync::Mutex<()>,
    lifecycle: Arc<dyn Lifecycle>,
    closing: CancellationToken,
}

impl Control {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paused: bool,
        max_vus: u32,
        executor: String,
        scenario: String,
        registry: Arc<MetricsRegistry>,
        thresholds: HashMap<String, Vec<Threshold>>,
        cancel: CancellationToken,
        lifecycle: Arc<dyn Lifecycle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            execution: Mutex::new(Execution {
                status: if paused { 4 } else { 3 },
                paused,
                started: None,
                ended: None,
                stopped: false,
            }),
            active_vus: AtomicU32::new(0),
            initialized_vus: AtomicU32::new(max_vus),
            cancel,
            abort: CancellationToken::new(),
            resume: tokio::sync::Notify::new(),
            executor,
            scenario,
            registry,
            thresholds,
            tainted_metrics: Mutex::new(HashMap::new()),
            setup_data: Mutex::new(None),
            lifecycle_lock: tokio::sync::Mutex::new(()),
            lifecycle,
            closing: CancellationToken::new(),
        })
    }
    pub async fn wait_for_start(&self) -> bool {
        loop {
            let notified = self.resume.notified();
            {
                let mut e = self.execution.lock();
                if self.cancel.is_cancelled() {
                    return false;
                }
                if !e.paused {
                    e.started = Some(Instant::now());
                    e.status = 5;
                    return true;
                }
            }
            tokio::select! { _ = notified => {}, _ = self.cancel.cancelled() => return false }
        }
    }
    pub fn phase(&self, phase: u32) {
        self.execution.lock().status = phase;
    }
    pub fn end(&self) {
        self.update_taints();
        let mut e = self.execution.lock();
        e.ended = Some(Instant::now());
        e.status = if e.stopped { 10 } else { 9 };
        self.active_vus.store(0, Ordering::Relaxed);
    }
    pub fn stopped(&self) -> bool {
        self.execution.lock().stopped
    }
    pub fn elapsed(&self) -> Duration {
        let e = self.execution.lock();
        e.started
            .map(|s| e.ended.unwrap_or_else(Instant::now).duration_since(s))
            .unwrap_or_default()
    }
    pub fn setup_data(&self) -> Option<String> {
        self.setup_data.lock().clone()
    }
    pub async fn hook(&self, setup: bool) -> anyhow::Result<Option<String>> {
        let _guard = self.lifecycle_lock.lock().await;
        anyhow::ensure!(!self.closing.is_cancelled(), "execution is shutting down");
        let cancel = self.closing.child_token();
        let mut call = self
            .lifecycle
            .invoke(setup, self.setup_data(), cancel.clone());
        let data = tokio::select! {
            result = &mut call => result?,
            _ = self.abort.cancelled(), if setup => {
                cancel.cancel();
                call.await?
            }
        };
        anyhow::ensure!(!self.closing.is_cancelled(), "execution is shutting down");
        if setup {
            *self.setup_data.lock() = data.clone();
        }
        Ok(data)
    }
    /// Reject new mutations and let an in-progress hook observe cancellation
    /// before the caller takes its final metrics/console snapshot.
    pub async fn close_lifecycle(&self) {
        self.closing.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.lifecycle_lock.lock()).await;
    }
    pub async fn put_setup(&self, data: Option<String>) -> anyhow::Result<()> {
        let _guard = self.lifecycle_lock.lock().await;
        anyhow::ensure!(!self.closing.is_cancelled(), "execution is shutting down");
        *self.setup_data.lock() = data;
        Ok(())
    }
    fn status_with(&self, e: &Execution, tainted: bool) -> Value {
        json!({"data":{"type":"status","id":"default","attributes":{
            "status":e.status,"paused":e.paused,"vus":self.active_vus.load(Ordering::Relaxed),
            "vus-max":self.initialized_vus.load(Ordering::Relaxed),"stopped":e.stopped || self.cancel.is_cancelled(),
            "running":e.started.is_some() && e.ended.is_none(),"tainted":tainted
        }}})
    }
    fn taints(&self, snapshot: &crate::metrics::MetricsSnapshot) -> HashMap<String, bool> {
        let mut taints = HashMap::new();
        for result in crate::thresholds::evaluate(&self.thresholds, snapshot).results {
            *taints.entry(result.metric).or_insert(false) |= !result.passed;
        }
        taints
    }
    pub fn status(&self) -> Value {
        let tainted = self.tainted_metrics.lock().values().any(|v| *v);
        self.status_with(&self.execution.lock(), tainted)
    }
    fn update_taints(&self) {
        if self.thresholds.is_empty() {
            return;
        }
        let snapshot = self.registry.snapshot(self.elapsed().as_secs_f64());
        let observed =
            crate::summary::build_summary_data(&snapshot, Duration::ZERO, Some(&self.thresholds))
                .metrics;
        let mut taints = self.taints(&snapshot);
        taints.retain(|name, _| observed.contains_key(name));
        *self.tainted_metrics.lock() = taints;
    }
    fn patch(&self, body: &[u8]) -> Result<Value, ApiError> {
        let patch = parse_patch(body).map_err(|e| ApiError::new(400, "Invalid data", e))?;
        // Match upstream's ordered mutations: stopped overrides everything;
        // resume takes effect even if a following scaling field is rejected.
        let tainted = self.status()["data"]["attributes"]["tainted"]
            .as_bool()
            .unwrap_or(false);
        let mut e = self.execution.lock();
        if patch.stopped {
            e.stopped = true;
            let response = self.status_with(&e, tainted);
            self.cancel.cancel();
            self.abort.cancel();
            self.resume.notify_one();
            return Ok(response);
        }
        if let Some(paused) = patch.paused {
            if e.started.is_none() && e.paused {
                if paused {
                    return Err(ApiError::new(
                        500,
                        "Pause error",
                        "execution is already paused",
                    ));
                }
                e.paused = false;
                self.resume.notify_one();
            } else {
                return Err(ApiError::new(
                    500,
                    "Pause error",
                    format!(
                        "{} executor '{}' doesn't support pause and resume operations after its start",
                        self.executor, self.scenario
                    ),
                ));
            }
        }
        if patch.scale {
            return Err(ApiError::new(
                500,
                "Execution config error",
                "live VU configuration updates are not supported",
            ));
        }
        drop(e);
        Ok(self.status())
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    title: &'static str,
    detail: String,
}
impl ApiError {
    fn new(status: u16, title: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            title,
            detail: detail.into(),
        }
    }
    fn response(self, content_type: &'static str) -> Response<Body> {
        response(
            self.status,
            content_type,
            json!({"errors":[{"status":self.status.to_string(),"title":self.title,"detail":self.detail}]}),
        )
    }
}
struct Patch {
    stopped: bool,
    paused: Option<bool>,
    scale: bool,
}
fn parse_json(body: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(body).map_err(|e| {
        if e.is_eof() {
            "unexpected end of JSON input".into()
        } else {
            e.to_string()
        }
    })
}
fn field<'a>(v: &'a Value, name: &str) -> &'a Value {
    v.as_object()
        .and_then(|o| {
            o.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
        .unwrap_or(&Value::Null)
}
fn parse_patch(body: &[u8]) -> Result<Patch, String> {
    let v = parse_json(body)?;
    let d = field(&v, "data");
    let a = field(d, "attributes");
    for (x, name) in [
        (&v, "v1.StatusJSONAPI"),
        (d, "v1.statusData"),
        (a, "v1.Status"),
    ] {
        if !x.is_null() && !x.is_object() {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type {name}",
                json_kind(x)
            ));
        }
    }
    for name in ["type", "id"] {
        let x = field(d, name);
        if !x.is_null() && !x.is_string() {
            return Err(format!(
                "json: cannot unmarshal {} into Go struct field statusData.data.{name} of type string",
                json_kind(x)
            ));
        }
    }
    let status = field(a, "status");
    if !status.is_null() && !status.as_u64().is_some_and(|n| n <= u32::MAX as u64) {
        return Err(format!(
            "json: cannot unmarshal {} into Go struct field Status.data.attributes.status of type lib.ExecutionStatus",
            json_kind(status)
        ));
    }
    let paused = match field(a, "paused") {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Object(_) => {
            let object = field(a, "paused");
            for name in ["Bool", "Valid"] {
                let value = field(object, name);
                if !value.is_null() && !value.is_boolean() {
                    return Err(format!(
                        "json: cannot unmarshal {} into Go struct field NullBool.{name} of type bool",
                        json_kind(value)
                    ));
                }
            }
            Some(field(object, "Bool").as_bool().unwrap_or(false))
        }
        x => {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type null.Bool",
                json_kind(x)
            ));
        }
    };
    let mut scale = false;
    for name in ["vus", "vus-max"] {
        match field(a, name) {
            Value::Null => {}
            Value::String(s) if s.is_empty() => {}
            Value::String(s) => {
                s.parse::<i64>()
                    .map_err(|_| format!("strconv.ParseInt: parsing {s:?}: invalid syntax"))?;
                scale = true;
            }
            Value::Number(n) if n.as_i64().is_some() => scale = true,
            Value::Object(o) => {
                let object = Value::Object(o.clone());
                let integer = field(&object, "Int64");
                let valid = field(&object, "Valid");
                if !integer.is_null() && integer.as_i64().is_none() {
                    return Err("json: invalid Int64 in null.Int".into());
                }
                if !valid.is_null() && !valid.is_boolean() {
                    return Err("json: invalid Valid in null.Int".into());
                }
                scale = true;
            }
            x => {
                return Err(format!(
                    "json: cannot unmarshal {} into Go value of type null.Int",
                    json_kind(x)
                ));
            }
        }
    }
    // Read-only fields are still type-checked by Go's decoder.
    for name in ["stopped", "running", "tainted"] {
        let x = field(a, name);
        if !x.is_null() && !x.is_boolean() {
            return Err(format!(
                "json: cannot unmarshal {} into Go struct field Status.data.attributes.{name} of type bool",
                json_kind(x)
            ));
        }
    }
    Ok(Patch {
        stopped: field(a, "stopped").as_bool().unwrap_or(false),
        paused,
        scale,
    })
}
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "bool",
        Value::Array(_) => "array",
        _ => "object",
    }
}
fn response(status: u16, content_type: &'static str, value: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from(value.to_string()))
        .unwrap()
}
const JSON: &str = "application/json; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
fn setup_response(data: Option<String>) -> Response<Body> {
    let attributes = match data {
        None => json!({}),
        Some(s) => json!({"data":serde_json::from_str::<Value>(&s).unwrap_or(Value::Null)}),
    };
    response(
        200,
        "application/json",
        json!({"data":{"type":"setupData","id":"default","attributes":attributes}}),
    )
}

pub struct Server {
    reporter: tokio::task::JoinHandle<()>,
    pub address: std::net::SocketAddr,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    control: Arc<Control>,
}
impl Server {
    pub async fn bind(address: &str, control: Arc<Control>) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let router = Router::new().fallback(dispatch).with_state(control.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await;
        });
        let observer = control.clone();
        let reporter = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_secs(1));
            ticks.tick().await;
            let mut count = 0;
            loop {
                ticks.tick().await;
                count += 1;
                if count % 2 == 0 {
                    observer.update_taints();
                }
                let active = observer.active_vus.load(Ordering::Relaxed);
                observer.registry.gauge_set("vus", active as f64);
                observer.registry.gauge_set(
                    "vus_max",
                    observer.initialized_vus.load(Ordering::Relaxed) as f64,
                );
                if active > 0 {
                    observer.registry.counter_add("data_sent", 0);
                    observer.registry.counter_add("data_received", 0);
                }
            }
        });
        Ok(Self {
            address,
            stop,
            task,
            control,
            reporter,
        })
    }
    pub async fn shutdown(mut self) {
        self.reporter.abort();
        self.control.closing.cancel();
        self.stop.cancel();
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.reporter.abort();
        self.control.closing.cancel();
        self.stop.cancel();
        self.task.abort();
    }
}

fn decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn dispatch(State(control): State<Arc<Control>>, request: Request<Body>) -> Response<Body> {
    let path = decode_path(request.uri().path());
    let method = request.method().as_str().to_owned();
    let known = matches!(
        path.as_str(),
        "/v1/status" | "/v1/metrics" | "/v1/groups" | "/v1/setup" | "/v1/teardown"
    ) || path.starts_with("/v1/metrics/")
        || path.starts_with("/v1/groups/");
    let allowed = match path.as_str() {
        "/v1/status" => method == "GET" || method == "PATCH",
        "/v1/setup" => matches!(method.as_str(), "GET" | "PUT" | "POST"),
        "/v1/teardown" => method == "POST",
        _ => method == "GET",
    };
    if known && !allowed {
        return Response::builder().status(405).body(Body::empty()).unwrap();
    }
    if !known {
        if path.starts_with("/v1/") {
            return Response::builder()
                .status(404)
                .header("content-type", TEXT)
                .body(Body::from("404 page not found\n"))
                .unwrap();
        }
        return Response::builder()
            .header("content-type", TEXT)
            .body(Body::from("ok"))
            .unwrap();
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/v1/status") => response(200, JSON, control.status()),
        ("PATCH", "/v1/status") => match to_bytes(request.into_body(), usize::MAX).await {
            Ok(body) => match control.patch(&body) {
                Ok(v) => response(200, JSON, v),
                Err(e) => e.response(JSON),
            },
            Err(e) => ApiError::new(400, "Couldn't read request", e.to_string()).response(JSON),
        },
        ("GET", "/v1/setup") => setup_response(control.setup_data()),
        ("PUT", "/v1/setup") => {
            let body = match to_bytes(request.into_body(), usize::MAX).await {
                Ok(b) => b,
                Err(e) => {
                    return ApiError::new(400, "Error reading request body", e.to_string())
                        .response(TEXT);
                }
            };
            let data = if body.is_empty() {
                None
            } else {
                match parse_json(&body) {
                    Ok(v) => Some(v.to_string()),
                    Err(e) => {
                        return ApiError::new(400, "Error parsing request body", e).response(TEXT);
                    }
                }
            };
            match control.put_setup(data).await {
                Ok(()) => setup_response(control.setup_data()),
                Err(e) => {
                    ApiError::new(500, "Error setting setup data", e.to_string()).response(TEXT)
                }
            }
        }
        ("POST", "/v1/setup" | "/v1/teardown") => {
            let setup = path == "/v1/setup";
            match control.hook(setup).await {
                Ok(data) => {
                    if setup {
                        setup_response(data)
                    } else {
                        Response::new(Body::empty())
                    }
                }
                Err(e) => ApiError::new(
                    500,
                    if setup {
                        "Error executing setup"
                    } else {
                        "Error executing teardown"
                    },
                    e.to_string(),
                )
                .response(TEXT),
            }
        }
        _ => {
            let snap = control.registry.snapshot(control.elapsed().as_secs_f64());
            let (collection, item, title) = if path.starts_with("/v1/metrics") {
                (
                    metric_resources(
                        &snap,
                        &control.tainted_metrics.lock(),
                        &control.thresholds,
                        &control.registry,
                    ),
                    path.strip_prefix("/v1/metrics/"),
                    "No metric with that ID was found",
                )
            } else {
                let mut groups = Vec::new();
                flatten_groups(&snap.group_tree, None, &mut groups);
                (
                    groups,
                    path.strip_prefix("/v1/groups/"),
                    "No group with that ID was found",
                )
            };
            match item {
                None => response(200, TEXT, json!({"data":collection})),
                Some(id) => match collection.into_iter().find(|v| v["id"] == id) {
                    Some(v) => response(200, TEXT, json!({"data":v})),
                    None => ApiError::new(404, "Not Found", title).response(TEXT),
                },
            }
        }
    }
}
fn md5(s: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn flatten_groups(group: &GroupSnapshot, parent: Option<&str>, out: &mut Vec<Value>) {
    let id = md5(&group.path);
    let checks: Vec<Value> = group
        .checks
        .iter()
        .map(|(name, (passes, fails))| {
            let path = format!("{}::{name}", group.path);
            json!({"id":md5(&path),"name":name,"path":path,"passes":passes,"fails":fails})
        })
        .collect();
    let children: Vec<Value> = group
        .children
        .values()
        .map(|g| json!({"type":"groups","id":md5(&g.path)}))
        .collect();
    out.push(json!({"type":"groups","id":id,"attributes":{"name":group.name,"path":group.path,"checks":if checks.is_empty(){Value::Null}else{json!(checks)}},"relationships":{"groups":{"data":children},"parent":{"data":parent.map(|id|json!({"type":"groups","id":id}))}}}));
    for child in group.children.values() {
        flatten_groups(child, Some(&id), out);
    }
}
fn metric_resources(
    snap: &crate::metrics::MetricsSnapshot,
    taints: &HashMap<String, bool>,
    thresholds: &HashMap<String, Vec<Threshold>>,
    registry: &MetricsRegistry,
) -> Vec<Value> {
    let mut selectors = thresholds.clone();
    selectors
        .entry("http_req_duration{expected_response:true}".into())
        .or_default();
    let summary = crate::summary::build_summary_data(snap, Duration::ZERO, Some(&selectors));
    let mut out = Vec::new();
    for (name, m) in summary.metrics {
        let mut sample = serde_json::to_value(&m.values).unwrap();
        if let Some(o) = sample.as_object_mut() {
            match m.metric_type.as_str() {
                "trend" => o.retain(|k, _| {
                    matches!(
                        k.as_str(),
                        "avg" | "min" | "max" | "med" | "p(90)" | "p(95)"
                    )
                }),
                "rate" => o.retain(|k, _| k == "rate"),
                "gauge" => o.retain(|k, _| k == "value"),
                _ => {}
            }
        }
        let contains = registry.contains(&name);
        out.push(json!({"type":"metrics","id":name,"attributes":{"type":m.metric_type,"contains":contains,"tainted":taints.get(&name),"sample":sample}}));
    }
    out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Hooks;
    impl Lifecycle for Hooks {
        fn invoke(&self, setup: bool, _: Option<String>, _: CancellationToken) -> HookFuture {
            Box::pin(async move { Ok(setup.then(|| "null".into())) })
        }
    }
    fn control(paused: bool) -> Arc<Control> {
        Control::new(
            paused,
            10,
            "constant-vus".into(),
            "default".into(),
            Arc::new(MetricsRegistry::new()),
            HashMap::new(),
            CancellationToken::new(),
            Arc::new(Hooks),
        )
    }
    fn patch(c: &Control, a: Value) -> Result<Value, ApiError> {
        c.patch(json!({"data":{"attributes":a}}).to_string().as_bytes())
    }
    #[tokio::test]
    async fn resume_precedes_rejected_scaling_and_stop_overrides_pause() {
        let c = control(true);
        assert_eq!(
            patch(&c, json!({"paused":true})).unwrap_err().detail,
            "execution is already paused"
        );
        assert_eq!(
            patch(&c, json!({"paused":false,"vus":100}))
                .unwrap_err()
                .status,
            500
        );
        assert!(c.wait_for_start().await);
        assert!(
            patch(&c, json!({"paused":false}))
                .unwrap_err()
                .detail
                .contains("doesn't support")
        );
        let status = patch(&c, json!({"stopped":true,"paused":true,"vus":100})).unwrap();
        assert_eq!(status["data"]["attributes"]["stopped"], true);
        assert!(c.abort.is_cancelled());
        assert!(c.cancel.is_cancelled());
    }
    #[tokio::test]
    async fn concurrent_resume_and_stop_never_lose_the_stop() {
        for _ in 0..50 {
            let c = control(true);
            let a = c.clone();
            let b = c.clone();
            let resume = tokio::spawn(async move { patch(&a, json!({"paused":false})) });
            let stop = tokio::spawn(async move { patch(&b, json!({"stopped":true})) });
            let _ = tokio::join!(resume, stop);
            assert!(!c.wait_for_start().await);
            assert!(c.stopped());
            c.end();
            assert_eq!(patch(&c, json!({"vus":0})).unwrap_err().status, 500);
            assert!(patch(&c, json!({"stopped":true})).is_ok());
        }
    }
    #[tokio::test]
    async fn setup_distinguishes_absent_and_json_null_and_rejects_writes_after_close() {
        let c = control(false);
        assert!(c.setup_data().is_none());
        c.hook(true).await.unwrap();
        assert_eq!(c.setup_data().as_deref(), Some("null"));
        c.put_setup(None).await.unwrap();
        assert!(c.setup_data().is_none());
        c.closing.cancel();
        assert!(c.put_setup(Some("42".into())).await.is_err());
        assert!(c.hook(true).await.is_err());
    }
    #[test]
    fn null_and_unknown_fields_are_noops_but_zero_and_negative_vus_are_rejected() {
        let c = control(false);
        for b in [
            "null",
            "{}",
            r#"{"data":null}"#,
            r#"{"data":{"attributes":{"vus":null,"paused":null,"unknown":42}}}"#,
        ] {
            assert!(c.patch(b.as_bytes()).is_ok());
        }
        for v in [json!(0), json!(-1), json!(100), json!("2")] {
            assert_eq!(patch(&c, json!({"vus":v})).unwrap_err().status, 500);
        }
        assert_eq!(
            c.patch(b"{").unwrap_err().detail,
            "unexpected end of JSON input"
        );
        assert_eq!(patch(&c, json!({"paused":"true"})).unwrap_err().status, 400);
    }
    #[test]
    fn group_relationships_and_metric_samples_use_rest_format() {
        let c = control(false);
        c.registry.check_add("::api", "ok", true);
        c.registry.trend_add("latency", 3.0);
        c.registry.register_contains("latency", "time");
        let snap = c.registry.snapshot(1.0);
        let mut groups = Vec::new();
        flatten_groups(&snap.group_tree, None, &mut groups);
        assert_eq!(groups[0]["attributes"]["checks"], Value::Null);
        assert_eq!(groups[1]["relationships"]["parent"]["data"]["id"], md5(""));
        assert_eq!(groups[1]["attributes"]["checks"][0]["id"], md5("::api::ok"));
        let metrics = metric_resources(&snap, &HashMap::new(), &HashMap::new(), &c.registry);
        let m = metrics.iter().find(|m| m["id"] == "latency").unwrap();
        assert_eq!(m["attributes"]["contains"], "time");
        assert!(m["attributes"]["sample"].get("count").is_none());
        assert_eq!(m["attributes"]["sample"]["avg"], 3.0);
    }
}
