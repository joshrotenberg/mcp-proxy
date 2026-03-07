# mcp-gateway

A config-driven [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) gateway built in Rust. Aggregates multiple MCP backends behind a single endpoint with per-backend middleware, authentication, and observability.

Built on [tower-mcp](https://github.com/joshrotenberg/tower-mcp) and the [tower](https://github.com/tower-rs/tower) middleware ecosystem.

## Features

- **Multi-backend proxy** -- connect stdio and HTTP MCP servers behind one endpoint
- **Per-backend middleware** -- timeout, rate limiting, circuit breaker, response caching, request coalescing
- **Capability filtering** -- allow/deny lists for tools, resources, and prompts per backend
- **Tool aliasing** -- rename tools exposed by backends
- **Authentication** -- bearer token or JWT/JWKS with role-based access control
- **Observability** -- Prometheus metrics, OpenTelemetry tracing, structured audit logging
- **Hot reload** -- watch config file and add new backends without restart
- **Library mode** -- embed the gateway in your own Rust application

## Quick Start

```bash
cargo install mcp-gateway
```

Create a `gateway.toml`:

```toml
[gateway]
name = "my-gateway"
separator = "/"

[gateway.listen]
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
mcp-gateway --config gateway.toml
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

[backends.cache]
tool_ttl_seconds = 60
resource_ttl_seconds = 300
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
audience = "mcp-gateway"
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
mcp-gateway = "0.1"
```

```rust
use mcp_gateway::{Gateway, GatewayConfig};

let config = GatewayConfig::load("gateway.toml".as_ref())?;
let gateway = Gateway::from_config(config).await?;

// Embed in an existing axum app
let (router, session_handle) = gateway.into_router();

// Or serve standalone
gateway.serve().await?;
```

## Admin API

The gateway exposes admin endpoints:

- `GET /admin/backends` -- list backends with health status
- `GET /admin/metrics` -- Prometheus metrics

## Architecture

```
Client --> [Auth] --> [Metrics] --> [Validation] --> [Filter] --> [Alias]
  --> McpProxy --> [Timeout] --> [RateLimit] --> [CircuitBreaker] --> Backend
                   [Cache]      [Coalesce]
```

The middleware stack is built with tower `Service` layers. Proxy-level middleware (auth, metrics, validation, filtering, aliasing) wraps the entire proxy. Per-backend middleware (timeout, rate limit, circuit breaker) is applied individually to each backend connection.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
