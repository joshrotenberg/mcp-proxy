//! Config-driven MCP reverse proxy with auth, resilience middleware, and observability.
//!
//! Aggregates multiple MCP backends behind a single HTTP endpoint with namespace
//! isolation, per-backend middleware, and a comprehensive admin API. Built on
//! [tower-mcp](https://docs.rs/tower-mcp) and the [tower](https://docs.rs/tower) ecosystem.
//!
//! # Quick Start
//!
//! ## From a config file
//!
//! ```rust,no_run
//! use mcp_proxy::{Proxy, ProxyConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = ProxyConfig::load("proxy.toml".as_ref())?;
//! let proxy = Proxy::from_config(config).await?;
//! proxy.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Programmatic builder
//!
//! ```rust,no_run
//! use mcp_proxy::ProxyBuilder;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let proxy = ProxyBuilder::new("my-proxy")
//!     .listen("0.0.0.0", 9090)
//!     .http_backend("api", "http://api:8080")
//!     .timeout(30)
//!     .rate_limit(100, 1)
//!     .stdio_backend("files", "npx", &["-y", "@mcp/server-files"])
//!     .build()
//!     .await?;
//!
//! proxy.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Embed in an existing axum app
//!
//! ```rust,no_run
//! use mcp_proxy::{Proxy, ProxyConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = ProxyConfig::load("proxy.toml".as_ref())?;
//! let proxy = Proxy::from_config(config).await?;
//! let (router, session_handle) = proxy.into_router();
//! // Mount `router` in your axum app
//! # Ok(())
//! # }
//! ```
//!
//! # Backend Transports
//!
//! | Transport | Config | Use Case |
//! |-----------|--------|----------|
//! | `stdio` | `command`, `args`, `env` | Local subprocess (npx, python, etc.) |
//! | `http` | `url`, `bearer_token` | Remote HTTP+SSE MCP server |
//! | `websocket` | `url`, `bearer_token` | Remote WebSocket MCP server |
//!
//! Backends are namespaced: a backend named `"files"` exposes tools as
//! `files/read_file`, `files/write_file`, etc.
//!
//! # Per-Backend Middleware
//!
//! Each backend can independently configure:
//!
//! - **Timeout** -- per-request deadline
//! - **Circuit breaker** -- failure-rate based with observable state handles
//! - **Rate limit** -- request volume caps
//! - **Retry** -- exponential backoff with budget control
//! - **Hedging** -- parallel requests for tail latency reduction
//! - **Outlier detection** -- passive health tracking with ejection
//! - **Response caching** -- per-backend TTL with memory, Redis, or SQLite backends
//! - **Argument injection** -- merge default args into tool calls
//! - **Parameter overrides** -- hide or rename tool parameters
//! - **Capability filtering** -- allowlist/denylist tools, resources, prompts
//!   (glob patterns and `re:` regex support)
//! - **Annotation filtering** -- hide destructive tools, read-only-only mode
//!
//! # Traffic Routing
//!
//! - **Failover** -- N-way priority chains with automatic fallback
//! - **Canary routing** -- weight-based traffic splitting
//! - **Traffic mirroring** -- fire-and-forget shadow traffic
//! - **Composite tools** -- fan-out a single call to multiple backends
//! - **Tool aliasing** -- rename tools across backends
//!
//! # Authentication
//!
//! Three auth modes, configured via `[auth]`:
//!
//! - **Bearer tokens** -- simple static tokens with optional per-token tool scoping
//! - **JWT/JWKS** -- validate JWTs via remote JWKS endpoint with RBAC role mapping
//! - **OAuth 2.1** -- auto-discovery (RFC 8414), token introspection (RFC 7662),
//!   JWT+introspection fallback
//!
//! # Admin API
//!
//! REST API at `/admin/` for monitoring and management:
//!
//! - Backend health, history, and circuit breaker states
//! - Session listing and termination
//! - Cache stats and clearing
//! - Config viewing, validation, and updates
//! - Prometheus metrics and OpenAPI spec
//! - Optional bearer token auth (`security.admin_token`)
//!
//! # Tool Discovery
//!
//! When `tool_discovery = true`, BM25 full-text search indexes all backend tools
//! and exposes `proxy/search_tools`, `proxy/similar_tools`, and
//! `proxy/tool_categories` for finding tools across large deployments.
//!
//! Set `tool_exposure = "search"` to hide individual tools from listing and expose
//! only meta-tools (search + call_tool), scaling to hundreds of backends.
//!
//! # Agent Skills
//!
//! MCP prompts following the [agentskills.io](https://agentskills.io) specification
//! for agent-assisted proxy management: setup, auth configuration, resilience tuning,
//! config validation, diagnostics, and status reporting.
//!
//! # Config Formats
//!
//! - **TOML** (default): `proxy.toml`
//! - **YAML** (`yaml` feature): `proxy.yaml` / `proxy.yml`
//! - **`.mcp.json`**: `mcp-proxy --from-mcp-json .mcp.json` for zero-config mode
//!
//! # Hot Reload
//!
//! Enable `hot_reload = true` to watch the config file. Backends are added,
//! removed, or replaced dynamically without restart. Discovery indexes are
//! automatically re-indexed.
//!
//! # Feature Flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `otel` | Yes | OpenTelemetry tracing export |
//! | `metrics` | Yes | Prometheus metrics at `/admin/metrics` |
//! | `oauth` | Yes | JWT/JWKS auth, OAuth 2.1, bearer scoping, RBAC |
//! | `openapi` | Yes | OpenAPI spec at `/admin/openapi.json` |
//! | `websocket` | Yes | WebSocket backend transport |
//! | `discovery` | Yes | BM25 tool discovery via jpx-engine |
//! | `yaml` | Yes | YAML config format support |
//! | `skills` | Yes | agentskills.io management prompts |
//! | `redis-cache` | No | Redis cache backend |
//! | `sqlite-cache` | No | SQLite cache backend |
//!
//! Minimal build: `cargo install mcp-proxy --no-default-features`
//!
//! # Performance
//!
//! Middleware stack overhead is sub-microsecond (~115ns per request). Cache hits
//! are 33x faster than backend round-trips. See `benches/proxy_overhead.rs`.

pub mod access_log;
pub mod admin;
pub mod admin_tools;
pub mod alias;
#[cfg(feature = "oauth")]
pub mod bearer_scope;
pub mod builder;
pub mod cache;
pub mod canary;
pub mod coalesce;
pub mod composite;
pub mod config;
#[cfg(feature = "discovery")]
pub mod discovery;
pub mod failover;
pub mod filter;
pub mod inject;
#[cfg(feature = "oauth")]
pub mod introspection;
pub mod mcp_json;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod mirror;
pub mod outlier;
pub mod param_override;
#[cfg(feature = "oauth")]
pub mod rbac;
pub mod reload;
pub mod retry;
#[cfg(feature = "skills")]
pub mod skills;
#[cfg(feature = "oauth")]
pub mod token;
pub mod validation;
#[cfg(feature = "websocket")]
pub mod ws_transport;

#[cfg(test)]
mod test_util;

mod proxy;

pub use builder::ProxyBuilder;
pub use config::ProxyConfig;
pub use proxy::Proxy;
