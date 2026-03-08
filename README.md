# mcp-proxy

[![Crates.io](https://img.shields.io/crates/v/mcp-proxy.svg)](https://crates.io/crates/mcp-proxy)
[![docs.rs](https://docs.rs/mcp-proxy/badge.svg)](https://docs.rs/mcp-proxy)
[![CI](https://github.com/joshrotenberg/mcp-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/mcp-proxy/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/mcp-proxy.svg)](LICENSE-MIT)

A config-driven [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) reverse proxy built in Rust. Aggregates multiple MCP backends behind a single endpoint with per-backend middleware, authentication, and observability.

Built on [tower-mcp](https://github.com/joshrotenberg/tower-mcp) and the [tower](https://github.com/tower-rs/tower) middleware ecosystem.

## Features

### Proxy
- **Multi-backend proxy** -- connect stdio and HTTP MCP servers behind one endpoint
- **Capability filtering** -- allow/deny lists for tools, resources, and prompts per backend
- **Tool aliasing** -- rename tools exposed by backends
- **Argument injection** -- merge default or per-tool arguments into tool calls
- **Hot reload** -- watch config file and add new backends without restart
- **Library mode** -- embed the proxy in your own Rust application

### Resilience
- **Timeout** -- per-backend request timeouts
- **Rate limiting** -- per-backend request rate limits
- **Concurrency limiting** -- per-backend max concurrent requests
- **Circuit breaker** -- trip open on failure rate threshold
- **Retry** -- automatic retries with exponential backoff and optional budget
- **Request hedging** -- parallel redundant requests to reduce tail latency
- **Outlier detection** -- passive health checks that eject unhealthy backends

### Traffic Management
- **Traffic mirroring** -- shadow traffic to a canary backend (fire-and-forget)
- **Response caching** -- per-backend TTL-based caching for tool calls and resource reads
- **Request coalescing** -- deduplicate identical concurrent requests

### Security
- **Bearer token auth** -- static token validation
- **JWT/JWKS auth** -- token verification with RBAC (role-based access control)
- **Token passthrough** -- forward client auth tokens to backends
- **Request validation** -- argument size limits

### Observability
- **Prometheus metrics** -- request counts and duration histograms
- **OpenTelemetry tracing** -- distributed trace export via OTLP
- **Audit logging** -- structured logging of all MCP requests
- **Admin API** -- health checks, backend status, cache stats
- **Admin MCP tools** -- introspection tools under `proxy/` namespace

## Quick Start

```bash
cargo install mcp-proxy
```

Create a `proxy.toml`:

```toml
[proxy]
name = "my-proxy"
separator = "/"

[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

Run:

```bash
mcp-proxy --config proxy.toml
```

All tools from the filesystem server are now available under the `files/` namespace at `http://127.0.0.1:8080/mcp`.

## Configuration

See [`config.example.toml`](config.example.toml) for the full configuration reference with all options documented.

### Per-backend middleware

```toml
[[backends]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[backends.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}"

[backends.timeout]
seconds = 60

[backends.rate_limit]
requests = 30
period_seconds = 1

[backends.circuit_breaker]
failure_rate_threshold = 0.5
minimum_calls = 5
wait_duration_seconds = 30

[backends.retry]
max_retries = 3
initial_backoff_ms = 100
max_backoff_ms = 5000
budget_percent = 20.0

[backends.hedging]
delay_ms = 200
max_hedges = 1

[backends.outlier_detection]
consecutive_errors = 5
base_ejection_seconds = 30
max_ejection_percent = 50

[backends.cache]
tool_ttl_seconds = 60
resource_ttl_seconds = 300
```

### Argument injection

```toml
[[backends]]
name = "db"
transport = "http"
url = "http://db.internal:8080"

# Inject into all tool calls for this backend
[backends.default_args]
timeout = 30

# Inject into a specific tool (overrides default_args for matching keys)
[[backends.inject_args]]
tool = "query"
args = { read_only = true, max_rows = 1000 }

# Force overwrite existing arguments
[[backends.inject_args]]
tool = "dangerous_op"
args = { dry_run = true }
overwrite = true
```

### Traffic mirroring

```toml
[[backends]]
name = "api"
transport = "http"
url = "http://api-v1:8080"

[[backends]]
name = "api-v2"
transport = "http"
url = "http://api-v2:8080"
mirror_of = "api"
mirror_percent = 10
```

### Authentication

```toml
# Bearer token
[auth]
type = "bearer"
tokens = ["my-secret-token"]

# Or JWT with RBAC
[auth]
type = "jwt"
issuer = "https://auth.example.com"
audience = "mcp-proxy"
jwks_uri = "https://auth.example.com/.well-known/jwks.json"

[[auth.roles]]
name = "reader"
allow_tools = ["files/read_file", "files/list_directory"]

[[auth.roles]]
name = "admin"

[auth.role_mapping]
claim = "scope"
mapping = { "mcp:read" = "reader", "mcp:admin" = "admin" }
```

### Capability filtering

```toml
[[backends]]
name = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
# Only expose these tools
expose_tools = ["read_file", "list_directory"]
# Or hide specific tools
# hide_tools = ["write_file", "delete_file"]
```

## Library Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
mcp-proxy = "0.1"
```

```rust
use mcp_proxy::{Proxy, ProxyConfig};

let config = ProxyConfig::load("proxy.toml".as_ref())?;
let proxy = Proxy::from_config(config).await?;

// Embed in an existing axum app
let (router, session_handle) = proxy.into_router();

// Or serve standalone
proxy.serve().await?;
```

## Admin API

HTTP endpoints:

- `GET /admin/backends` -- list backends with health status and proxy info
- `GET /admin/health` -- health check summary (healthy/degraded)
- `GET /admin/metrics` -- Prometheus metrics
- `GET /admin/cache/stats` -- per-backend cache hit/miss rates
- `POST /admin/cache/clear` -- clear all caches

MCP tools (under `proxy/` namespace):

- `proxy/list_backends` -- list backends with health status
- `proxy/health_check` -- cached health check results
- `proxy/session_count` -- active session count
- `proxy/add_backend` -- dynamically add an HTTP backend
- `proxy/config` -- dump current config

## Architecture

```
Client
  |
  v
[Auth] -> [Audit] -> [Metrics] -> [Token Passthrough] -> [RBAC]
  -> [Alias] -> [Filter] -> [Validation] -> [Coalesce] -> [Cache]
  -> [Mirror] -> [Inject Args]
  -> McpProxy
       |
       v  (per-backend)
     [Retry] -> [Hedge] -> [Concurrency] -> [Rate Limit]
       -> [Timeout] -> [Circuit Breaker] -> [Outlier Detection]
       -> Backend
```

Global middleware wraps the entire proxy. Per-backend middleware is applied individually to each backend connection. All middleware is built with tower `Service` layers.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
