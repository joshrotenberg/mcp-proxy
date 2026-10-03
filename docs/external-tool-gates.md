# External decision gates for selected tool calls

An external decision service can be integrated as a reusable Tower layer over
`tower_mcp::RouterRequest` and `RouterResponse`. This is the interception point
for a non-production pilot such as the one described in issue #242. The adapter
belongs in the embedding application or a reusable tower-mcp middleware crate;
mcp-proxy does not need a vendor-specific endpoint or decision format in its
configuration.

## Where to attach the layer

Implement `tower::Layer` and `tower::Service<RouterRequest>`, following the
service pattern in [`src/inject.rs`](../src/inject.rs). Match
`McpRequest::CallTool` and inspect `params.name` and `params.arguments`.
Pass other MCP methods and tools outside the selected set through unchanged.

For an application using `tower_mcp::proxy::McpProxy`, attach the layer with
`McpProxy::builder().backend(...).await.backend_layer(gate)` after registering
the selected backend. Dynamic backends have an equivalent
`McpProxy::add_backend_with_layer` API. A per-backend layer already knows its
backend identity; tools are routed with their local names at this boundary.

In mcp-proxy's configured stack, `src/proxy.rs::build_middleware_stack` applies
local authentication, RBAC, filtering, and validation before routing to the
per-backend services. `BackendMiddlewareLayer` in that file is the composition
point for a custom per-backend gate. The corresponding hot-reload construction
is `src/reload.rs::build_backend_layer`; update both paths when maintaining a
custom integration.

`Proxy::mcp_proxy()` exposes the underlying routing service. It does not include
the configured global middleware or HTTP authentication layer. An embedding
application using that accessor must compose its authorization and policy
layers explicitly before serving requests. Wrapping the accessor alone does
not preserve the policy stack of `Proxy::into_router()`.

## Decision and execution flow

1. Select one non-production backend and one or two tool names. Apply the
   existing local authorization checks before consulting the external gate.
2. Bound the decision request by both a deadline and a payload size limit.
   Include the backend, tool, and agreed caller context, with normalized
   arguments or an argument hash as required by the pilot. Derive caller
   identity from validated request extensions. Do not forward credentials.
3. Interpret `ALLOW`, `REVIEW`, and `BLOCK` explicitly. Only `ALLOW` dispatches
   the original tool call. Unknown decisions, malformed receipts, decision
   timeouts, and endpoint failures stop the call in this pilot. `REVIEW` needs
   an application-owned review workflow before a later execution.
4. Keep the receipt ID in the request's execution context. After the allowed
   call completes, correlate the result or JSON-RPC error with that receipt.
   Bound outcome reporting independently and avoid executing a successful
   tool again merely because its outcome report failed.

mcp-proxy services use `Error = Infallible`: return blocked decisions as
`RouterResponse.inner = Err(JsonRpcError)` while preserving the request ID.
An asynchronous decision must still respect Tower readiness. Move the inner
service that was polled ready into the call future (for a cloneable service,
replace it with a clone), or use `ServiceExt::oneshot` on a clone to obtain
readiness before dispatch. Do not send a tool call before `ALLOW` arrives.

## Placement and pilot checks

Decide whether a receipt authorizes one logical client request or one backend
attempt. Retries, hedges, failover, mirroring, caching, and coalescing can change
how many backend executions one client request produces. For the first pilot,
disable these options for the selected tools, then test each interaction before
enabling it. An authorization decision must not be reused through a response
cache unless its caller, arguments, and expiry semantics permit that reuse.

Use a local decision stub and an execution counter to verify that `ALLOW`
executes exactly once and that `BLOCK`, `REVIEW`, timeout, malformed responses,
and endpoint failures execute zero times. Verify result/error correlation,
cancellation, and that unrelated tools and MCP methods continue to work.
A real pilot additionally requires an agreed endpoint, receipt verification
contract, and test backend; this guide does not configure an external service.
