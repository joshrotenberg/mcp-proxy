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
  config.rs        # TOML/YAML config parsing, validation, all config types
  builder.rs       # Fluent programmatic configuration builder
  discovery.rs     # Search index and complete tool schema store
  mcp_json.rs      # Import Claude-style .mcp.json configurations
  ws_transport.rs  # Outbound WebSocket transport
  skills.rs        # Management prompts bundled from src/skills/
  admin.rs         # HTTP admin API (/admin/backends, /admin/health, etc.)
  admin_tools.rs   # MCP admin tools (proxy/ namespace, via ChannelTransport)
  reload.rs        # Hot reload: file watcher, dynamic backend addition
  test_util.rs     # MockService, ErrorMockService, call_service helper

  # Global middleware (wraps the entire proxy, applied in proxy.rs)
  access_log.rs    # Structured access logging (AccessLogService, target: mcp::access)
  alias.rs         # Tool renaming (AliasService)
  filter.rs        # Capability filtering -- allow/deny lists, glob patterns (CapabilityFilterService)
  inject.rs        # Argument injection into tool calls (InjectArgsService)
  mirror.rs        # Traffic mirroring to canary backends (MirrorService)
  cache.rs         # Response caching with memory, Redis, or SQLite storage
  coalesce.rs      # Request deduplication (CoalesceService)
  validation.rs    # Argument size limits (ValidationService)
  metrics.rs       # Prometheus counters and histograms (MetricsService)
  rbac.rs          # Role-based access control (RbacService)
  token.rs         # Auth token passthrough to backends (TokenPassthroughService)
  bearer_scope.rs  # Per-token tool scoping and claims
  introspection.rs # OAuth token introspection and JWT fallback
  param_override.rs # Tool parameter hiding, renaming, and defaults
  composite.rs     # Composite tool fan-out
  canary.rs        # Weighted canary routing
  failover.rs      # Ordered backend failover

  # Per-backend middleware (applied per-backend in proxy.rs and reload.rs)
  retry.rs         # tower-resilience retry layer, exponential backoff, budgets
  outlier.rs       # Outlier detection and ejection (OutlierDetectionLayer)

tests/
  integration.rs   # Middleware composition tests with in-process backends via ChannelTransport
  e2e.rs           # Full proxy pipeline tests with in-process backends
  http_e2e.rs      # Real HTTP transport and authentication tests
  health_probes.rs # Configured proxy process probes and auth boundaries
  outbound_headers.rs # Actual HTTP requests and WebSocket handshake headers
  discovery_schemas.rs # Tool schema retrieval and reindex behavior
  chaos.rs         # Fault injection and resilience recovery regressions
  cache_backends.rs # Real Redis containers and temporary SQLite databases
  config_properties.rs # Property tests for config validation
  examples.rs      # Load and validate every example TOML config

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
Auth (axum layer) -> Global Rate Limit -> Audit -> Access Log -> Metrics
  -> Token Passthrough -> Token Validation -> RBAC -> Bearer Scoping -> Composite -> Alias
  -> Search Mode Filter -> Capability Filter -> Validation -> Coalesce -> Cache
  -> Mirror -> Failover -> Canary -> Parameter Overrides -> Inject Args -> McpProxy
```

**Per-backend middleware** (applied to each backend individually):
```
Request flow (outer to inner):
Outlier Detection -> CatchError -> Circuit Breaker -> Timeout -> Rate Limit
  -> Concurrency Limit -> Hedge -> Retry -> Backend
```

### Key design pattern: Error = Infallible

MCP-facing services use `Error = Infallible`. Errors are represented inside the response:
`RouterResponse { id: RequestId, inner: Result<McpResponse, JsonRpcError> }`.

tower-resilience and tower middleware produce typed errors (e.g., `CircuitBreakerError<E>`).
These are converted to `RouterResponse` error values via `tower_mcp::CatchError` wrapper.
The pattern in `reload.rs` is:
```rust
let limited = tower::Layer::layer(&layer, svc);
svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
```

`BackendMiddlewareLayer` builds retry in the Infallible zone, then
hedge/concurrency/rate/timeout/breaker in the BoxError zone, converts errors back into
responses with `CatchError`, and applies outlier detection outside that boundary.
Pass the composed layer to `builder.backend_layer()` once. The reload path
constructs the same zones; changing either path requires checking the other.

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

Every feature gets tests. No exceptions.

**Unit tests** (`#[cfg(test)]` in each module):
- Use `MockService::with_tools(&["tool1", "tool2"])` for a service that returns tool lists and echoes call results
- Use `ErrorMockService` for a service that returns JSON-RPC errors
- Use `call_service(&mut svc, McpRequest::...)` to send requests through the service

**E2E tests** (`tests/e2e.rs`):
- Comprehensive test suite covering full proxy pipeline
- Use `ChannelTransport` to create in-process MCP backends (no external processes)
- Build an `McpProxy` with tools, compose middleware, verify end-to-end behavior
- Includes error backends, slow backends, concurrent request testing
- Use `tower-resilience-chaos` for chaos engineering / failure injection tests

**Integration tests** (`tests/integration.rs`):
- Middleware composition tests with real proxy routing

### Documentation

Rust docs are the single source of truth for both CLI users and library users.
The library enables `missing_docs`; CI treats its warnings as errors.

- All public APIs must have doc comments
- Module-level docs explain purpose and usage patterns
- Doc examples should be runnable (`cargo test --doc`)
- Keep examples up to date -- stale doc examples break `cargo test --doc`

**Pre-push checks** (MUST pass before every push):
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib --all-features
cargo test --test '*' --all-features
RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps --all-features # missing/broken docs
cargo test --doc --all-features          # catches stale doc examples
```

## Conventions

- Rust 2024 edition, MSRV 1.90
- `anyhow` for application errors, `thiserror` for library errors
- Conventional commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`
- All public APIs have doc comments
- Metrics use `mcp_proxy_` prefix
- Admin tools live under `proxy/` namespace

## Compatibility, coverage, and deployment checks

See `CONTRIBUTING.md` for the full Rust 1.90 feature matrix and coverage commands.
All-feature integration tests require Docker and `redis:7-alpine`; startup or
readiness failures are test failures. All Rust examples are checked, and the
outlier recovery regression runs in the normal suite.

CI publishes a `coverage-report` artifact and validates the Helm chart with
`./scripts/check-chart.sh`. Release preparation must synchronize
`charts/mcp-proxy/Chart.yaml` appVersion with Cargo.toml and bump the chart
version. `/livez` and `/readyz` are credential-free process probes; detailed
backend health is served by the authenticated `/admin/health` endpoint.
