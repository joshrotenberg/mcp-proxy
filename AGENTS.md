# AGENTS.md

## Overview

mcp-proxy is a config-driven MCP (Model Context Protocol) reverse proxy built in Rust. It aggregates multiple MCP backends behind a single HTTP endpoint with per-backend middleware, authentication, and observability. Built on tower-mcp and the tower middleware ecosystem.

Package name: `mcp-proxy`. Binary name: `mcp-proxy`. Library name: `mcp_proxy`.

## Project structure

```
src/
  main.rs          # CLI entry point (clap), logging setup
  lib.rs           # Library root, re-exports Proxy and ProxyConfig
  proxy.rs         # Core: builds proxy, middleware stack, and axum router
  config.rs        # TOML config parsing, all config types
  admin.rs         # HTTP admin API (/admin/backends, /admin/health, etc.)
  admin_tools.rs   # MCP admin tools (proxy/ namespace, via ChannelTransport)
  reload.rs        # Hot reload: file watcher, dynamic backend addition
  test_util.rs     # MockService, ErrorMockService, call_service helper

  # Global middleware (wraps the entire proxy, applied in proxy.rs)
  alias.rs         # Tool renaming (AliasService)
  filter.rs        # Capability filtering -- allow/deny lists (CapabilityFilterService)
  inject.rs        # Argument injection into tool calls (InjectArgsService)
  mirror.rs        # Traffic mirroring to canary backends (MirrorService)
  cache.rs         # Response caching with TTL (CacheService)
  coalesce.rs      # Request deduplication (CoalesceService)
  validation.rs    # Argument size limits (ValidationService)
  metrics.rs       # Prometheus counters and histograms (MetricsService)
  rbac.rs          # Role-based access control (RbacService)
  token.rs         # Auth token passthrough to backends (TokenPassthroughService)

  # Per-backend middleware (applied per-backend in proxy.rs and reload.rs)
  retry.rs         # Retry with exponential backoff (McpRetryPolicy)
  outlier.rs       # Outlier detection and ejection (OutlierDetectionLayer)

tests/
  integration.rs   # End-to-end tests with in-process backends via ChannelTransport

examples/
  *.toml           # Example proxy configs for different deployment patterns
  docker-compose/  # Docker compose example with HTTP backend
```

## Architecture

### Middleware stack ordering

The middleware stack is built in `proxy.rs::build_middleware_stack()`. Order matters -- outermost runs first.

**Global middleware** (wraps the entire proxy service):
```
Request flow (outer to inner):
Auth (axum layer) -> Audit -> Metrics -> Token Passthrough -> RBAC
  -> Alias -> Filter -> Validation -> Coalesce -> Cache
  -> Mirror -> Inject Args -> McpProxy
```

**Per-backend middleware** (applied to each backend individually):
```
Request flow (inner to outer, applied via builder.backend_layer()):
Retry -> Hedge -> Concurrency Limit -> Rate Limit
  -> Timeout -> Circuit Breaker -> Outlier Detection -> Backend
```

### Key design pattern: Error = Infallible

All services use `Error = Infallible`. Errors are represented inside the response:
`RouterResponse { id: RequestId, inner: Result<McpResponse, JsonRpcError> }`.

tower-resilience and tower middleware produce typed errors (e.g., `CircuitBreakerError<E>`).
These are converted to `RouterResponse` error values via `tower_mcp::CatchError` wrapper.
The pattern in `reload.rs` is:
```rust
let limited = tower::Layer::layer(&layer, svc);
svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
```

In `proxy.rs`, `builder.backend_layer(layer)` handles this internally.

### Adding a new middleware layer

Every middleware follows the same tower Service pattern. Use `inject.rs` as a template:

1. Create `src/your_middleware.rs` with:
   - A service struct wrapping an inner service: `YourService<S> { inner: S, config: ... }`
   - `impl<S> Service<RouterRequest> for YourService<S>` with `Response = RouterResponse`, `Error = Infallible`
   - Match on `req.inner` to inspect the MCP request type (ListTools, CallTool, etc.)
   - Unit tests using `MockService` and `call_service` from `test_util`

2. Add `pub mod your_middleware;` to `lib.rs`

3. Wire it into the stack in `proxy.rs::build_middleware_stack()`:
   - For global middleware: wrap the `BoxCloneService` at the appropriate position
   - For per-backend: use `builder.backend_layer(layer)` in the backend loop

4. Add config fields to `config.rs` (with `#[serde(default)]` for optional fields)

5. Wire the hot reload path in `reload.rs::build_backend_layer()` (per-backend only)

6. Add integration tests in `tests/integration.rs`

### Adding a new backend transport

Transports are added in `proxy.rs::build_mcp_proxy()` and `reload.rs::add_backend()`:

1. Add a variant to `TransportType` enum in `config.rs`
2. Add a match arm in both `build_mcp_proxy()` and `add_backend()`
3. The transport must implement tower-mcp's `ClientTransport` trait

### Configuration

All config is in `config.rs`. The top-level type is `ProxyConfig`:
- `proxy`: name, version, separator, listen address, hot_reload
- `backends[]`: name, transport, command/url, per-backend middleware configs
- `auth`: bearer tokens or JWT/JWKS with RBAC
- `performance`: request coalescing
- `security`: argument size limits
- `observability`: logging, metrics, tracing

Config maps directly to runtime: each optional config section (timeout, rate_limit, circuit_breaker, etc.) gates whether that middleware layer is applied.

Environment variable substitution: `${VAR_NAME}` in string values is resolved via `config.resolve_env_vars()`.

### Testing

**Unit tests** (`#[cfg(test)]` in each module):
- Use `MockService::with_tools(&["tool1", "tool2"])` for a service that returns tool lists and echoes call results
- Use `ErrorMockService` for a service that returns JSON-RPC errors
- Use `call_service(&mut svc, McpRequest::...)` to send requests through the service

**Integration tests** (`tests/integration.rs`):
- Use `ChannelTransport` to create in-process MCP backends (no external processes)
- Build an `McpRouter` with tools, wrap in `ChannelTransport`, add to `McpProxy`
- Test middleware composition end-to-end

**CI checks** (run before committing):
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo doc --no-deps --all-features
```

## Conventions

- Rust 2024 edition, MSRV 1.90
- `anyhow` for application errors, `thiserror` for library errors
- Conventional commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`
- All public APIs have doc comments
- Metrics use `mcp_proxy_` prefix
- Admin tools live under `proxy/` namespace
