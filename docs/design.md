# mcp-gateway Design Document

## Overview

mcp-gateway is a standalone, config-driven MCP proxy server built on tower-mcp.
It aggregates multiple MCP backend servers behind a single HTTP endpoint,
adding auth, resilience, observability, and routing capabilities.

**Goals:**
- Zero-code gateway: all behavior driven by TOML configuration
- Stress-test and validate tower-mcp's proxy and transport primitives
- Production-grade defaults with minimal config, full control when needed

**Non-goals:**
- Custom business logic (use tower-mcp directly for that)
- Plugin system or scripting (may revisit later)

## Architecture

```
Clients (Claude, IDEs, custom)
    |
    v
[HTTP Transport]  <-- inbound auth, CORS, session management
    |
    v
[Middleware Stack]  <-- audit, rate limiting, request validation
    |
    v
[McpProxy]  <-- namespace routing, tool/resource/prompt aggregation
    |
    +---> [Backend A: stdio]  <-- per-backend resilience (timeout, circuit breaker)
    +---> [Backend B: stdio]
    +---> [Backend C: http]   <-- (future: HTTP backend transport)
```

Key tower-mcp primitives used:
- `McpProxy` -- multi-backend aggregation with namespace prefixing
- `HttpTransport::from_service()` -- serve any Service<RouterRequest> over HTTP
- `StdioClientTransport::spawn_command()` -- subprocess backends with env vars
- `AuditLayer` -- structured audit logging
- tower middleware -- timeout, rate limiting, circuit breakers

## Feature Areas

### 1. Authentication and Authorization

**Priority: High**

#### 1.1 Inbound Authentication

Validate client requests before they reach backends.

```toml
[auth]
type = "bearer"
tokens = ["token-1", "token-2"]

# OR

[auth]
type = "jwt"
issuer = "https://auth.example.com"
audience = "mcp-gateway"
jwks_uri = "https://auth.example.com/.well-known/jwks.json"
```

**Status:** Done. Wired to tower-mcp's AuthLayer (bearer) and OAuthLayer (JWT/JWKS).

#### 1.2 Outbound Credential Injection

Pass credentials to backends (API keys, tokens).

```toml
[[backends]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[backends.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}"
```

**Status:** Done. Env vars passed to `Command` via `spawn_command()` (PR #624).

#### 1.3 Role-Based Access Control (RBAC)

Restrict which tools/resources/prompts a client can access based on auth context.

```toml
[[auth.roles]]
name = "reader"
# Only allow tools with read_only_hint = true
allow_tools = { read_only = true }
# Or explicit allowlist
allow_tools = ["files/read_file", "files/list_directory"]
deny_tools = ["github/create_issue"]

[[auth.roles]]
name = "admin"
allow_tools = "*"

# Map JWT claims to roles
[auth.role_mapping]
claim = "scope"
mapping = { "mcp:read" = "reader", "mcp:admin" = "admin" }
```

**Status:** Not started. Requires:
- Claim-to-role mapping logic
- Per-request role resolution from auth context
- Composes with static capability filtering: static filters are the floor,
  RBAC further restricts per-request. Two stacked middleware layers.

#### 1.4 Token Passthrough

Forward client credentials to backends (e.g., user's OAuth token to GitHub backend).

```toml
[[backends]]
name = "github"
# Forward the client's Authorization header as GITHUB_TOKEN env var
# (only meaningful for HTTP backends or via request context)
token_passthrough = true
```

**Status:** Not started. Complex -- requires per-request credential propagation.
Defer to later phase.

### 2. Routing and Request Management

**Priority: High**

#### 2.1 Namespace Routing (Current)

All tools/resources/prompts are prefixed with backend name.

```toml
[gateway]
separator = "/"  # files/read_file, github/create_issue
```

**Status:** Working via McpProxy.

#### 2.2 Capability Filtering

Hide specific tools/resources/prompts from clients.

```toml
[[backends]]
name = "files"
# Only expose these tools (allowlist)
expose_tools = ["read_file", "list_directory"]
# Or hide specific ones (denylist)
hide_tools = ["write_file", "delete_file"]
# Same for resources and prompts
hide_resources = ["file:///etc/shadow"]
```

**Status:** Done. Implemented as `CapabilityFilterService` middleware wrapping `McpProxy`.
Filters list responses (tools/resources/prompts) and rejects calls to filtered capabilities.
Supports per-backend `expose_tools`/`hide_tools`, `expose_resources`/`hide_resources`,
`expose_prompts`/`hide_prompts`. Validation rejects conflicting allow+deny on same dimension.

#### 2.3 Tool Aliasing and Rewriting

Rename or reorganize tools across backends.

```toml
[[backends.aliases]]
from = "read_file"        # backend's tool name
to = "read"               # exposed as files/read

# Or merge tools from multiple backends under a virtual namespace
[[virtual_namespaces]]
name = "search"
tools = [
    { backend = "github", tool = "search_code", as = "code" },
    { backend = "files", tool = "search_files", as = "files" },
]
```

**Status:** Done. `AliasService` middleware rewrites tool/resource/prompt names in both
directions: list responses (forward map) and call/read/get requests (reverse map).
Per-backend `[[backends.aliases]]` with `from`/`to` fields. Virtual namespaces not yet
implemented (would require cross-backend routing).

#### 2.4 Content-Based Routing

Route requests based on tool annotations or request content.

```toml
[routing]
# Route all destructive operations through a specific backend
destructive_backend = "audited-executor"
# Route based on resource URI scheme
resource_routes = [
    { scheme = "file", backend = "files" },
    { scheme = "https", backend = "web" },
]
```

**Status:** Not started. Interesting but complex. Defer.

### 3. Resilience

**Priority: High**

#### 3.1 Per-Backend Timeout

```toml
[[backends]]
name = "slow-backend"
[backends.timeout]
seconds = 60
```

**Status:** Done. TimeoutLayer wired per-backend from config.

#### 3.2 Circuit Breaker

Trip open after consecutive failures, allow recovery.

```toml
[[backends]]
name = "flaky-backend"
[backends.circuit_breaker]
failure_threshold = 5       # failures before opening
success_threshold = 2       # successes in half-open before closing
timeout_seconds = 30        # how long to stay open before half-open
```

**Status:** Done. CircuitBreakerLayer wired per-backend from config.

#### 3.3 Rate Limiting

Per-backend or global request rate limits.

```toml
# Global rate limit
[rate_limit]
requests_per_second = 100

# Per-backend rate limit
[[backends]]
name = "expensive-api"
[backends.rate_limit]
requests_per_second = 10
burst = 20
```

**Status:** Done. RateLimiterLayer wired per-backend from config.

#### 3.4 Retry

Retry failed requests (only for idempotent operations).

```toml
[[backends]]
name = "unreliable"
[backends.retry]
max_attempts = 3
backoff_ms = [100, 500, 2000]  # exponential backoff
# Only retry idempotent tools (uses tool annotations)
idempotent_only = true
```

**Status:** Not started. tower has RetryLayer. Need policy that
respects tool annotations.

**Note:** Tool annotations are self-reported by backends, so a misbehaving backend
could mark a destructive tool as idempotent. Consider adding a config-level
`retry_tools` allowlist that overrides annotation trust.

#### 3.5 Bulkheading

Isolate backends so one slow/failing backend doesn't consume
all resources.

```toml
[[backends]]
name = "isolated"
[backends.concurrency]
max_concurrent = 10
```

**Status:** Done. ConcurrencyLimitLayer wired per-backend from config.

### 4. Observability

**Priority: High**

#### 4.1 Audit Logging

Structured logging of all MCP operations.

```toml
[observability]
audit = true
log_level = "info"
json_logs = false
```

**Status:** Done. AuditLayer wraps proxy before `from_service()`.
Uses `CatchError` to maintain `Error = Infallible` contract.

#### 4.2 Metrics

Expose Prometheus-compatible metrics.

```toml
[observability.metrics]
enabled = true
endpoint = "/metrics"     # Prometheus scrape endpoint
# Metrics emitted:
# - mcp_gateway_requests_total{backend, method, status}
# - mcp_gateway_request_duration_seconds{backend, method}
# - mcp_gateway_backend_health{backend}
# - mcp_gateway_active_sessions
```

**Status:** Done. `MetricsService` middleware records `mcp_gateway_requests_total{method, status}`
(counter) and `mcp_gateway_request_duration_seconds{method}` (histogram).
Uses `metrics` + `metrics-exporter-prometheus`. Prometheus scrape endpoint at `/admin/metrics`.
Enabled via `[observability.metrics] enabled = true`.

#### 4.3 Distributed Tracing

OpenTelemetry trace propagation.

```toml
[observability.tracing]
enabled = true
exporter = "otlp"         # or "jaeger", "zipkin"
endpoint = "http://localhost:4317"
service_name = "mcp-gateway"
```

**Status:** Done. When `[observability.tracing] enabled = true`, bridges all `tracing` spans
(including tower-mcp's `McpTracingLayer` spans) to OpenTelemetry via `tracing-opentelemetry`.
Exports traces via OTLP HTTP to a configurable endpoint (default: `http://localhost:4317`).
Uses `opentelemetry_sdk` with batch span processor.

#### 4.4 Health Endpoint

```toml
[observability.health]
endpoint = "/health"       # already available via HttpTransport
detailed = true            # include per-backend health
```

**Status:** `McpProxy::health_check()` returns per-backend health (namespace + healthy).
Called at startup, logged per-backend. HttpTransport provides `/health` endpoint.
TODO: expose detailed per-backend health via admin API.

### 5. Performance

**Priority: Medium**

#### 5.1 Connection Pooling (HTTP Backends)

```toml
[[backends]]
name = "remote-server"
transport = "http"
url = "http://mcp-server:8080"
[backends.pool]
max_connections = 10
idle_timeout_seconds = 60
```

**Status:** Done. `HttpClientTransport::new(url)` wired for `transport = "http"` backends.
Per-backend middleware (timeout, circuit breaker, rate limit, concurrency) applies to HTTP
backends the same as stdio. Requires `http-client` feature on tower-mcp.

#### 5.2 Request Coalescing

Deduplicate identical in-flight tool calls.

```toml
[performance]
coalesce_requests = true   # deduplicate identical concurrent tool calls
```

**Status:** Done. `CoalesceService` middleware deduplicates identical in-flight
`CallTool` and `ReadResource` requests. Uses `tokio::sync::broadcast` to share
results with waiting callers. Enabled via `[performance] coalesce_requests = true`.

### 6. Caching

**Priority: Medium**

#### 6.1 Resource Caching

Cache resource reads with configurable TTL.

```toml
[cache]
enabled = true
max_entries = 1000

# Per-backend cache policy
[[backends]]
name = "files"
[backends.cache]
resource_ttl_seconds = 300   # cache resource reads for 5 minutes
# Invalidate on notifications/resources/updated from backend
invalidate_on_notify = true
```

**Status:** Done. `CacheService` middleware with per-backend `resource_ttl_seconds` and
`tool_ttl_seconds`. Uses `moka` async cache with LRU eviction and TTL expiry.
Cache key = request type + name/URI + serialized arguments.

**Note:** Cache invalidation via backend notifications is single-process only.
Multiple gateway instances would not share invalidation signals. For multi-instance
deployments, would need a shared cache bus (e.g., Redis pub/sub).
Notification-based invalidation not yet implemented.

#### 6.2 Tool Result Caching

Cache results of idempotent tool calls.

```toml
[[backends]]
name = "lookup-service"
[backends.cache]
# Only cache tools marked idempotent_hint = true
tool_ttl_seconds = 60
# Cache key: tool name + serialized arguments
```

**Status:** Done (TTL-based). Tool call results cached when `tool_ttl_seconds > 0` in
per-backend `[backends.cache]`. Cache key includes tool name + serialized arguments.
Annotation-based caching (only cache `idempotent_hint = true`) not yet implemented.

### 7. Security

**Priority: Medium**

#### 7.1 Request Validation

Validate tool arguments against schemas before forwarding.

```toml
[security]
validate_arguments = true    # validate tool call args against JSON schema
max_argument_size = "1MB"    # reject oversized arguments
```

**Status:** Done (argument size). `[security].max_argument_size` validates tool call
argument size before forwarding. Schema validation not yet implemented.

#### 7.2 Response Filtering

Filter or redact sensitive data in responses.

```toml
[security]
max_response_size = "10MB"
# PII redaction patterns
redact_patterns = ["\\b\\d{3}-\\d{2}-\\d{4}\\b"]  # SSN pattern
```

**Status:** Not started. Would need a response filtering middleware. Lower priority.

### 8. Operational

**Priority: Low (initial), Medium (production)**

#### 8.1 Hot Reload

Watch config file for changes, apply without restart.

```toml
[gateway]
hot_reload = true
watch_interval_seconds = 5
```

**Status:** Done (add-only). Watches config file via `notify` and adds new backends
dynamically using `McpProxy::add_backend()` (tower-mcp #628). Removed or modified
backends are logged as warnings since the proxy currently supports add-only updates.

**Limitations:**
- Per-backend middleware (timeout, circuit_breaker, rate_limit, concurrency) is not
  applied to hot-reloaded backends. Proxy-level middleware still applies.
- Backend removal and modification require a full restart.

#### 8.2 Admin API

Runtime introspection and control.

```toml
[admin]
enabled = true
endpoint = "/admin"
# Endpoints:
# GET /admin/backends      - list backends and health
# GET /admin/sessions      - list active sessions
# GET /admin/cache/stats   - cache hit/miss rates
# POST /admin/cache/clear  - clear cache
# POST /admin/reload       - trigger config reload
```

**Status:** Partial. `GET /admin/backends` returns per-backend health, gateway info, and
active session count (via `SessionHandle` from tower-mcp #629).
`GET /admin/metrics` returns Prometheus metrics. Health checks run on a dedicated thread
(workaround for `McpProxy: !Sync`). Cache stats and reload are future work.

#### 8.3 Graceful Shutdown

Drain in-flight requests before stopping.

```toml
[gateway]
shutdown_timeout_seconds = 30
```

**Status:** Done. Listens for SIGTERM and SIGINT (Ctrl-C). Configurable drain timeout
via `gateway.shutdown_timeout_seconds` (default: 30). Uses axum's `with_graceful_shutdown`.

## Implementation Phases

### Phase 1: Core (MVP) -- DONE

Get a working, useful gateway with the basics.

1. **Inbound auth** -- bearer + JWT/JWKS via AuthLayer/OAuthLayer
2. **Outbound env vars** -- pass env to spawn_command
3. **Per-backend resilience** -- timeout, circuit breaker, rate limit, concurrency limit
4. **Audit logging** -- AuditLayer + CatchError wrapping proxy
5. **Structured logging** -- JSON output, configurable levels
6. **Health endpoint** -- per-backend health via McpProxy::health_check()

### Phase 2: Access Control -- DONE

Fine-grained control over what clients can do.

7. **Capability filtering** -- CapabilityFilterService middleware with per-backend allow/deny lists
8. **RBAC** -- RbacService middleware, maps JWT claims to roles with per-role tool allow/deny
9. **Request validation** -- ValidationService, argument size limits via `[security].max_argument_size`

### Phase 3: Observability -- DONE

Production monitoring.

10. **Metrics** -- MetricsService middleware + Prometheus endpoint at `/admin/metrics`
11. **Distributed tracing** -- tracing-opentelemetry bridge + OTLP HTTP export
12. **Admin API** -- `/admin/backends` with cached per-backend health

### Phase 4: Performance and Caching -- DONE

Optimization for high-traffic deployments.

13. **Resource caching** -- CacheService with per-backend TTL via moka
14. **Tool result caching** -- TTL-based, keyed on tool name + arguments
15. **HTTP backends** -- HttpClientTransport wired for `transport = "http"`
16. **Graceful shutdown** -- SIGTERM/SIGINT + configurable drain timeout

### Phase 5: Advanced Routing -- DONE

Power-user features.

17. **Capability filtering by auth context** -- done in Phase 2 (RBAC)
18. **Tool aliasing** -- AliasService with per-backend from/to rename mappings
19. **Hot reload** -- add-only via `notify` file watcher + `McpProxy::add_backend()`
20. **Request coalescing** -- CoalesceService deduplicating in-flight requests

### Phase 6: Library Mode

Make mcp-gateway embeddable as a library so other Rust applications can
programmatically build and run a gateway without the CLI/config file.

21. **Extract `lib.rs`** -- move all gateway logic out of `main.rs` into a library crate
22. **`GatewayBuilder` API** -- programmatic builder that mirrors the config-driven flow
    ```rust
    let gateway = mcp_gateway::GatewayBuilder::new("my-gw", "1.0.0")
        .backend_stdio("files", "npx", &["-y", "@mcp/server-filesystem", "/tmp"])
        .backend_http("remote", "http://mcp:8080")
        .auth_bearer(&["token-1"])
        .timeout("files", Duration::from_secs(30))
        .metrics(true)
        .build()
        .await?;

    // Returns (Router, SessionHandle) for embedding in an existing axum app
    let (router, handle) = gateway.into_router();

    // Or serve standalone
    gateway.serve("127.0.0.1:8080").await?;
    ```
23. **Config-from-struct** -- `GatewayConfig` usable directly without TOML parsing
24. **Dual crate structure** -- `mcp-gateway` (lib) + `mcp-gateway-cli` (bin), or
    single crate with `[[bin]]` and `[lib]`

**Status:** Done. Single crate with `[lib]` + `[[bin]]` targets.
- `Gateway::from_config()` builds proxy + middleware + auth + admin
- `gateway.serve()` for standalone, `gateway.into_router()` for embedding
- `gateway.proxy()` exposes McpProxy for dynamic operations
- `gateway.enable_hot_reload()` watches config for new backends

### Phase 7: Testing

Comprehensive test coverage across three categories.

25. **Config parsing tests** -- TOML loading, env var resolution, validation errors,
    default values, invalid configs rejected with clear messages
26. **Middleware unit tests** -- each middleware module tested in isolation:
    - `CacheService`: cache hits/misses, TTL expiry, per-backend isolation
    - `CoalesceService`: deduplication of concurrent requests, non-interference
    - `AliasService`: forward/reverse name rewriting
    - `CapabilityFilterService`: allow/deny lists, pass-through
    - `ValidationService`: argument size limits, pass-through for valid requests
    - `MetricsService`: counter/histogram recording
    - `RbacService`: role-based allow/deny
27. **Integration tests** -- full gateway stack with in-process backends:
    - End-to-end: config -> Gateway::from_config() -> list/call through middleware
    - Auth: bearer token acceptance/rejection
    - Per-backend middleware: timeout, circuit breaker, rate limit
    - Hot reload: add backend via config change, verify new tools appear
    - Admin API: `/admin/backends` returns correct health and session count

**Status:** Not started.

### Phase 8: MCP Admin Tools

Expose gateway management as MCP tools, so any MCP client can introspect
and manage the gateway.

28. **Admin MCP server** -- register tools on the gateway's own McpRouter:
    - `gateway/list_backends` -- list backends with health status
    - `gateway/health_check` -- trigger health check, return results
    - `gateway/session_count` -- active HTTP sessions
    - `gateway/add_backend` -- dynamically add a backend (name, transport, url/command)
    - `gateway/config` -- dump current running config
29. **Separate namespace** -- admin tools live under a `gateway/` namespace,
    clearly separated from proxied backend tools

**Status:** Not started.

## Config Schema (Target)

See `config.example.toml` for current state. The full target schema
after all phases would look like:

```toml
[gateway]
name = "my-gateway"
version = "0.1.0"
separator = "/"
hot_reload = false
shutdown_timeout_seconds = 30

[gateway.listen]
host = "127.0.0.1"
port = 8080

[auth]
type = "jwt"
issuer = "https://auth.example.com"
audience = "mcp-gateway"
jwks_uri = "https://auth.example.com/.well-known/jwks.json"

[[auth.roles]]
name = "reader"
allow_tools = { read_only = true }

[[auth.roles]]
name = "admin"
allow_tools = "*"

[auth.role_mapping]
claim = "scope"
mapping = { "mcp:read" = "reader", "mcp:admin" = "admin" }

[rate_limit]
requests_per_second = 100

[cache]
enabled = true
max_entries = 1000

[security]
validate_arguments = true
max_argument_size = "1MB"
max_response_size = "10MB"

[observability]
audit = true
log_level = "info"
json_logs = false

[observability.metrics]
enabled = true
endpoint = "/metrics"

[observability.tracing]
enabled = true
exporter = "otlp"
endpoint = "http://localhost:4317"
service_name = "mcp-gateway"

[observability.health]
endpoint = "/health"
detailed = true

[admin]
enabled = false
endpoint = "/admin"

[[backends]]
name = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
expose_tools = ["read_file", "list_directory"]
hide_tools = []

[backends.env]
SOME_VAR = "${ENV_VAR}"

[backends.timeout]
seconds = 30

[backends.circuit_breaker]
failure_threshold = 5
success_threshold = 2
timeout_seconds = 30

[backends.rate_limit]
requests_per_second = 10
burst = 20

[backends.concurrency]
max_concurrent = 10

[backends.retry]
max_attempts = 3
backoff_ms = [100, 500, 2000]
idempotent_only = true

[backends.cache]
resource_ttl_seconds = 300
tool_ttl_seconds = 60
invalidate_on_notify = true
```

## tower-mcp Gaps Identified

Tracked as tower-mcp issues (without mentioning mcp-gateway):

| Gap | Issue | Status |
|-----|-------|--------|
| spawn_command() for env vars | #622 | Merged (PR #624) |
| HttpTransport::from_service() | #623 | Merged (PR #626) |
| HTTP backend transport for proxy | -- | Done (HttpClientTransport) |
| Capability filtering on McpProxy | -- | Not needed (gateway-side middleware) |
| Per-backend notification forwarding over HTTP | -- | Not filed yet |
| Per-backend health in proxy (public API) | -- | Already available (McpProxy::health_check) |
| Session count from HttpTransport | #627 | Done (PR #629, SessionHandle) |
| Dynamic backend addition on McpProxy | #628 | Done (PR #630, add_backend()) |
