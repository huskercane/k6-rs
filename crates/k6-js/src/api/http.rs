use std::sync::Arc;

use anyhow::Result;
use rquickjs::{Ctx, Function, IntoJs, Object, Value};

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, HttpMethod, HttpRequest, ResponseBody, Timings};

/// HTTP response data that converts directly into a native JS object via `IntoJs`,
/// bypassing JSON serialization/parsing on the hot path.
struct JsHttpResponse {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
    timings: Timings,
    url: String,
    error: String,
    error_code: u32,
}

impl<'js> IntoJs<'js> for JsHttpResponse {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;

        obj.set("status", self.status)?;
        obj.set("body", self.body)?;
        obj.set("headers", build_headers_obj(ctx, &self.headers)?)?;
        obj.set("url", self.url)?;
        obj.set("timings", build_timings_obj(ctx, &self.timings)?)?;
        obj.set("error", self.error)?;
        obj.set("error_code", self.error_code)?;

        Ok(obj.into_value())
    }
}

/// Classify an error into a k6-compatible error code.
///
/// Error codes follow k6 conventions:
/// - 1000: generic error
/// - 1010: DNS resolution failed
/// - 1020: connection timeout
/// - 1050: connection refused
/// - 1100: TLS error
/// - 1200: request timeout
/// - 1300: connection reset
/// - 1400: blocked by policy
fn classify_error(err: &anyhow::Error) -> u32 {
    let msg = err.to_string().to_lowercase();

    if msg.contains("blocked by") {
        return 1400;
    }
    if msg.contains("dns") || msg.contains("resolve") || msg.contains("name or service not known")
    {
        return 1010;
    }
    if msg.contains("tls") || msg.contains("ssl") || msg.contains("certificate") {
        return 1100;
    }
    if msg.contains("connection refused") {
        return 1050;
    }
    if msg.contains("connection reset") || msg.contains("broken pipe") {
        return 1300;
    }
    if msg.contains("timed out") || msg.contains("timeout") {
        if msg.contains("connect") {
            return 1020;
        }
        return 1200;
    }

    1000
}

/// Register the k6 `http` object with get/post methods.
///
/// HTTP requests bridge through `Handle::block_on` to the async `HttpClient`.
/// The backpressure semaphore is acquired before sending.
pub fn register<C: HttpClient + 'static>(
    ctx: &Ctx<'_>,
    handle: tokio::runtime::Handle,
    client: Arc<C>,
    backpressure: Backpressure,
) -> Result<()> {
    register_with_metrics(ctx, handle, client, backpressure, None)
}

/// Register HTTP with optional metrics collection.
pub fn register_with_metrics<C: HttpClient + 'static>(
    ctx: &Ctx<'_>,
    handle: tokio::runtime::Handle,
    client: Arc<C>,
    backpressure: Backpressure,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    // Register the low-level Rust function for HTTP requests
    {
        let handle = handle.clone();
        let client = Arc::clone(&client);
        let bp = backpressure.clone();
        let metrics = metrics.clone();

        ctx.globals().set(
            "__http_request",
            Function::new(
                ctx.clone(),
                move |method: String, url: String, body: rquickjs::Value<'_>, headers_json: String, timeout_ms: f64| -> rquickjs::Result<JsHttpResponse> {
                    let headers: Vec<(String, String)> = serde_json::from_str(&headers_json)
                        .unwrap_or_default();
                    let timeout = if timeout_ms > 0.0 {
                        Some(std::time::Duration::from_millis(timeout_ms as u64))
                    } else {
                        None
                    };

                    let body_bytes = if body.is_null() || body.is_undefined() {
                        None
                    } else if let Some(s) = body.as_string() {
                        Some(s.to_string().unwrap_or_default().into_bytes())
                    } else {
                        None
                    };

                    let send_bytes = body_bytes.as_ref().map(|b| b.len() as u64).unwrap_or(0);

                    let http_method = match method.as_str() {
                        "GET" => HttpMethod::Get,
                        "POST" => HttpMethod::Post,
                        "PUT" => HttpMethod::Put,
                        "PATCH" => HttpMethod::Patch,
                        "DELETE" => HttpMethod::Delete,
                        "HEAD" => HttpMethod::Head,
                        "OPTIONS" => HttpMethod::Options,
                        _ => HttpMethod::Get,
                    };

                    let req = HttpRequest {
                        method: http_method,
                        url,
                        headers,
                        body: body_bytes,
                        timeout,
                    };

                    let result = handle.block_on(async {
                        let _permit = bp.acquire().await;
                        client.send(req).await
                    });

                    match result {
                        Ok(resp) => {
                            if let Some(ref m) = metrics {
                                let failed = resp.status >= 400;
                                m.record_http_request(&resp.timings, failed);
                                m.record_data_sent(send_bytes);
                                let recv_bytes = match &resp.body {
                                    ResponseBody::Buffered(b) => b.len() as u64,
                                    ResponseBody::Discarded => 0,
                                };
                                m.record_data_received(recv_bytes);
                            }

                            let body_str = match &resp.body {
                                ResponseBody::Buffered(b) => {
                                    String::from_utf8_lossy(b).to_string()
                                }
                                ResponseBody::Discarded => String::new(),
                            };

                            Ok(JsHttpResponse {
                                status: resp.status,
                                body: body_str,
                                headers: resp.headers,
                                timings: resp.timings,
                                url: resp.url,
                                error: String::new(),
                                error_code: 0,
                            })
                        }
                        Err(e) => {
                            if let Some(ref m) = metrics {
                                let timings = Timings::default();
                                m.record_http_request(&timings, true);
                            }

                            let error_code = classify_error(&e);
                            Ok(JsHttpResponse {
                                status: 0,
                                body: String::new(),
                                headers: Vec::new(),
                                timings: Timings::default(),
                                url: String::new(),
                                error: e.to_string(),
                                error_code,
                            })
                        }
                    }
                },
            )?,
        )?;
    }

    // JS wrapper with cookie jar and response helpers
    ctx.eval::<(), _>(r##"
        // Per-VU cookie jar
        const __cookieJar = {
            _cookies: {}, // domain -> { name: { value, path, domain, expires, ... } }
            set: function(domain, name, value, opts) {
                if (!this._cookies[domain]) this._cookies[domain] = {};
                this._cookies[domain][name] = Object.assign({ value: value }, opts || {});
            },
            get: function(domain, name) {
                const d = this._cookies[domain];
                return d && d[name] ? d[name].value : undefined;
            },
            cookiesForURL: function(url) {
                try {
                    // Extract domain from URL
                    const match = url.match(/^https?:\/\/([^\/\:]+)/);
                    if (!match) return {};
                    const domain = match[1];
                    return this._cookies[domain] || {};
                } catch(e) { return {}; }
            },
            clear: function() { this._cookies = {}; },
        };

        // Parse Set-Cookie headers from response
        function __extractCookies(headers, url) {
            const cookies = {};
            if (!headers) return cookies;
            for (const key in headers) {
                if (key.toLowerCase() !== 'set-cookie') continue;
                const val = headers[key];
                const parts = (Array.isArray(val) ? val : [val]);
                for (const cookie of parts) {
                    const eqIdx = cookie.indexOf('=');
                    if (eqIdx < 0) continue;
                    const name = cookie.substring(0, eqIdx).trim();
                    const rest = cookie.substring(eqIdx + 1);
                    const semiIdx = rest.indexOf(';');
                    const value = semiIdx >= 0 ? rest.substring(0, semiIdx) : rest;
                    cookies[name] = { name: name, value: value.trim() };
                    // Store in jar
                    try {
                        const match = url.match(/^https?:\/\/([^\/\:]+)/);
                        if (match) __cookieJar.set(match[1], name, value.trim());
                    } catch(e) {}
                }
            }
            return cookies;
        }

        // Build Cookie header from jar for a URL
        function __buildCookieHeader(url) {
            const cookies = __cookieJar.cookiesForURL(url);
            const parts = [];
            for (const name in cookies) {
                parts.push(name + '=' + cookies[name].value);
            }
            return parts.length > 0 ? parts.join('; ') : null;
        }

        function __wrap_response(raw) {
            raw.json = function(selector) {
                const parsed = JSON.parse(raw.body);
                if (selector !== undefined) {
                    return selector.split('.').reduce(function(obj, key) {
                        return obj != null ? obj[key] : undefined;
                    }, parsed);
                }
                return parsed;
            };
            raw.html = function() { return raw.body; };
            raw.cookies = __extractCookies(raw.headers, raw.url || '');
            return raw;
        }
        const __http = {
            request: function(method, url, body, params) {
                // Merge cookie header from jar
                const allHeaders = Object.assign({}, (params && params.headers) || {});
                const jarCookie = __buildCookieHeader(url);
                if (jarCookie && !allHeaders['Cookie'] && !allHeaders['cookie']) {
                    allHeaders['Cookie'] = jarCookie;
                }
                // Merge explicit cookies param
                if (params && params.cookies) {
                    const parts = [];
                    for (const n in params.cookies) {
                        parts.push(n + '=' + params.cookies[n]);
                    }
                    if (parts.length > 0) {
                        allHeaders['Cookie'] = (allHeaders['Cookie'] || '') +
                            (allHeaders['Cookie'] ? '; ' : '') + parts.join('; ');
                    }
                }
                const headers = JSON.stringify(Object.entries(allHeaders));
                const bodyArg = (typeof body === 'object' && body !== null) ? JSON.stringify(body) : (body || null);
                const timeoutMs = (params && params.timeout) ? Number(params.timeout) : 0;
                const responseObj = __http_request(method, url, bodyArg, headers, timeoutMs);
                return __wrap_response(responseObj);
            },
            get: function(url, params) {
                return __http.request('GET', url, null, params);
            },
            post: function(url, body, params) {
                return __http.request('POST', url, body, params);
            },
            put: function(url, body, params) {
                return __http.request('PUT', url, body, params);
            },
            del: function(url, body, params) {
                return __http.request('DELETE', url, body, params);
            },
            patch: function(url, body, params) {
                return __http.request('PATCH', url, body, params);
            },
            head: function(url, params) {
                return __http.request('HEAD', url, null, params);
            },
            options: function(url, body, params) {
                return __http.request('OPTIONS', url, body, params);
            },
            batch: function(requests) {
                // requests can be array or object
                if (Array.isArray(requests)) {
                    return requests.map(function(req) {
                        return __http._parseBatchReq(req);
                    });
                }
                const results = {};
                for (const key in requests) {
                    results[key] = __http._parseBatchReq(requests[key]);
                }
                return results;
            },
            _parseBatchReq: function(req) {
                // String URL → GET
                if (typeof req === 'string') {
                    return __http.get(req);
                }
                // Array: [method, url, body?, params?]
                if (Array.isArray(req)) {
                    return __http.request(req[0], req[1], req[2] || null, req[3]);
                }
                // Object: { method, url, body?, params? }
                if (typeof req === 'object' && req !== null) {
                    return __http.request(
                        req.method || 'GET',
                        req.url,
                        req.body || null,
                        req.params
                    );
                }
                throw new Error('Invalid batch request format');
            },
            expectedStatuses: function() {
                // Collect all valid status specs
                const specs = [];
                for (let i = 0; i < arguments.length; i++) {
                    const arg = arguments[i];
                    if (typeof arg === 'number') {
                        specs.push({ min: arg, max: arg });
                    } else if (typeof arg === 'object' && arg !== null) {
                        specs.push({ min: arg.min || 0, max: arg.max || 999 });
                    }
                }
                return { __expectedStatuses: specs };
            },
            setResponseCallback: function(callback) {
                globalThis.__http_response_callback = callback;
            },
            cookieJar: function() {
                return __cookieJar;
            },
        };
        globalThis.http = __http;
    "##)?;

    Ok(())
}

/// Build a native JS object for response headers, coalescing duplicates into arrays.
fn build_headers_obj<'js>(
    ctx: &Ctx<'js>,
    headers: &[(String, String)],
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;

    for (k, v) in headers {
        let key = k.to_lowercase();
        let existing: Value<'js> = obj.get(&*key)?;
        if existing.is_undefined() {
            obj.set(&*key, v.as_str())?;
        } else if existing.is_array() {
            let arr: rquickjs::Array<'js> = existing.into_array().unwrap();
            arr.set(arr.len(), v.as_str())?;
        } else {
            let arr = rquickjs::Array::new(ctx.clone())?;
            arr.set(0, existing)?;
            arr.set(1, v.as_str())?;
            obj.set(&*key, arr)?;
        }
    }

    Ok(obj)
}

/// Build a native JS object for HTTP timings.
fn build_timings_obj<'js>(ctx: &Ctx<'js>, timings: &Timings) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("blocked", timings.blocked)?;
    obj.set("connecting", timings.connecting)?;
    obj.set("tls_handshaking", timings.tls_handshaking)?;
    obj.set("sending", timings.sending)?;
    obj.set("waiting", timings.waiting)?;
    obj.set("receiving", timings.receiving)?;
    obj.set("duration", timings.duration)?;
    Ok(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime;
    use k6_core::traits::{HttpResponse, ResponseBody, Timings};

    /// A mock HTTP client that returns canned responses.
    struct MockHttpClient {
        status: u16,
        body: String,
    }

    impl MockHttpClient {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    struct MockHttpClientWithCookies {
        status: u16,
        body: String,
        set_cookies: Vec<String>,
    }

    impl MockHttpClientWithCookies {
        fn new(status: u16, body: &str, cookies: Vec<&str>) -> Self {
            Self {
                status,
                body: body.to_string(),
                set_cookies: cookies.into_iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl HttpClient for MockHttpClientWithCookies {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            let mut headers: Vec<(String, String)> =
                vec![("content-type".to_string(), "application/json".to_string())];
            for cookie in &self.set_cookies {
                headers.push(("set-cookie".to_string(), cookie.clone()));
            }
            let resp = HttpResponse {
                status: self.status,
                headers,
                body: ResponseBody::Buffered(self.body.clone().into_bytes()),
                timings: Timings {
                    duration: 50.0,
                    waiting: 45.0,
                    receiving: 5.0,
                    ..Default::default()
                },
                url: "http://mock.test".to_string(),
            };
            async move { Ok(resp) }
        }
    }

    impl HttpClient for MockHttpClient {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            let resp = HttpResponse {
                status: self.status,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: ResponseBody::Buffered(self.body.clone().into_bytes()),
                timings: Timings {
                    duration: 50.0,
                    waiting: 45.0,
                    receiving: 5.0,
                    ..Default::default()
                },
                url: "http://mock.test".to_string(),
            };
            async move { Ok(resp) }
        }
    }

    // All HTTP tests run in spawn_blocking to simulate real VU execution
    // (block_on requires not being on an async thread).

    #[tokio::test]
    async fn http_get_basic() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"ok":true}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let status: i32 = ctx
                    .eval("http.get('http://example.com').status")
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_get_response_body() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"message":"hello"}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let body: String = ctx
                    .eval("http.get('http://example.com').body")
                    .unwrap();
                assert_eq!(body, r#"{"message":"hello"}"#);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_get_with_headers() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let status: i32 = ctx
                    .eval(r#"
                        http.get('http://example.com', {
                            headers: { 'Authorization': 'Bearer token123' }
                        }).status
                    "#)
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_post_with_body() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(201, r#"{"id":1}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let status: i32 = ctx
                    .eval(r#"
                        http.post('http://example.com/api', JSON.stringify({ name: 'test' }), {
                            headers: { 'Content-Type': 'application/json' }
                        }).status
                    "#)
                    .unwrap();
                assert_eq!(status, 201);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_response_has_timings() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let duration: f64 = ctx
                    .eval("http.get('http://example.com').timings.duration")
                    .unwrap();
                assert!((duration - 50.0).abs() < 0.01);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_works_with_check() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                crate::api::check::register(&ctx).unwrap();

                let result: bool = ctx
                    .eval(r#"
                        const res = http.get('http://example.com');
                        check(res, {
                            'status was 200': (r) => r.status === 200,
                            'has timings': (r) => r.timings.duration > 0,
                        })
                    "#)
                    .unwrap();
                assert!(result);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_response_json() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"user":{"name":"Alice","age":30}}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                // Test .json() with no selector
                let name: String = ctx
                    .eval("http.get('http://example.com').json().user.name")
                    .unwrap();
                assert_eq!(name, "Alice");

                // Test .json() with dotpath selector
                let age: i32 = ctx
                    .eval("http.get('http://example.com').json('user.age')")
                    .unwrap();
                assert_eq!(age, 30);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_response_json_check_pattern() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"items":[1,2,3]}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                crate::api::check::register(&ctx).unwrap();

                let result: bool = ctx
                    .eval(r#"
                        const res = http.get('http://example.com');
                        check(res, {
                            'has items': (r) => r.json().items.length === 3,
                            'status ok': (r) => r.status === 200,
                        })
                    "#)
                    .unwrap();
                assert!(result);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_batch_array() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"ok":true}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                // Batch with array of URLs
                let count: i32 = ctx
                    .eval(r#"
                        const responses = http.batch([
                            'http://example.com/a',
                            'http://example.com/b',
                            'http://example.com/c',
                        ]);
                        responses.length
                    "#)
                    .unwrap();
                assert_eq!(count, 3);

                // Verify each response
                let status: i32 = ctx.eval("responses[0].status").unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_batch_object() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, r#"{"ok":true}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                let status: i32 = ctx
                    .eval(r#"
                        const responses = http.batch({
                            home: 'http://example.com/',
                            api: ['POST', 'http://example.com/api', '{"x":1}'],
                            health: { method: 'GET', url: 'http://example.com/health' },
                        });
                        responses.api.status
                    "#)
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_expected_statuses() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                // Create expected statuses object
                let has_specs: bool = ctx
                    .eval(r#"
                        const es = http.expectedStatuses(200, 201, {min: 200, max: 299});
                        es.__expectedStatuses.length === 3
                    "#)
                    .unwrap();
                assert!(has_specs);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_cookie_jar_from_set_cookie() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClientWithCookies::new(
                200,
                "{}",
                vec!["session=abc123; Path=/", "token=xyz; Path=/"],
            ));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                // First request gets Set-Cookie headers
                let has_cookies: bool = ctx
                    .eval(r#"
                        const res = http.get('http://mock.test/login');
                        res.cookies.session !== undefined && res.cookies.token !== undefined
                    "#)
                    .unwrap();
                assert!(has_cookies);

                // Cookie jar has them
                let session: String = ctx
                    .eval("http.cookieJar().get('mock.test', 'session')")
                    .unwrap();
                assert_eq!(session, "abc123");
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_explicit_cookies_param() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();

                // Pass explicit cookies
                let status: i32 = ctx
                    .eval(r#"
                        http.get('http://example.com', {
                            cookies: { session: 'test123' }
                        }).status
                    "#)
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[test]
    fn classify_error_dns() {
        let err = anyhow::anyhow!("dns resolution failed for example.com");
        assert_eq!(super::classify_error(&err), 1010);
    }

    #[test]
    fn classify_error_tls() {
        let err = anyhow::anyhow!("TLS handshake failed: certificate expired");
        assert_eq!(super::classify_error(&err), 1100);
    }

    #[test]
    fn classify_error_timeout() {
        let err = anyhow::anyhow!("request timed out after 30s");
        assert_eq!(super::classify_error(&err), 1200);
    }

    #[test]
    fn classify_error_connect_timeout() {
        let err = anyhow::anyhow!("connect timed out");
        assert_eq!(super::classify_error(&err), 1020);
    }

    #[test]
    fn classify_error_connection_refused() {
        let err = anyhow::anyhow!("connection refused");
        assert_eq!(super::classify_error(&err), 1050);
    }

    #[test]
    fn classify_error_connection_reset() {
        let err = anyhow::anyhow!("connection reset by peer");
        assert_eq!(super::classify_error(&err), 1300);
    }

    #[test]
    fn classify_error_blocked() {
        let err = anyhow::anyhow!("hostname is blocked by blockHostnames");
        assert_eq!(super::classify_error(&err), 1400);
    }

    #[test]
    fn classify_error_generic() {
        let err = anyhow::anyhow!("something went wrong");
        assert_eq!(super::classify_error(&err), 1000);
    }
}
