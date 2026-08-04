# mcp-proxy

[![Crates.io](https://img.shields.io/crates/v/mcp-proxy.svg)](https://crates.io/crates/mcp-proxy)
[![docs.rs](https://docs.rs/mcp-proxy/badge.svg)](https://docs.rs/mcp-proxy)
[![CI](https://github.com/joshrotenberg/mcp-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/mcp-proxy/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/mcp-proxy.svg)](LICENSE-MIT)

A Tower-native MCP traffic plane for teams that need resilient, policy-aware MCP routing without adopting a full platform.

mcp-proxy is a config-driven [Model Context Protocol](https://modelcontextprotocol.io/) reverse proxy. It aggregates stdio, HTTP, and WebSocket MCP backends behind one endpoint and applies authentication, per-backend resilience, traffic management, and observability as composable middleware from the [tower](https://github.com/tower-rs/tower) ecosystem, via [tower-mcp](https://github.com/joshrotenberg/tower-mcp). Run it as a single binary or embed it in a Rust application.

> **Project status:** maintained with an intentionally stable scope. mcp-proxy
> is a tower-native gateway and reference deployment for aggregating MCP
> backends. Maintenance focuses on security, dependency and protocol updates,
> bug fixes, and documentation rather than speculative new features.

## Where it fits

- **Self-hosted internal MCP fleets** -- one authenticated, observable endpoint in front of the MCP servers a team already runs.
- **Non-Kubernetes and mixed deployments** -- a single binary and a TOML file; no service mesh, operator, or container platform required.
- **Rust applications that need a gateway** -- the same proxy is a library; mount it in an existing axum app or drive it from a builder.

For how mcp-proxy relates to other MCP gateways (Docker MCP Gateway, IBM ContextForge, Microsoft MCP Gateway, Kong), see [docs/comparison.md](docs/comparison.md).

## What it does

**Aggregates many MCP servers behind one endpoint.** Backends speaking stdio, HTTP, or WebSocket are exposed under per-backend namespaces at a single HTTP endpoint. Tools, resources, and prompts can be allow/deny filtered and aliased per backend; default or per-tool arguments can be injected into calls; composite tools fan one call out across multiple backend tools; hot reload adds new backends from config changes without a restart; and optional BM25 discovery can expose search instead of full tool lists.

**Contains backend failures.** Each backend gets its own resilience chain: timeouts, rate limits, concurrency caps, retries with exponential backoff and budgets, circuit breakers, request hedging, and outlier detection that temporarily ejects unhealthy backends.

**Manages traffic for rollouts and load.** Traffic mirroring shadows a percentage of requests to a canary backend; canary routing and ordered failover control weighted rollouts; response caching (in-memory, Redis, or SQLite) and request coalescing cut duplicate work.

**Applies policy at the front door.** Bearer token, JWT/JWKS, and OAuth 2.1 authentication; role-based tool visibility (RBAC); token passthrough to backends; and request argument validation.

**Reports what is happening.** Prometheus metrics, OpenTelemetry trace export, structured audit logging, an admin HTTP API for health, backend status, and cache stats, and admin MCP tools under the `proxy/` namespace.

Every option is documented in [`config.example.toml`](config.example.toml), deployment shapes in [`docs/architectures.md`](docs/architectures.md), and runnable configurations in [`examples/`](examples/).

## Installation

### Homebrew

```bash
brew install joshrotenberg/brew/mcp-proxy
```

### Cargo

```bash
cargo install mcp-proxy
```

### Docker

```bash
docker pull ghcr.io/joshrotenberg/mcp-proxy:latest
docker run -v ./proxy.toml:/etc/mcp-proxy/proxy.toml:ro -p 8080:8080 ghcr.io/joshrotenberg/mcp-proxy:latest
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/joshrotenberg/mcp-proxy/releases).

## Quick Start

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
mcp-proxy = "0.4"
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

## Feature Flags

Pre-built binaries and `cargo install` include the default features. If you're building from source and don't need everything, you can disable optional features for a smaller binary:

| Feature | Default | What it includes |
|---------|---------|-----------------|
| `otel` | yes | OpenTelemetry distributed tracing (OTLP export) |
| `metrics` | yes | Prometheus metrics and `/admin/metrics` endpoint |
| `oauth` | yes | JWT/JWKS auth, RBAC, and token passthrough |
| `openapi` | yes | OpenAPI schema and endpoint support |
| `websocket` | yes | WebSocket backend transport |
| `discovery` | yes | BM25 tool discovery and search exposure mode |
| `yaml` | yes | YAML configuration files |
| `skills` | yes | agentskills.io prompts for proxy administration |
| `redis-cache` | no | Shared Redis response cache |
| `sqlite-cache` | no | Persistent SQLite response cache |
| `protocol-2026-07-28` | no | Released MCP 2026-07-28 protocol support through tower-mcp |

```bash
# Minimal build (bearer auth only, no metrics/tracing/JWT)
cargo install mcp-proxy --no-default-features

# Just metrics, no otel or JWT
cargo install mcp-proxy --no-default-features --features metrics
```

Config parsing always works regardless of features -- if you reference a disabled feature in your config (e.g., `type = "jwt"` without the `oauth` feature), you'll get a clear error at startup.

The default protocol baseline remains MCP 2025-11-25. Build with
`--features protocol-2026-07-28` to compile support for the released,
sessionless 2026-07-28 protocol. Continuation fields such as `inputResponses`
and `requestState` are preserved through proxy routing in either build.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
