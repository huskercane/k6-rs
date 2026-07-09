# Hyper HTTP Client Parity Plan

This plan tracks the work to bring `HyperHttpClient` to parity with the
current `ReqwestHttpClient` implementation before switching the default HTTP
backend.

## Phase 0: Lock Baseline

Status: completed in this branch.

Create a small parity checklist in repo docs and add paired tests that run the
same `HttpRequest` through reqwest and hyper where possible. Keep reqwest as
default until every reqwest-supported config path has hyper tests.

## Phase 1: Low-Risk Config Parity

Status: completed in this branch.

1. Request timeout: honor `HttpRequest.timeout` and default 30s timeout around
   DNS, connect, send, and body drain.
2. `noConnectionReuse`: bypass the pool and send/close each request.
3. `userAgent`: use configured `userAgent`, but allow explicit `User-Agent`
   request headers to override it.
4. `httpDebug`: mirror reqwest request/response logging.

## Phase 2: Routing and Blocking

Status: completed in this branch.

5. `blockHostnames`: port the existing hostname matcher into hyper preflight.
6. `blacklistIPs`: block literal IPs first, then resolved IPs after DNS lookup.
7. `hosts`: implement static hostname to IP mapping while preserving the
   original Host header and route identity semantics.

## Phase 3: Response Semantics

Status: completed in this branch.

8. Redirects: implement `maxRedirects`, including `0` as disabled, with final
   URL preserved in `HttpResponse.url`.
9. Error behavior: normalize hyper errors so JS classification remains stable
   for DNS, TLS, timeout, refused, reset, and blocked cases.
10. Connection close handling: detect `Connection: close` and avoid re-pooling
    instead of letting the next send fail.

## Phase 4: Network Feature Parity

Status: in progress. `localIPs` source binding and plain HTTP proxy routing
are completed in this branch.

11. `localIPs`: build source-IP round-robin into `RouteKey.source_ip` and bind
    sockets before connect.
12. Proxy support: support `HTTP_PROXY` / `HTTPS_PROXY` parity for plain HTTP
    first, then CONNECT for HTTPS.
13. HTTPS/TLS: add rustls client connections, `insecureSkipTLSVerify`, TLS
    min/max config, SNI, and `tls_handshaking` timings.
14. HTTP/2: add after HTTPS is stable, since reqwest may negotiate it.

## Phase 5: Default Switch

15. Run `cargo test --workspace`.
16. Run conformance with hyper and reqwest paths.
17. Add a temporary escape hatch such as `K6RS_HTTP_CLIENT=reqwest`.
18. Switch default to hyper only after HTTP, HTTPS, config, and conformance
    scenarios pass.

## Implementation Notes

Implement in this order because Phase 1 is isolated and testable, while
`hosts`, proxy, local IP binding, and TLS affect connection identity and
pooling.
