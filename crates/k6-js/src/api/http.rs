use std::sync::Arc;

use anyhow::Result;
use rquickjs::prelude::Async;
use rquickjs::{Array, Ctx, Function, IntoJs, Object, Value};

use k6_core::backpressure::Backpressure;
use k6_core::metrics::BuiltinMetrics;
use k6_core::traits::{HttpClient, HttpMethod, HttpRequest, HttpResponse, ResponseBody, Timings};

use crate::vu_sched::{AsyncMeta, HostOp, OpDone, Shared, YielderPtr};

pub(crate) enum ResponseCallback {
    Default,
    Disabled,
    ExpectedStatuses(Vec<(u16, u16)>),
}

impl ResponseCallback {
    fn expected(&self, status: u16) -> Option<bool> {
        match self {
            Self::Disabled => None,
            Self::Default => Some((200..=399).contains(&status)),
            Self::ExpectedStatuses(specs) => Some(
                specs
                    .iter()
                    .any(|(min, max)| status >= *min && status <= *max),
            ),
        }
    }
}

/// HTTP response data that converts directly into a native JS object via `IntoJs`,
/// bypassing JSON serialization/parsing on the hot path.
pub(crate) struct JsHttpResponse {
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
pub(crate) fn classify_error(err: &anyhow::Error) -> u32 {
    let msg = err.to_string().to_lowercase();

    if msg.contains("blocked by") {
        return 1400;
    }
    if msg.contains("dns") || msg.contains("resolve") || msg.contains("name or service not known") {
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
                move |method: String,
                      url: String,
                      body: rquickjs::Value<'_>,
                      headers_val: rquickjs::Value<'_>,
                      timeout_ms: f64,
                      tags_val: rquickjs::Value<'_>,
                      response_callback_val: rquickjs::Value<'_>|
                      -> rquickjs::Result<JsHttpResponse> {
                    // Same request-building + response/metrics mapping as the
                    // async path — single source of truth via build_http_request
                    // / finish_http_response (CG-3 tags, failure mapping,
                    // data_sent/received, zero-copy body). Headers/tags are read
                    // from the native JS objects into owned data here, avoiding a
                    // per-request JSON.stringify + serde round-trip.
                    let user_tags = object_entries_to_pairs(&tags_val);
                    let response_callback = parse_response_callback(&response_callback_val);
                    let req = build_http_request(&method, url, &body, &headers_val, timeout_ms);

                    let result = handle.block_on(async {
                        let _permit = bp.acquire().await;
                        client.send(req).await
                    });

                    Ok(finish_http_response(
                        result,
                        &method,
                        user_tags,
                        &response_callback,
                        metrics.as_ref(),
                    ))
                },
            )?,
        )?;
    }

    // JS wrapper with cookie jar and response helpers
    register_http_object(ctx)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Async http surface — the ALREADY-AWAITED path (asyncRequest, and later
// promise-returning timers). This is the part of Phase 1 that is genuinely
// mechanical: scripts already `await` it, so no suspension mechanism is needed.
//
// SYNC `http.get()` is deliberately NOT handled here. Making a synchronous,
// un-awaited JS call yield a shared loop is unsolved and gated on the B2
// stackful-coroutine spike (see ASYNC_RUNTIME_PLAN.md). Converting it via a
// forced-await transpile (B1) is unsound in general (map(http.get) needs
// whole-program dataflow), so it is explicitly out of this increment.
//
// The two helpers below are shared by this async path. The sync `__http_request`
// closure keeps its own inline twin this increment (left untouched to bound
// blast radius); both collapse onto the coroutine-yielding version at cutover.
// ---------------------------------------------------------------------------

/// Build an owned `HttpRequest` from the JS-side arguments. Runs synchronously
/// (it reads the `Value`s), so an async host fn can call it up front and then
/// move the owned request into its future.
pub(crate) fn build_http_request(
    method: &str,
    url: String,
    body: &Value<'_>,
    headers_val: &Value<'_>,
    timeout_ms: f64,
) -> HttpRequest {
    let headers = object_entries_to_pairs(headers_val);
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
    let http_method = match method {
        "GET" => HttpMethod::Get,
        "POST" => HttpMethod::Post,
        "PUT" => HttpMethod::Put,
        "PATCH" => HttpMethod::Patch,
        "DELETE" => HttpMethod::Delete,
        "HEAD" => HttpMethod::Head,
        "OPTIONS" => HttpMethod::Options,
        _ => HttpMethod::Get,
    };
    HttpRequest {
        method: http_method,
        url,
        headers,
        body: body_bytes,
        timeout,
    }
}

/// Turn a client `send` result into the `JsHttpResponse` the JS layer sees,
/// recording metrics with the same tag/failure semantics as the sync path
/// (CG-3 system+user tags, data_sent/received, status=0 on transport failure).
/// Plain data in/out — no `Ctx` — so it can run inside the host fn's future.
pub(crate) fn finish_http_response(
    result: anyhow::Result<HttpResponse>,
    method: &str,
    user_tags: Vec<(String, String)>,
    response_callback: &ResponseCallback,
    metrics: Option<&BuiltinMetrics>,
) -> JsHttpResponse {
    match result {
        Ok(mut resp) => {
            if let Some(m) = metrics {
                let expected = response_callback.expected(resp.status);
                let mut all_tags: Vec<(String, String)> = user_tags.clone();
                all_tags.push(("status".to_string(), resp.status.to_string()));
                all_tags.push(("method".to_string(), method.to_string()));
                if let Some(expected) = expected {
                    all_tags.push(("expected_response".to_string(), expected.to_string()));
                }
                m.record_http_request_tagged_with_failure(
                    &resp.timings,
                    expected.map(|ok| !ok),
                    &all_tags,
                );
                m.record_data_sent(resp.data_sent);
                m.record_data_received(resp.data_received);
            }

            let body_str = match std::mem::replace(&mut resp.body, ResponseBody::Discarded) {
                ResponseBody::Buffered(b) => String::from_utf8(b)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
                ResponseBody::Discarded => String::new(),
            };

            JsHttpResponse {
                status: resp.status,
                body: body_str,
                headers: resp.headers,
                timings: resp.timings,
                url: resp.url,
                error: String::new(),
                error_code: 0,
            }
        }
        Err(e) => {
            if let Some(m) = metrics {
                let timings = Timings::default();
                let expected = response_callback.expected(0);
                let mut all_tags: Vec<(String, String)> = user_tags.clone();
                all_tags.push(("status".to_string(), "0".to_string()));
                all_tags.push(("method".to_string(), method.to_string()));
                if let Some(expected) = expected {
                    all_tags.push(("expected_response".to_string(), expected.to_string()));
                }
                m.record_http_request_tagged_with_failure(
                    &timings,
                    expected.map(|ok| !ok),
                    &all_tags,
                );
            }

            let error_code = classify_error(&e);
            JsHttpResponse {
                status: 0,
                body: String::new(),
                headers: Vec::new(),
                timings: Timings::default(),
                url: String::new(),
                error: e.to_string(),
                error_code,
            }
        }
    }
}

/// Register `__http_request_async` — the async counterpart of `__http_request`,
/// for VUs running on an `AsyncRuntime`. Same request/response semantics, but it
/// **awaits** the client on the VU's own loop (no `block_on`) and resolves a JS
/// promise. Intended to back `http.asyncRequest` (and `http.batch` later).
///
/// Registered on an async context only: on a sync `Runtime` the returned
/// future would never be driven (no async executor), so the promise would hang.
///
/// TODO(cutover): this resolves a **raw** `JsHttpResponse`. When the production
/// `http.asyncRequest` moves onto this at the sync-blocking cutover, its JS
/// wrapper MUST re-apply `__wrap_response` (adds `.json()`/`.html()`/`.cookies`)
/// AND the per-VU cookie jar (inject `Cookie` on request, extract `Set-Cookie`
/// on response) — both of which the old `Promise.resolve().then(__http.request)`
/// stub got for free by routing through `__http.request`. Skipping them silently
/// regresses `res.json()` and cookies on the async path. Parity bar: the
/// existing `http_async_request_resolves_response` test asserts `res.json().ok`.
pub fn register_async_request<C: HttpClient + 'static>(
    ctx: &Ctx<'_>,
    client: Arc<C>,
    backpressure: Backpressure,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    ctx.globals().set(
        "__http_request_async",
        Function::new(
            ctx.clone(),
            Async(
                move |method: String,
                      url: String,
                      body: Value<'_>,
                      headers_val: Value<'_>,
                      timeout_ms: f64,
                      tags_val: Value<'_>,
                      response_callback_val: Value<'_>| {
                    // Sync prep: read the JS Values into owned data up front...
                    let req = build_http_request(&method, url, &body, &headers_val, timeout_ms);
                    let user_tags = object_entries_to_pairs(&tags_val);
                    let response_callback = parse_response_callback(&response_callback_val);
                    // ...clone per-call captures so the future is 'static...
                    let client = Arc::clone(&client);
                    let bp = backpressure.clone();
                    let metrics = metrics.clone();
                    // ...then the future holds nothing borrowed from `'js`.
                    async move {
                        let result = {
                            let _permit = bp.acquire().await;
                            client.send(req).await
                        };
                        finish_http_response(
                            result,
                            &method,
                            user_tags,
                            &response_callback,
                            metrics.as_ref(),
                        )
                    }
                },
            ),
        )?,
    )?;
    Ok(())
}


/// Eval the shared `http` JS object (cookie jar, __wrap_response, request/
/// get/post/batch/asyncRequest). Reused by BOTH the block_on `__http_request`
/// path and the yielding path — the native `__http_request` it calls differs;
/// the jar + wrapper are identical.
fn register_http_object(ctx: &Ctx<'_>) -> Result<()> {
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
            // Request preparation (cookie-jar merge, body serialization, content
            // type) shared by the sync `request` and the async path — so an
            // async request participates in the jar BOTH directions too.
            _prep: function(method, url, body, params) {
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
                let bodyArg = body || null;
                if (body && typeof body === 'object' && !(body instanceof ArrayBuffer)) {
                    // Upstream object-body semantics (js/modules/k6/http/request.go):
                    // an object with any http.file() value is sent as
                    // multipart/form-data; otherwise it is form-encoded as
                    // application/x-www-form-urlencoded. Neither path is JSON —
                    // JSON requires an explicit JSON.stringify() by the caller.
                    const setContentType = function(v) {
                        delete allHeaders['Content-Type'];
                        delete allHeaders['content-type'];
                        allHeaders['Content-Type'] = v;
                    };
                    let hasFile = false;
                    for (const key in body) {
                        const v = body[key];
                        if (v && typeof v === 'object' && v.__isFileData) { hasFile = true; break; }
                    }
                    if (hasFile) {
                        const boundary = '----k6rsFormBoundary'
                            + Date.now().toString(16)
                            + Math.floor(Math.random() * 0x100000000).toString(16);
                        const esc = function(s) {
                            return String(s).replace(/\\/g, '\\\\').replace(/"/g, '\\"');
                        };
                        let parts = '';
                        for (const key in body) {
                            const v = body[key];
                            parts += '--' + boundary + '\r\n';
                            if (v && typeof v === 'object' && v.__isFileData) {
                                parts += 'Content-Disposition: form-data; name="' + esc(key)
                                    + '"; filename="' + esc(v.filename) + '"\r\n';
                                parts += 'Content-Type: ' + v.content_type + '\r\n\r\n';
                                parts += v.data + '\r\n';
                            } else {
                                parts += 'Content-Disposition: form-data; name="' + esc(key) + '"\r\n\r\n';
                                parts += String(v) + '\r\n';
                            }
                        }
                        parts += '--' + boundary + '--\r\n';
                        bodyArg = parts;
                        setContentType('multipart/form-data; boundary=' + boundary);
                    } else {
                        const kv = [];
                        for (const key in body) {
                            const v = body[key];
                            if (Array.isArray(v)) {
                                for (let i = 0; i < v.length; i++) {
                                    kv.push(encodeURIComponent(key) + '=' + encodeURIComponent(String(v[i])));
                                }
                            } else {
                                kv.push(encodeURIComponent(key) + '=' + encodeURIComponent(String(v)));
                            }
                        }
                        bodyArg = kv.join('&');
                        setContentType('application/x-www-form-urlencoded');
                    }
                }
                const timeoutMs = (params && params.timeout) ? Number(params.timeout) : 0;
                // CG-3: forward user-provided `tags: { k: v }` so the engine
                // can attach them to http metric samples and store the full
                // tag combination. Headers and tags are passed as native
                // objects (not JSON strings); the Rust side iterates them
                // directly, avoiding a stringify + parse round-trip per request.
                const tagsArg = (params && params.tags && typeof params.tags === 'object')
                    ? params.tags
                    : null;
                let responseCallbackArg = (typeof globalThis.__http_response_callback !== 'undefined')
                    ? globalThis.__http_response_callback
                    : undefined;
                if (params && Object.prototype.hasOwnProperty.call(params, 'responseCallback')) {
                    responseCallbackArg = params.responseCallback;
                }
                return [method, url, bodyArg, allHeaders, timeoutMs, tagsArg, responseCallbackArg];
            },
            request: function(method, url, body, params) {
                const a = this._prep(method, url, body, params);
                return __wrap_response(__http_request(a[0], a[1], a[2], a[3], a[4], a[5], a[6]));
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
                if (arguments.length === 0) {
                    throw new Error('no arguments');
                }
                const specs = [];
                for (let i = 0; i < arguments.length; i++) {
                    const arg = arguments[i];
                    if (typeof arg === 'number' && Number.isInteger(arg)) {
                        specs.push({ min: arg, max: arg });
                    } else if (typeof arg === 'object' && arg !== null) {
                        const min = arg.min;
                        const max = arg.max;
                        if (!Number.isInteger(min) || !Number.isInteger(max)) {
                            throw new Error('both min and max need to be integers for argument number ' + (i + 1));
                        }
                        specs.push({ min: min, max: max });
                    } else {
                        throw new Error('argument number ' + (i + 1) + ' to expectedStatuses was neither an integer nor an object like {min:100, max:329}');
                    }
                }
                return { __expectedStatuses: specs };
            },
            setResponseCallback: function(callback) {
                globalThis.__http_response_callback = callback;
            },
            asyncRequest: function(method, url, body, params) {
                return Promise.resolve().then(function() {
                    return __http.request(method, url, body || null, params);
                });
            },
            file: function(data, filename, contentType) {
                if (typeof data !== 'string') {
                    throw new Error('invalid type ' + (typeof data) + ', expected string or ArrayBuffer');
                }
                // Marker + fields mirror upstream FileData. `__isFileData` lets
                // the request builder detect a file value inside a form object
                // and switch that request to multipart/form-data. Default
                // filename is a unique-ish token like upstream's UnixNano.
                return {
                    __isFileData: true,
                    data: data,
                    filename: filename || String(Date.now()),
                    content_type: contentType || 'application/octet-stream',
                };
            },
            cookieJar: function() {
                return __cookieJar;
            },
        };
        globalThis.http = __http;
    "##)?;
    Ok(())
}

/// Register the `http` API for a **coroutine VU** (the async-runtime graduation):
/// the native `__http_request` *yields* the coroutine (`AwaitOne(Http)`) instead
/// of `block_on`, then records metrics via `finish_http_response` AFTER resume —
/// pure Rust, no borrow held across the await. The scheduler (`drive_vu`) runs
/// the request; this fn is metrics-aware but client-free (I1). The shared JS
/// object (`register_http_object`) — cookie jar both directions + `__wrap_response`
/// — is reused verbatim; only the native fn it calls differs.
pub(crate) fn register_yielding_http(
    ctx: &Ctx<'_>,
    yp: YielderPtr,
    shared: Shared,
    metrics: Option<BuiltinMetrics>,
) -> Result<()> {
    // sync http.get/request: YIELD the coroutine, resume with the response.
    {
        let metrics = metrics.clone();
        ctx.globals().set(
            "__http_request",
            Function::new(
                ctx.clone(),
                move |method: String,
                      url: String,
                      body: Value<'_>,
                      headers_val: Value<'_>,
                      timeout_ms: f64,
                      tags_val: Value<'_>,
                      response_callback_val: Value<'_>|
                      -> JsHttpResponse {
                    // Read the JS Values into owned data BEFORE the yield (they
                    // don't survive the coroutine suspension); method/tags/callback
                    // stay on the coroutine stack across the yield.
                    let req = build_http_request(&method, url, &body, &headers_val, timeout_ms);
                    let user_tags = object_entries_to_pairs(&tags_val);
                    let response_callback = parse_response_callback(&response_callback_val);
                    let result = match yp.await_one(HostOp::Http(req)) {
                        OpDone::Http(r) => r,
                        _ => unreachable!("yielding http op must resolve as Http"),
                    };
                    // Metrics recorded here, after resume — coroutine-side, no
                    // borrow across the await. Reuses the sync CG-3 mapping.
                    finish_http_response(result, &method, user_tags, &response_callback, metrics.as_ref())
                },
            )?,
        )?;
    }

    // asyncRequest: REGISTER the request (does NOT yield) and return its op id.
    // The driver loop runs it concurrently with others and resolves the promise
    // via finish_http_response (metrics driver-loop-side). Prepped args come from
    // __http._prep, so the async path carries the cookie jar + tags too.
    ctx.globals().set(
        "__register_async_http",
        Function::new(
            ctx.clone(),
            move |method: String,
                  url: String,
                  body: Value<'_>,
                  headers_val: Value<'_>,
                  timeout_ms: f64,
                  tags_val: Value<'_>,
                  response_callback_val: Value<'_>|
                  -> f64 {
                let req = build_http_request(&method, url, &body, &headers_val, timeout_ms);
                let meta = AsyncMeta {
                    method,
                    user_tags: object_entries_to_pairs(&tags_val),
                    response_callback: parse_response_callback(&response_callback_val),
                };
                shared.register_async(HostOp::Http(req), Some(meta)) as f64
            },
        )?,
    )?;

    // http.batch: a SYNCHRONOUS host fn that runs N requests CONCURRENTLY. It
    // takes an array of prepped request tuples and yields AwaitAll — neither
    // AwaitOne (serializes) nor asyncRequest (returns a promise). Returns the raw
    // responses in input order (the JS wrapper applies __wrap_response).
    {
        let metrics = metrics.clone();
        ctx.globals().set(
            "__http_batch",
            Function::new(ctx.clone(), move |prepped: Array<'_>| -> rquickjs::Result<Vec<JsHttpResponse>> {
                let mut ops = Vec::with_capacity(prepped.len());
                // Per-request finishing meta kept on the coroutine stack across the yield.
                let mut metas: Vec<(String, Vec<(String, String)>, ResponseCallback)> = Vec::with_capacity(prepped.len());
                for i in 0..prepped.len() {
                    let t: Array = prepped.get(i)?;
                    let method: String = t.get(0)?;
                    let url: String = t.get(1)?;
                    let body: Value = t.get(2)?;
                    let headers: Value = t.get(3)?;
                    let timeout_ms: f64 = t.get(4)?;
                    let tags: Value = t.get(5)?;
                    let cb: Value = t.get(6)?;
                    ops.push(HostOp::Http(build_http_request(&method, url, &body, &headers, timeout_ms)));
                    metas.push((method, object_entries_to_pairs(&tags), parse_response_callback(&cb)));
                }
                let results = yp.await_all(ops);
                // Owned Vec<JsHttpResponse> -> JS array via IntoJs (no 'js borrow
                // across the yield, and no closure-lifetime unification).
                let mut out = Vec::with_capacity(metas.len());
                for (result, (method, user_tags, response_callback)) in results.into_iter().zip(metas) {
                    let r = match result {
                        OpDone::Http(r) => r,
                        _ => unreachable!("batch op must resolve as Http"),
                    };
                    out.push(finish_http_response(r, &method, user_tags, &response_callback, metrics.as_ref()));
                }
                Ok(out)
            })?,
        )?;
    }

    register_http_object(ctx)?;

    // Override the stub asyncRequest + serial batch with the concurrent ones.
    // Both reuse __http._prep (cookie jar both directions) and __wrap_response
    // (parity: res.json()/.cookies).
    ctx.eval::<(), _>(
        r#"
        globalThis.http.asyncRequest = function (method, url, body, params) {
            var a = globalThis.http._prep(method, url, body || null, params);
            var id = __register_async_http(a[0], a[1], a[2], a[3], a[4], a[5], a[6]);
            return new Promise(function (resolve) {
                globalThis.__resolvers[id] = function (raw) { resolve(__wrap_response(raw)); };
            });
        };
        globalThis.http.batch = function (requests) {
            var http = globalThis.http;
            var isArray = Array.isArray(requests);
            var keys = isArray ? null : Object.keys(requests);
            var list = isArray ? requests : keys.map(function (k) { return requests[k]; });
            var preps = list.map(function (req) {
                if (typeof req === 'string') return http._prep('GET', req, null, undefined);
                if (Array.isArray(req)) return http._prep(req[0], req[1], req[2] || null, req[3]);
                return http._prep(req.method || 'GET', req.url, req.body || null, req.params);
            });
            var raws = __http_batch(preps).map(__wrap_response);
            if (isArray) return raws;
            // Object input -> object output keyed the same (upstream parity):
            // responses.a.status must work, not responses[0].status.
            var out = {};
            for (var i = 0; i < keys.length; i++) out[keys[i]] = raws[i];
            return out;
        };
    "#,
    )?;
    Ok(())
}

pub(crate) fn parse_response_callback(value: &Value<'_>) -> ResponseCallback {
    if value.is_null() {
        return ResponseCallback::Disabled;
    }

    let Some(obj) = value.as_object() else {
        return ResponseCallback::Default;
    };
    let Ok(specs_value) = obj.get::<_, Value>("__expectedStatuses") else {
        return ResponseCallback::Default;
    };
    let Some(specs_array) = specs_value.into_array() else {
        return ResponseCallback::Default;
    };

    let mut specs = Vec::new();
    for i in 0..specs_array.len() {
        let Ok(spec) = specs_array.get::<Value>(i) else {
            continue;
        };
        let Some(spec_obj) = spec.as_object() else {
            continue;
        };
        let Ok(min) = spec_obj.get::<_, u16>("min") else {
            continue;
        };
        let Ok(max) = spec_obj.get::<_, u16>("max") else {
            continue;
        };
        specs.push((min, max));
    }

    if specs.is_empty() {
        ResponseCallback::Default
    } else {
        ResponseCallback::ExpectedStatuses(specs)
    }
}

/// Collect a JS object's own enumerable string-valued entries into pairs,
/// preserving enumeration order.
///
/// Used for both request headers and user tags on the request hot path. A
/// non-object value (`undefined`/`null`, e.g. `http.get(url)` with no params)
/// yields no pairs. Entries whose value is not a JS string are skipped, which
/// closely matches the prior
/// `serde_json::from_str::<Vec<(String, String)>>(...).unwrap_or_default()`
/// contract (where a non-string value made the whole parse fail) while
/// avoiding the `JSON.stringify` + parse round-trip entirely.
pub(crate) fn object_entries_to_pairs(value: &Value<'_>) -> Vec<(String, String)> {
    match value.as_object() {
        Some(obj) => obj.props::<String, String>().flatten().collect(),
        None => Vec::new(),
    }
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
                data_sent: 0,
                data_received: 0,
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
                data_sent: 0,
                data_received: 0,
            };
            async move { Ok(resp) }
        }
    }

    /// Mock that records the request headers it received, so tests can assert
    /// the JS->Rust request bridge forwards headers through the native
    /// object-passing path (no JSON round-trip).
    struct MockHttpClientCapture {
        last_headers: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        last_body: Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    }

    impl MockHttpClientCapture {
        fn new() -> (
            Self,
            Arc<std::sync::Mutex<Vec<(String, String)>>>,
            Arc<std::sync::Mutex<Option<Vec<u8>>>>,
        ) {
            let headers = Arc::new(std::sync::Mutex::new(Vec::new()));
            let body = Arc::new(std::sync::Mutex::new(None));
            (
                Self {
                    last_headers: Arc::clone(&headers),
                    last_body: Arc::clone(&body),
                },
                headers,
                body,
            )
        }
    }

    impl HttpClient for MockHttpClientCapture {
        fn send(
            &self,
            req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            *self.last_headers.lock().unwrap() = req.headers.clone();
            *self.last_body.lock().unwrap() = req.body.clone();
            let resp = HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Buffered(b"{}".to_vec()),
                timings: Timings::default(),
                url: "http://mock.test".to_string(),
                data_sent: 0,
                data_received: 0,
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

                let status: i32 = ctx.eval("http.get('http://example.com').status").unwrap();
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

                let body: String = ctx.eval("http.get('http://example.com').body").unwrap();
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
                    .eval(
                        r#"
                        http.get('http://example.com', {
                            headers: { 'Authorization': 'Bearer token123' }
                        }).status
                    "#,
                    )
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_request_forwards_headers_natively() {
        // Regression-locks the native header-passing bridge: custom request
        // headers built in JS must reach the HttpClient. Previously these were
        // JSON.stringify'd in JS and parsed back in Rust; now the object is
        // iterated directly, so a broken iteration would silently drop them.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let (client, captured, _body) = MockHttpClientCapture::new();
            let client = Arc::new(client);
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                let status: i32 = ctx
                    .eval(
                        r#"
                        http.get('http://example.com', {
                            headers: { 'Authorization': 'Bearer token123', 'X-Trace': 'abc' }
                        }).status
                    "#,
                    )
                    .unwrap();
                assert_eq!(status, 200);
            });

            let headers = captured.lock().unwrap().clone();
            assert!(
                headers
                    .iter()
                    .any(|(k, v)| k == "Authorization" && v == "Bearer token123"),
                "expected Authorization header to reach client, got {headers:?}"
            );
            assert!(
                headers.iter().any(|(k, v)| k == "X-Trace" && v == "abc"),
                "expected X-Trace header to reach client, got {headers:?}"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn object_entries_to_pairs_semantics() {
        // Locks the contract the request bridge relies on: an object yields its
        // string entries in order, a non-object (undefined/null) yields none,
        // and non-string values are skipped rather than aborting the whole set.
        let rt = runtime::create_runtime().unwrap();
        let ctx = runtime::create_context(&rt).unwrap();
        ctx.with(|ctx| {
            let obj: Value = ctx.eval("({a:'1', b:'2'})").unwrap();
            assert_eq!(
                object_entries_to_pairs(&obj),
                vec![
                    ("a".to_string(), "1".to_string()),
                    ("b".to_string(), "2".to_string())
                ]
            );

            let null_val: Value = ctx.eval("null").unwrap();
            assert!(object_entries_to_pairs(&null_val).is_empty());

            let undef: Value = ctx.eval("undefined").unwrap();
            assert!(object_entries_to_pairs(&undef).is_empty());

            // A numeric value is skipped; the string entry still comes through.
            let mixed: Value = ctx.eval("({a:'1', n:5})").unwrap();
            assert_eq!(
                object_entries_to_pairs(&mixed),
                vec![("a".to_string(), "1".to_string())]
            );
        });
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
                    .eval(
                        r#"
                        http.post('http://example.com/api', JSON.stringify({ name: 'test' }), {
                            headers: { 'Content-Type': 'application/json' }
                        }).status
                    "#,
                    )
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
                    .eval(
                        r#"
                        const res = http.get('http://example.com');
                        check(res, {
                            'status was 200': (r) => r.status === 200,
                            'has timings': (r) => r.timings.duration > 0,
                        })
                    "#,
                    )
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
            let client = Arc::new(MockHttpClient::new(
                200,
                r#"{"user":{"name":"Alice","age":30}}"#,
            ));
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
                    .eval(
                        r#"
                        const res = http.get('http://example.com');
                        check(res, {
                            'has items': (r) => r.json().items.length === 3,
                            'status ok': (r) => r.status === 200,
                        })
                    "#,
                    )
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
                    .eval(
                        r#"
                        const responses = http.batch([
                            'http://example.com/a',
                            'http://example.com/b',
                            'http://example.com/c',
                        ]);
                        responses.length
                    "#,
                    )
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
                    .eval(
                        r#"
                        const responses = http.batch({
                            home: 'http://example.com/',
                            api: ['POST', 'http://example.com/api', '{"x":1}'],
                            health: { method: 'GET', url: 'http://example.com/health' },
                        });
                        responses.api.status
                    "#,
                    )
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
                    .eval(
                        r#"
                        const es = http.expectedStatuses(200, 201, {min: 200, max: 299});
                        es.__expectedStatuses.length === 3
                    "#,
                    )
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
                    .eval(
                        r#"
                        const res = http.get('http://mock.test/login');
                        res.cookies.session !== undefined && res.cookies.token !== undefined
                    "#,
                    )
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
                    .eval(
                        r#"
                        http.get('http://example.com', {
                            cookies: { session: 'test123' }
                        }).status
                    "#,
                    )
                    .unwrap();
                assert_eq!(status, 200);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_async_request_resolves_response() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(201, r#"{"ok":true}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                ctx.eval::<(), _>(
                    r#"
                    (async function() {
                        const res = await http.asyncRequest('POST', 'http://example.com/api', { a: 'a', b: 2 }, {
                            headers: { 'Content-Type': 'application/x-www-form-urlencoded; charset=utf-8' }
                        });
                        globalThis.__async_status = res.status;
                        globalThis.__async_body_ok = res.json().ok;
                    })();
                    "#,
                )
                .unwrap();
            });
            runtime::drain_pending_jobs(&rt);
            ctx.with(|ctx| {
                let status: i32 = ctx.globals().get("__async_status").unwrap();
                let ok: bool = ctx.globals().get("__async_body_ok").unwrap();
                assert_eq!(status, 201);
                assert!(ok);
            });
        })
        .await
        .unwrap();
    }

    #[test]
    fn async_http_request_resolves_on_async_loop() {
        // The already-awaited http surface as a TRUE async host fn on the async
        // VU foundation: __http_request_async awaits the client across an
        // `.await`, driven by spawn_driver, and resolves a JS promise that
        // `await http.asyncRequest(...)` unwraps. This is the !Send-across-await
        // proof end-to-end on the pool-of-loops runtime.
        //
        // Sync http.get is intentionally NOT exercised here — it needs the B2
        // stackful-coroutine suspension mechanism, out of this increment.
        use crate::runtime;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&rt, async {
            let qjs = runtime::create_async_runtime().await.unwrap();
            let ctx = runtime::create_async_context(&qjs).await.unwrap();
            let client = Arc::new(MockHttpClient::new(201, r#"{"ok":true}"#));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register_async_request(&ctx, client, bp, None).unwrap();
                // Minimal asyncRequest wrapper over the async native fn — the
                // shape the production wrapper takes once the http object moves
                // onto the async runtime at cutover.
                ctx.eval::<(), _>(
                    r#"
                    globalThis.http = {
                        asyncRequest: function(method, url, body, params) {
                            return __http_request_async(
                                method, url, body || null,
                                (params && params.headers) || {},
                                (params && params.timeout) || 0,
                                (params && params.tags) || null,
                                undefined);
                        }
                    };
                    "#,
                )
                .unwrap();
            })
            .await;

            let driver = runtime::spawn_driver(&qjs);

            let status: i32 = ctx
                .async_with(async |ctx| {
                    let p: rquickjs::Promise = ctx
                        .eval(
                            r#"http.asyncRequest('POST', 'http://example.com/api',
                                   { a: 'a', b: 2 },
                                   { headers: { 'X-Test': '1' } })
                               .then(r => r.status)"#,
                        )
                        .unwrap();
                    p.into_future().await.unwrap()
                })
                .await;
            assert_eq!(status, 201);

            driver.abort();
        });
    }

    #[tokio::test]
    async fn http_file_returns_data_for_request_body() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                let ok: bool = ctx
                    .eval(
                        r#"
                        const f = http.file('hello', 'test.txt', 'text/plain');
                        f.data === 'hello' &&
                            f.filename === 'test.txt' &&
                            f.content_type === 'text/plain' &&
                            http.post('http://example.com/upload', f.data).status === 200
                        "#,
                    )
                    .unwrap();
                assert!(ok);
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_object_body_defaults_to_form_urlencoded() {
        // Upstream: an object body with no file is sent as
        // application/x-www-form-urlencoded, NOT JSON. Locks that k6-rs no
        // longer JSON.stringify's object bodies by default.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let (client, headers, body) = MockHttpClientCapture::new();
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register(&ctx, handle, Arc::new(client), bp).unwrap();
                let _: i32 = ctx
                    .eval("http.post('http://example.com/f', { a: 'x y', b: 2 }).status")
                    .unwrap();
            });

            let sent = String::from_utf8(body.lock().unwrap().clone().unwrap_or_default()).unwrap();
            assert_eq!(sent, "a=x%20y&b=2", "object body must be form-urlencoded");
            let hs = headers.lock().unwrap().clone();
            assert!(
                hs.iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case("content-type")
                        && v == "application/x-www-form-urlencoded"),
                "content-type must be set to form-urlencoded, got {hs:?}"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_object_body_with_file_becomes_multipart() {
        // Upstream: a form object containing an http.file() value is encoded as
        // multipart/form-data with a boundary; the file becomes a part with a
        // filename + its content-type, and plain fields become form fields.
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let (client, headers, body) = MockHttpClientCapture::new();
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register(&ctx, handle, Arc::new(client), bp).unwrap();
                let _: i32 = ctx
                    .eval(
                        r#"
                        http.post('http://example.com/upload', {
                            field: 'value',
                            document: http.file('FILEDATA', 'report.csv', 'text/csv'),
                        }).status
                        "#,
                    )
                    .unwrap();
            });

            let hs = headers.lock().unwrap().clone();
            let ct = hs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.clone())
                .expect("content-type header present");
            assert!(
                ct.starts_with("multipart/form-data; boundary="),
                "expected multipart content-type, got {ct}"
            );
            let boundary = ct.rsplit("boundary=").next().unwrap();

            let sent = String::from_utf8(body.lock().unwrap().clone().unwrap_or_default()).unwrap();
            // Boundary in the body must match the one advertised in the header.
            assert!(sent.contains(&format!("--{boundary}\r\n")));
            assert!(sent.trim_end().ends_with(&format!("--{boundary}--")));
            // Plain field part.
            assert!(
                sent.contains("Content-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n")
            );
            // File part carries filename + its content-type + the data.
            assert!(sent.contains(
                "Content-Disposition: form-data; name=\"document\"; filename=\"report.csv\""
            ));
            assert!(sent.contains("Content-Type: text/csv\r\n\r\nFILEDATA\r\n"));
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

    /// Mock that always returns an error — used for the expected_response
    /// transport-error regression test below.
    struct FailingHttpClient;

    impl HttpClient for FailingHttpClient {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send {
            async move { Err(anyhow::anyhow!("connection refused")) }
        }
    }

    /// Snapshot-helper: returns the set of stored trend keys that include
    /// `expected_response:<expected>` AND have the metric name prefix.
    fn stored_trend_keys_with_expected(
        metrics: &BuiltinMetrics,
        metric_name: &str,
        expected: bool,
    ) -> Vec<String> {
        let snap = metrics.registry.snapshot(1.0);
        let needle = format!("expected_response:{}", expected);
        snap.trends
            .iter()
            .filter_map(|(name, _)| {
                if name.starts_with(&format!("{}{{", metric_name)) && name.contains(&needle) {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// 2xx/3xx → expected_response:true. Regression-lock for the default
    /// expected-statuses semantics (matches upstream's [200..=399] band).
    /// Without the tag attach, no stored key carries `expected_response:true`
    /// and this assertion fails.
    #[tokio::test]
    async fn http_attaches_expected_response_true_for_2xx_3xx() {
        let handle = tokio::runtime::Handle::current();
        let metrics = BuiltinMetrics::new();
        let mh = metrics.clone();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                let _: i32 = ctx.eval("http.get('http://example.com').status").unwrap();
            });
        })
        .await
        .unwrap();

        let trues = stored_trend_keys_with_expected(&metrics, "http_req_duration", true);
        assert!(
            !trues.is_empty(),
            "stored http_req_duration must carry expected_response:true for status 200; \
             snapshot trends were: {:?}",
            metrics
                .registry
                .snapshot(1.0)
                .trends
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
        );
        let falses = stored_trend_keys_with_expected(&metrics, "http_req_duration", false);
        assert!(
            falses.is_empty(),
            "no http_req_duration key should carry expected_response:false for status 200"
        );
    }

    /// 4xx/5xx → expected_response:false. Locks the inverse case so a future
    /// refactor that flips the predicate or drops the success-path attach
    /// can't sneak past.
    #[tokio::test]
    async fn http_attaches_expected_response_false_for_4xx_5xx() {
        let handle = tokio::runtime::Handle::current();
        let metrics = BuiltinMetrics::new();
        let mh = metrics.clone();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(500, ""));
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                let _: i32 = ctx.eval("http.get('http://example.com').status").unwrap();
            });
        })
        .await
        .unwrap();

        let falses = stored_trend_keys_with_expected(&metrics, "http_req_duration", false);
        assert!(
            !falses.is_empty(),
            "stored http_req_duration must carry expected_response:false for status 500"
        );
        let trues = stored_trend_keys_with_expected(&metrics, "http_req_duration", true);
        assert!(
            trues.is_empty(),
            "no http_req_duration key should carry expected_response:true for status 500"
        );
    }

    /// Band boundaries for the unified `(200..=399)` expected-status
    /// predicate. Specifically locks the 1xx case (status 101 from a
    /// WebSocket-style upgrade): under the old `failed = status >= 400`
    /// derivation, 101 was incorrectly categorized as "expected" (true)
    /// and "not-failed" (false), diverging from upstream's
    /// `defaultExpectedStatuses.match(101) == false`. With the unified
    /// predicate, both `expected_response` and `http_req_failed` agree at
    /// every boundary.
    #[tokio::test]
    async fn http_expected_response_band_boundaries() {
        // (status, expected_response_value, http_req_failed_value)
        let cases: &[(u16, bool, bool)] = &[
            (101, false, true), // 1xx: NOT in [200..=399] — the bug case
            (199, false, true), // just below lower band edge
            (200, true, false), // lower band edge
            (399, true, false), // upper band edge
            (400, false, true), // just above upper band edge
            (500, false, true), // 5xx
        ];

        for (status, want_expected, want_failed) in cases {
            let handle = tokio::runtime::Handle::current();
            let metrics = BuiltinMetrics::new();
            let mh = metrics.clone();
            let status_v = *status;
            tokio::task::spawn_blocking(move || {
                let rt = runtime::create_runtime().unwrap();
                let ctx = runtime::create_context(&rt).unwrap();
                let client = Arc::new(MockHttpClient::new(status_v, ""));
                let bp = Backpressure::new(10);
                ctx.with(|ctx| {
                    register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                    let _: i32 = ctx.eval("http.get('http://example.com').status").unwrap();
                });
            })
            .await
            .unwrap();

            // expected_response tag: assert exactly one of true/false present
            // matching the expected value.
            let want_keys =
                stored_trend_keys_with_expected(&metrics, "http_req_duration", *want_expected);
            assert!(
                !want_keys.is_empty(),
                "status {status}: stored http_req_duration must carry \
                 expected_response:{want_expected} (unified band predicate)"
            );
            let wrong_keys =
                stored_trend_keys_with_expected(&metrics, "http_req_duration", !*want_expected);
            assert!(
                wrong_keys.is_empty(),
                "status {status}: no http_req_duration key should carry \
                 expected_response:{} (got {wrong_keys:?})",
                !*want_expected,
            );

            // http_req_failed rate: passes = failed-count, total = all
            // requests, rate = passes/total. The boundary bug shows up
            // here too — for status 101 the old predicate put it in the
            // success bucket (passes=0) when upstream's default callback
            // would put it in failures (passes=1).
            let snap = metrics.registry.snapshot(1.0);
            let (_, passes, total) = snap
                .rates
                .iter()
                .find(|(n, _, _, _)| n.starts_with("http_req_failed"))
                .map(|(_, r, p, t)| (*r, *p, *t))
                .expect("http_req_failed must be recorded for every request");
            assert_eq!(
                total, 1,
                "status {status}: exactly one request recorded, got total={total}"
            );
            assert_eq!(
                passes, *want_failed as u64,
                "status {status}: http_req_failed.passes (=count of failed) \
                 must be {} but was {passes}",
                *want_failed as u64,
            );
        }
    }

    #[tokio::test]
    async fn http_expected_statuses_validates_arguments() {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(200, ""));
            let bp = Backpressure::new(10);

            ctx.with(|ctx| {
                register(&ctx, handle, client, bp).unwrap();
                let valid: bool = ctx
                    .eval(
                        r#"
                        const es = http.expectedStatuses(200, 300, { min: 200, max: 399 });
                        es.__expectedStatuses.length === 3
                        "#,
                    )
                    .unwrap();
                assert!(valid);

                assert!(ctx.eval::<(), _>("http.expectedStatuses()").is_err());
                assert!(
                    ctx.eval::<(), _>("http.expectedStatuses(200, '300')")
                        .is_err()
                );
                assert!(ctx.eval::<(), _>("http.expectedStatuses(200.5)").is_err());
                assert!(
                    ctx.eval::<(), _>("http.expectedStatuses({ min: 200, max: 300.5 })")
                        .is_err()
                );
            });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn http_response_callback_overrides_expected_statuses() {
        let handle = tokio::runtime::Handle::current();
        let metrics = BuiltinMetrics::new();
        let mh = metrics.clone();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(302, ""));
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                let _: i32 = ctx
                    .eval(
                        r#"
                        http.setResponseCallback(http.expectedStatuses(200));
                        http.get('http://example.com/redirect').status
                        "#,
                    )
                    .unwrap();
            });
        })
        .await
        .unwrap();

        let falses = stored_trend_keys_with_expected(&metrics, "http_req_duration", false);
        assert!(
            !falses.is_empty(),
            "status 302 should be unexpected when callback only allows 200"
        );
        let (_, passes, total) = metrics.registry.rate_get("http_req_failed");
        assert_eq!(total, 1);
        assert_eq!(passes, 1, "unexpected response should count as failed");
    }

    #[tokio::test]
    async fn http_response_callback_null_disables_failed_metric() {
        let handle = tokio::runtime::Handle::current();
        let metrics = BuiltinMetrics::new();
        let mh = metrics.clone();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(MockHttpClient::new(500, ""));
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                let _: i32 = ctx
                    .eval(
                        r#"
                        http.setResponseCallback(null);
                        http.get('http://example.com/fail').status
                        "#,
                    )
                    .unwrap();
            });
        })
        .await
        .unwrap();

        assert!(stored_trend_keys_with_expected(&metrics, "http_req_duration", true).is_empty());
        assert!(stored_trend_keys_with_expected(&metrics, "http_req_duration", false).is_empty());
        let (_, _passes, total) = metrics.registry.rate_get("http_req_failed");
        assert_eq!(
            total, 0,
            "responseCallback:null should skip http_req_failed emission"
        );
        assert_eq!(metrics.registry.counter_get("http_reqs"), 1);
    }

    /// Transport error → expected_response:false. Status is "0" (sentinel)
    /// which is outside [200..=399], so the tag must be false on this path
    /// too. Without the error-branch attach this fails because the stored
    /// key has no `expected_response:` segment at all.
    #[tokio::test]
    async fn http_attaches_expected_response_false_on_transport_error() {
        let handle = tokio::runtime::Handle::current();
        let metrics = BuiltinMetrics::new();
        let mh = metrics.clone();
        tokio::task::spawn_blocking(move || {
            let rt = runtime::create_runtime().unwrap();
            let ctx = runtime::create_context(&rt).unwrap();
            let client = Arc::new(FailingHttpClient);
            let bp = Backpressure::new(10);
            ctx.with(|ctx| {
                register_with_metrics(&ctx, handle, client, bp, Some(mh)).unwrap();
                // The script must not throw — http.get returns an error
                // response object, not a JS exception.
                let status: i32 = ctx.eval("http.get('http://example.com').status").unwrap();
                assert_eq!(status, 0, "transport error must surface status=0");
            });
        })
        .await
        .unwrap();

        let falses = stored_trend_keys_with_expected(&metrics, "http_req_duration", false);
        assert!(
            !falses.is_empty(),
            "transport-error path must attach expected_response:false"
        );
    }
}
