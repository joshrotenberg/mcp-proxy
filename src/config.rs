//! Proxy configuration types and TOML parsing.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level proxy configuration, typically loaded from a TOML file.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProxyConfig {
    /// Core proxy settings (name, version, listen address).
    pub proxy: ProxySettings,
    /// Backend MCP servers to proxy.
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
    /// Inbound authentication configuration.
    pub auth: Option<AuthConfig>,
    /// Performance tuning options.
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// Security policies.
    #[serde(default)]
    pub security: SecurityConfig,
    /// Global cache backend configuration.
    #[serde(default)]
    pub cache: CacheBackendConfig,
    /// Logging, metrics, and tracing configuration.
    #[serde(default)]
    pub observability: ObservabilityConfig,
    /// Composite tools that fan out to multiple backend tools.
    #[serde(default)]
    pub composite_tools: Vec<CompositeToolConfig>,
}

/// Fan-out strategy for composite tools.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompositeStrategy {
    /// Execute all tools concurrently using `tokio::JoinSet`.
    #[default]
    Parallel,
}

/// Configuration for a composite tool that fans out to multiple backend tools.
///
/// Composite tools appear in `ListTools` responses alongside regular tools.
/// When called, the proxy dispatches the request to every tool in [`tools`](Self::tools)
/// concurrently (for `parallel` strategy) and aggregates all results.
///
/// # Example
///
/// ```toml
/// [[composite_tools]]
/// name = "search_all"
/// description = "Search across all knowledge sources"
/// tools = ["github/search", "jira/search", "docs/search"]
/// strategy = "parallel"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompositeToolConfig {
    /// Name of the composite tool as it appears to MCP clients.
    pub name: String,
    /// Human-readable description of the composite tool.
    pub description: String,
    /// Fully-qualified backend tool names to fan out to (e.g. `"github/search"`).
    pub tools: Vec<String>,
    /// Execution strategy (default: `parallel`).
    #[serde(default)]
    pub strategy: CompositeStrategy,
}

/// Core proxy identity and server settings.
#[derive(Debug, Deserialize, Serialize)]
pub struct ProxySettings {
    /// Proxy name, used in MCP server info.
    pub name: String,
    /// Proxy version, used in MCP server info (default: "0.1.0").
    #[serde(default = "default_version")]
    pub version: String,
    /// Namespace separator between backend name and tool/resource name (default: "/").
    #[serde(default = "default_separator")]
    pub separator: String,
    /// HTTP listen address.
    pub listen: ListenConfig,
    /// Optional instructions text sent to MCP clients.
    pub instructions: Option<String>,
    /// Graceful shutdown timeout in seconds (default: 30)
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,
    /// Enable hot reload: watch config file for new backends
    #[serde(default)]
    pub hot_reload: bool,
    /// Import backends from a `.mcp.json` file. Backends defined in the TOML
    /// config take precedence over imported ones with the same name.
    pub import_backends: Option<String>,
    /// Global rate limit applied to all requests before per-backend dispatch.
    pub rate_limit: Option<GlobalRateLimitConfig>,
    /// Enable BM25-based tool discovery and search (default: false).
    /// Adds `proxy/search_tools`, `proxy/similar_tools`, and
    /// `proxy/tool_categories` tools for finding tools across backends.
    #[serde(default)]
    pub tool_discovery: bool,
    /// How backend tools are exposed to MCP clients (default: "direct").
    ///
    /// - `direct` -- all tools appear in `ListTools` responses (default behavior).
    /// - `search` -- only `proxy/` meta-tools are listed; backend tools are
    ///   discoverable via `proxy/search_tools` and invokable via `proxy/call_tool`.
    ///   Useful when aggregating 100+ tools that would overwhelm LLM context.
    ///   Implies `tool_discovery = true`.
    #[serde(default)]
    pub tool_exposure: ToolExposure,
}

/// How backend tools are exposed to MCP clients.
///
/// Controls whether individual backend tools appear in `ListTools` responses
/// or are hidden behind discovery meta-tools.
///
/// # Examples
///
/// ```
/// use mcp_proxy::config::ToolExposure;
///
/// let direct: ToolExposure = serde_json::from_str("\"direct\"").unwrap();
/// assert_eq!(direct, ToolExposure::Direct);
///
/// let search: ToolExposure = serde_json::from_str("\"search\"").unwrap();
/// assert_eq!(search, ToolExposure::Search);
/// ```
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolExposure {
    /// All backend tools appear in `ListTools` responses.
    #[default]
    Direct,
    /// Only `proxy/` namespace meta-tools appear. Backend tools are hidden
    /// from listings but remain invokable via `proxy/call_tool`.
    Search,
}

/// Global rate limit configuration applied across all backends.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GlobalRateLimitConfig {
    /// Maximum number of requests allowed per period.
    pub requests: usize,
    /// Period length in seconds (default: 1).
    #[serde(default = "default_rate_period")]
    pub period_seconds: u64,
}

/// HTTP server listen address.
#[derive(Debug, Deserialize, Serialize)]
pub struct ListenConfig {
    /// Bind host (default: "127.0.0.1").
    #[serde(default = "default_host")]
    pub host: String,
    /// Bind port (default: 8080).
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Configuration for a single backend MCP server.
#[derive(Debug, Deserialize, Serialize)]
pub struct BackendConfig {
    /// Unique backend name, used as the namespace prefix for its tools/resources.
    pub name: String,
    /// Transport protocol to use when connecting to this backend.
    pub transport: TransportType,
    /// Command for stdio backends
    pub command: Option<String>,
    /// Arguments for stdio backends
    #[serde(default)]
    pub args: Vec<String>,
    /// URL for HTTP backends
    pub url: Option<String>,
    /// Environment variables for subprocess backends
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-backend timeout
    pub timeout: Option<TimeoutConfig>,
    /// Per-backend circuit breaker
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    /// Per-backend rate limit
    pub rate_limit: Option<RateLimitConfig>,
    /// Per-backend concurrency limit
    pub concurrency: Option<ConcurrencyConfig>,
    /// Per-backend retry policy
    pub retry: Option<RetryConfig>,
    /// Per-backend outlier detection (passive health checks)
    pub outlier_detection: Option<OutlierDetectionConfig>,
    /// Per-backend request hedging (parallel redundant requests)
    pub hedging: Option<HedgingConfig>,
    /// Mirror traffic from another backend (fire-and-forget).
    /// Set to the name of the source backend to mirror.
    pub mirror_of: Option<String>,
    /// Percentage of requests to mirror (1-100, default: 100).
    #[serde(default = "default_mirror_percent")]
    pub mirror_percent: u32,
    /// Per-backend cache policy
    pub cache: Option<BackendCacheConfig>,
    /// Static bearer token for authenticating to this backend (HTTP only).
    /// Supports `${ENV_VAR}` syntax for env var resolution.
    pub bearer_token: Option<String>,
    /// Forward the client's inbound auth token to this backend.
    /// Only works with HTTP backends when the proxy has auth enabled.
    #[serde(default)]
    pub forward_auth: bool,
    /// Tool aliases: rename tools exposed by this backend
    #[serde(default)]
    pub aliases: Vec<AliasConfig>,
    /// Default arguments injected into all tool calls for this backend.
    /// Merged into tool call arguments (does not overwrite existing keys).
    #[serde(default)]
    pub default_args: serde_json::Map<String, serde_json::Value>,
    /// Per-tool argument injection rules.
    #[serde(default)]
    pub inject_args: Vec<InjectArgsConfig>,
    /// Per-tool parameter overrides: hide, rename, and inject defaults.
    #[serde(default)]
    pub param_overrides: Vec<ParamOverrideConfig>,
    /// Capability filtering: only expose these tools (allowlist)
    #[serde(default)]
    pub expose_tools: Vec<String>,
    /// Capability filtering: hide these tools (denylist)
    #[serde(default)]
    pub hide_tools: Vec<String>,
    /// Capability filtering: only expose these resources (allowlist, by URI)
    #[serde(default)]
    pub expose_resources: Vec<String>,
    /// Capability filtering: hide these resources (denylist, by URI)
    #[serde(default)]
    pub hide_resources: Vec<String>,
    /// Capability filtering: only expose these prompts (allowlist)
    #[serde(default)]
    pub expose_prompts: Vec<String>,
    /// Capability filtering: hide these prompts (denylist)
    #[serde(default)]
    pub hide_prompts: Vec<String>,
    /// Hide tools annotated as destructive (`destructive_hint = true`).
    #[serde(default)]
    pub hide_destructive: bool,
    /// Only expose tools annotated as read-only (`read_only_hint = true`).
    #[serde(default)]
    pub read_only_only: bool,
    /// Failover: name of the primary backend this is a failover for.
    /// When set, this backend's tools are hidden and requests are only
    /// routed here when the primary returns an error.
    pub failover_for: Option<String>,
    /// Failover priority for ordering multiple failover backends.
    /// Lower values are preferred (tried first). Default is 0.
    /// When multiple backends declare `failover_for` the same primary,
    /// they are tried in ascending priority order until one succeeds.
    #[serde(default)]
    pub priority: u32,
    /// Canary routing: name of the primary backend this is a canary for.
    /// When set, this backend's tools are hidden and requests targeting
    /// the primary are probabilistically routed here based on weight.
    pub canary_of: Option<String>,
    /// Routing weight for canary deployments (default: 100).
    /// Higher values receive proportionally more traffic.
    #[serde(default = "default_weight")]
    pub weight: u32,
}

/// Backend transport protocol.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    /// Subprocess communicating via stdin/stdout.
    Stdio,
    /// HTTP+SSE remote server.
    Http,
    /// WebSocket remote server.
    Websocket,
}

/// Per-backend request timeout.
#[derive(Debug, Deserialize, Serialize)]
pub struct TimeoutConfig {
    /// Timeout duration in seconds.
    pub seconds: u64,
}

/// Per-backend circuit breaker configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct CircuitBreakerConfig {
    /// Failure rate threshold (0.0-1.0) to trip open (default: 0.5)
    #[serde(default = "default_failure_rate")]
    pub failure_rate_threshold: f64,
    /// Minimum number of calls before evaluating failure rate (default: 5)
    #[serde(default = "default_min_calls")]
    pub minimum_calls: usize,
    /// Seconds to wait in open state before half-open (default: 30)
    #[serde(default = "default_wait_duration")]
    pub wait_duration_seconds: u64,
    /// Number of permitted calls in half-open state (default: 3)
    #[serde(default = "default_half_open_calls")]
    pub permitted_calls_in_half_open: usize,
}

/// Per-backend rate limiting configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct RateLimitConfig {
    /// Maximum requests per period
    pub requests: usize,
    /// Period in seconds (default: 1)
    #[serde(default = "default_rate_period")]
    pub period_seconds: u64,
}

/// Per-backend concurrency limit configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct ConcurrencyConfig {
    /// Maximum concurrent requests.
    pub max_concurrent: usize,
}

/// Per-backend retry policy with exponential backoff.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (default: 3)
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Initial backoff in milliseconds (default: 100)
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Maximum backoff in milliseconds (default: 5000)
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
    /// Maximum percentage of requests that can be retries (default: none / unlimited).
    /// When set, prevents retry storms by capping retries as a fraction of total
    /// request volume. Envoy uses 20% as a default. Evaluated over a 10-second
    /// rolling window.
    pub budget_percent: Option<f64>,
    /// Minimum retries per second allowed regardless of budget (default: 10).
    /// Ensures low-traffic backends can still retry.
    #[serde(default = "default_min_retries_per_sec")]
    pub min_retries_per_sec: u32,
}

/// Passive health check / outlier detection configuration.
///
/// Tracks consecutive errors on live traffic and ejects unhealthy backends.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutlierDetectionConfig {
    /// Number of consecutive errors before ejecting (default: 5)
    #[serde(default = "default_consecutive_errors")]
    pub consecutive_errors: u32,
    /// Evaluation interval in seconds (default: 10)
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u64,
    /// How long to eject in seconds (default: 30)
    #[serde(default = "default_base_ejection_seconds")]
    pub base_ejection_seconds: u64,
    /// Maximum percentage of backends that can be ejected (default: 50)
    #[serde(default = "default_max_ejection_percent")]
    pub max_ejection_percent: u32,
}

/// Per-tool argument injection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InjectArgsConfig {
    /// Tool name (backend-local, without namespace prefix).
    pub tool: String,
    /// Arguments to inject. Merged into the tool call arguments.
    /// Does not overwrite existing keys unless `overwrite` is true.
    pub args: serde_json::Map<String, serde_json::Value>,
    /// Whether injected args should overwrite existing values (default: false).
    #[serde(default)]
    pub overwrite: bool,
}

/// Per-tool parameter override configuration.
///
/// Allows hiding parameters from tool schemas (injecting defaults instead),
/// and renaming parameters to present a more domain-specific interface.
///
/// # Configuration
///
/// ```toml
/// [[backends.param_overrides]]
/// tool = "list_directory"
/// hide = ["path"]
/// defaults = { path = "/home/docs" }
/// rename = { recursive = "deep_search" }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ParamOverrideConfig {
    /// Tool name (backend-local, without namespace prefix).
    pub tool: String,
    /// Parameters to hide from the tool's input schema.
    /// Hidden parameters are removed from the schema and their values
    /// are injected from `defaults` at call time.
    #[serde(default)]
    pub hide: Vec<String>,
    /// Default values for hidden parameters. These are injected into
    /// tool call arguments when the parameter is hidden.
    #[serde(default)]
    pub defaults: serde_json::Map<String, serde_json::Value>,
    /// Parameter renames: maps original parameter names to new names.
    /// The schema exposes the new name; at call time the new name is
    /// mapped back to the original before forwarding to the backend.
    #[serde(default)]
    pub rename: HashMap<String, String>,
}

/// Request hedging configuration.
///
/// Sends parallel redundant requests to reduce tail latency. If the primary
/// request hasn't completed after `delay_ms`, a hedge request is fired.
/// The first successful response wins.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HedgingConfig {
    /// Delay in milliseconds before sending a hedge request (default: 200).
    /// Set to 0 for parallel mode (all requests fire immediately).
    #[serde(default = "default_hedge_delay_ms")]
    pub delay_ms: u64,
    /// Maximum number of additional hedge requests (default: 1)
    #[serde(default = "default_max_hedges")]
    pub max_hedges: usize,
}

/// Inbound authentication configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    /// Static bearer token authentication.
    Bearer {
        /// Accepted bearer tokens (all tools allowed).
        #[serde(default)]
        tokens: Vec<String>,
        /// Tokens with per-token tool access control.
        #[serde(default)]
        scoped_tokens: Vec<BearerTokenConfig>,
    },
    /// JWT authentication via JWKS endpoint.
    Jwt {
        /// Expected token issuer (`iss` claim).
        issuer: String,
        /// Expected token audience (`aud` claim).
        audience: String,
        /// URL to fetch the JSON Web Key Set for token verification.
        jwks_uri: String,
        /// RBAC role definitions
        #[serde(default)]
        roles: Vec<RoleConfig>,
        /// Map JWT claims to roles
        role_mapping: Option<RoleMappingConfig>,
    },
    /// OAuth 2.1 authentication with auto-discovery and token introspection.
    ///
    /// Discovers authorization server endpoints (JWKS URI, introspection endpoint)
    /// from the issuer URL via RFC 8414 metadata. Supports JWT validation,
    /// opaque token introspection, or both.
    OAuth {
        /// Authorization server issuer URL (e.g. `https://accounts.google.com`).
        /// Used for RFC 8414 metadata discovery.
        issuer: String,
        /// Expected token audience (`aud` claim).
        audience: String,
        /// OAuth client ID (required for token introspection).
        #[serde(default)]
        client_id: Option<String>,
        /// OAuth client secret (required for token introspection).
        /// Supports `${ENV_VAR}` syntax.
        #[serde(default)]
        client_secret: Option<String>,
        /// Token validation strategy.
        #[serde(default)]
        token_validation: TokenValidationStrategy,
        /// Override the auto-discovered JWKS URI.
        #[serde(default)]
        jwks_uri: Option<String>,
        /// Override the auto-discovered introspection endpoint.
        #[serde(default)]
        introspection_endpoint: Option<String>,
        /// Required scopes for access (space-delimited).
        #[serde(default)]
        required_scopes: Vec<String>,
        /// RBAC role definitions.
        #[serde(default)]
        roles: Vec<RoleConfig>,
        /// Map JWT/token claims to roles.
        role_mapping: Option<RoleMappingConfig>,
    },
}

/// Token validation strategy for OAuth 2.1 auth.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenValidationStrategy {
    /// Validate JWTs locally via JWKS (default). Fast, no network call per request.
    #[default]
    Jwt,
    /// Validate tokens via the authorization server's introspection endpoint (RFC 7662).
    /// Works with opaque tokens. Requires `client_id` and `client_secret`.
    Introspection,
    /// Try JWT validation first; fall back to introspection for non-JWT tokens.
    /// Requires `client_id` and `client_secret`.
    Both,
}

/// Per-token configuration for bearer auth with optional tool scoping.
///
/// Allows restricting which tools each bearer token can access, bridging
/// the gap between all-or-nothing bearer auth and full JWT/RBAC.
///
/// # Examples
///
/// ```
/// use mcp_proxy::config::BearerTokenConfig;
///
/// let frontend = BearerTokenConfig {
///     token: "frontend-token".into(),
///     allow_tools: vec!["files/read_file".into()],
///     deny_tools: vec![],
/// };
///
/// let admin = BearerTokenConfig {
///     token: "admin-token".into(),
///     allow_tools: vec![],
///     deny_tools: vec![],
/// };
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BearerTokenConfig {
    /// The bearer token value. Supports `${ENV_VAR}` syntax.
    pub token: String,
    /// Tools this token can access (namespaced, e.g. "files/read_file").
    /// Empty means all tools allowed.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools this token cannot access.
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

/// RBAC role definition.
#[derive(Debug, Deserialize, Serialize)]
pub struct RoleConfig {
    /// Role name, referenced by `RoleMappingConfig`.
    pub name: String,
    /// Tools this role can access (namespaced, e.g. "files/read_file")
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools this role cannot access
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

/// Maps JWT claim values to RBAC role names.
#[derive(Debug, Deserialize, Serialize)]
pub struct RoleMappingConfig {
    /// JWT claim to read for role resolution (e.g. "scope", "role", "groups")
    pub claim: String,
    /// Map claim values to role names
    pub mapping: HashMap<String, String>,
}

/// Tool alias: exposes a backend tool under a different name.
#[derive(Debug, Deserialize, Serialize)]
pub struct AliasConfig {
    /// Original tool name (backend-local, without namespace prefix)
    pub from: String,
    /// New tool name to expose (will be namespaced as backend/to)
    pub to: String,
}

/// Per-backend response cache configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct BackendCacheConfig {
    /// TTL for cached resource reads in seconds (0 = disabled)
    #[serde(default)]
    pub resource_ttl_seconds: u64,
    /// TTL for cached tool call results in seconds (0 = disabled)
    #[serde(default)]
    pub tool_ttl_seconds: u64,
    /// Maximum number of cached entries per backend (default: 1000)
    #[serde(default = "default_max_cache_entries")]
    pub max_entries: u64,
}

/// Global cache backend configuration.
///
/// Controls which storage backend is used for response caching. Per-backend
/// TTL and max_entries settings remain the same regardless of backend.
///
/// # Backends
///
/// - `"memory"` (default): In-process cache using moka. Fast, no external deps,
///   but not shared across proxy instances.
/// - `"redis"`: External Redis cache. Shared across instances. Requires the
///   `redis-cache` feature.
/// - `"sqlite"`: Local SQLite cache. Persistent across restarts. Requires the
///   `sqlite-cache` feature.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CacheBackendConfig {
    /// Cache backend type: "memory" (default), "redis", or "sqlite".
    #[serde(default = "default_cache_backend")]
    pub backend: String,
    /// Connection URL for external backends (Redis or SQLite path).
    pub url: Option<String>,
    /// Key prefix for external cache entries (default: "mcp-proxy:").
    #[serde(default = "default_cache_prefix")]
    pub prefix: String,
}

impl Default for CacheBackendConfig {
    fn default() -> Self {
        Self {
            backend: default_cache_backend(),
            url: None,
            prefix: default_cache_prefix(),
        }
    }
}

fn default_cache_backend() -> String {
    "memory".to_string()
}

fn default_cache_prefix() -> String {
    "mcp-proxy:".to_string()
}

/// Performance tuning options.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PerformanceConfig {
    /// Deduplicate identical concurrent tool calls and resource reads
    #[serde(default)]
    pub coalesce_requests: bool,
}

/// Security policies.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SecurityConfig {
    /// Maximum size of tool call arguments in bytes (default: unlimited)
    pub max_argument_size: Option<usize>,
}

/// Logging, metrics, and distributed tracing configuration.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ObservabilityConfig {
    /// Enable audit logging of all MCP requests (default: false).
    #[serde(default)]
    pub audit: bool,
    /// Log level filter (default: "info").
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Emit structured JSON logs (default: false).
    #[serde(default)]
    pub json_logs: bool,
    /// Prometheus metrics configuration.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// OpenTelemetry distributed tracing configuration.
    #[serde(default)]
    pub tracing: TracingConfig,
    /// Structured access logging configuration.
    #[serde(default)]
    pub access_log: AccessLogConfig,
}

/// Structured access log configuration.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct AccessLogConfig {
    /// Enable structured access logging (default: false).
    #[serde(default)]
    pub enabled: bool,
}

/// Prometheus metrics configuration.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MetricsConfig {
    /// Enable Prometheus metrics at `/admin/metrics` (default: false).
    #[serde(default)]
    pub enabled: bool,
}

/// OpenTelemetry distributed tracing configuration.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TracingConfig {
    /// Enable OTLP trace export (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// OTLP endpoint (default: http://localhost:4317)
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
    /// Service name for traces (default: "mcp-proxy")
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

// Defaults

fn default_version() -> String {
    "0.1.0".to_string()
}

fn default_separator() -> String {
    "/".to_string()
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_failure_rate() -> f64 {
    0.5
}

fn default_min_calls() -> usize {
    5
}

fn default_wait_duration() -> u64 {
    30
}

fn default_half_open_calls() -> usize {
    3
}

fn default_rate_period() -> u64 {
    1
}

fn default_max_retries() -> u32 {
    3
}

fn default_initial_backoff_ms() -> u64 {
    100
}

fn default_max_backoff_ms() -> u64 {
    5000
}

fn default_min_retries_per_sec() -> u32 {
    10
}

fn default_consecutive_errors() -> u32 {
    5
}

fn default_interval_seconds() -> u64 {
    10
}

fn default_base_ejection_seconds() -> u64 {
    30
}

fn default_max_ejection_percent() -> u32 {
    50
}

fn default_hedge_delay_ms() -> u64 {
    200
}

fn default_max_hedges() -> usize {
    1
}

fn default_mirror_percent() -> u32 {
    100
}

fn default_weight() -> u32 {
    100
}

fn default_max_cache_entries() -> u64 {
    1000
}

fn default_shutdown_timeout() -> u64 {
    30
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".to_string()
}

fn default_service_name() -> String {
    "mcp-proxy".to_string()
}

/// Resolved filter rules for a backend's capabilities.
#[derive(Debug, Clone)]
pub struct BackendFilter {
    /// Namespace prefix (e.g. "db/") this filter applies to.
    pub namespace: String,
    /// Filter for tool names.
    pub tool_filter: NameFilter,
    /// Filter for resource URIs.
    pub resource_filter: NameFilter,
    /// Filter for prompt names.
    pub prompt_filter: NameFilter,
    /// Hide tools with `destructive_hint = true`.
    pub hide_destructive: bool,
    /// Only allow tools with `read_only_hint = true`.
    pub read_only_only: bool,
}

/// A compiled pattern for name matching -- either a glob or a regex.
///
/// Constructed internally by [`NameFilter::allow_list`] and
/// [`NameFilter::deny_list`].
#[derive(Debug, Clone)]
pub enum CompiledPattern {
    /// A glob pattern (matched via `glob_match`).
    Glob(String),
    /// A pre-compiled regex pattern (from `re:` prefix).
    Regex(regex::Regex),
}

impl CompiledPattern {
    /// Compile a pattern string. Patterns prefixed with `re:` are treated as
    /// regular expressions; all others are treated as glob patterns.
    fn compile(pattern: &str) -> Result<Self> {
        if let Some(re_pat) = pattern.strip_prefix("re:") {
            let re = regex::Regex::new(re_pat)
                .with_context(|| format!("invalid regex in filter pattern: {pattern}"))?;
            Ok(Self::Regex(re))
        } else {
            Ok(Self::Glob(pattern.to_string()))
        }
    }

    /// Check if this pattern matches the given name.
    fn matches(&self, name: &str) -> bool {
        match self {
            Self::Glob(pat) => glob_match::glob_match(pat, name),
            Self::Regex(re) => re.is_match(name),
        }
    }
}

/// A name-based allow/deny filter.
///
/// Patterns support two syntaxes:
/// - **Glob** (default): `*` matches any sequence, `?` matches one character.
/// - **Regex** (`re:` prefix): e.g. `re:^list_.*$` uses the `regex` crate.
///
/// Regex patterns are compiled once at config parse time.
#[derive(Debug, Clone)]
pub enum NameFilter {
    /// No filtering -- everything passes.
    PassAll,
    /// Only items matching at least one pattern are allowed.
    AllowList(Vec<CompiledPattern>),
    /// Items matching any pattern are denied.
    DenyList(Vec<CompiledPattern>),
}

impl NameFilter {
    /// Build an allow-list filter from raw pattern strings.
    ///
    /// Patterns prefixed with `re:` are compiled as regular expressions;
    /// all others are treated as glob patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if any `re:` pattern contains invalid regex syntax.
    pub fn allow_list(patterns: impl IntoIterator<Item = String>) -> Result<Self> {
        let compiled: Result<Vec<_>> = patterns
            .into_iter()
            .map(|p| CompiledPattern::compile(&p))
            .collect();
        Ok(Self::AllowList(compiled?))
    }

    /// Build a deny-list filter from raw pattern strings.
    ///
    /// Patterns prefixed with `re:` are compiled as regular expressions;
    /// all others are treated as glob patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if any `re:` pattern contains invalid regex syntax.
    pub fn deny_list(patterns: impl IntoIterator<Item = String>) -> Result<Self> {
        let compiled: Result<Vec<_>> = patterns
            .into_iter()
            .map(|p| CompiledPattern::compile(&p))
            .collect();
        Ok(Self::DenyList(compiled?))
    }

    /// Check if a capability name is allowed by this filter.
    ///
    /// Supports glob patterns (`*`, `?`) and regex patterns (`re:` prefix).
    /// Exact strings match themselves.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_proxy::config::NameFilter;
    ///
    /// let filter = NameFilter::deny_list(["delete".to_string()]).unwrap();
    /// assert!(filter.allows("read"));
    /// assert!(!filter.allows("delete"));
    ///
    /// let filter = NameFilter::allow_list(["read".to_string()]).unwrap();
    /// assert!(filter.allows("read"));
    /// assert!(!filter.allows("write"));
    ///
    /// assert!(NameFilter::PassAll.allows("anything"));
    ///
    /// // Glob patterns
    /// let filter = NameFilter::allow_list(["*_file".to_string()]).unwrap();
    /// assert!(filter.allows("read_file"));
    /// assert!(filter.allows("write_file"));
    /// assert!(!filter.allows("query"));
    ///
    /// // Regex patterns
    /// let filter = NameFilter::allow_list(["re:^list_.*$".to_string()]).unwrap();
    /// assert!(filter.allows("list_files"));
    /// assert!(!filter.allows("get_files"));
    /// ```
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::PassAll => true,
            Self::AllowList(patterns) => patterns.iter().any(|p| p.matches(name)),
            Self::DenyList(patterns) => !patterns.iter().any(|p| p.matches(name)),
        }
    }
}

impl BackendConfig {
    /// Build a [`BackendFilter`] from this backend's expose/hide lists.
    /// Returns `None` if no filtering is configured.
    ///
    /// Canary and failover backends automatically hide all capabilities so
    /// their tools don't appear in `ListTools` responses (traffic reaches
    /// them via routing middleware, not direct tool calls).
    pub fn build_filter(&self, separator: &str) -> Result<Option<BackendFilter>> {
        // Canary and failover backends hide all capabilities -- tools are
        // accessed via routing middleware rewriting the primary namespace.
        if self.canary_of.is_some() || self.failover_for.is_some() {
            return Ok(Some(BackendFilter {
                namespace: format!("{}{}", self.name, separator),
                tool_filter: NameFilter::allow_list(std::iter::empty::<String>())?,
                resource_filter: NameFilter::allow_list(std::iter::empty::<String>())?,
                prompt_filter: NameFilter::allow_list(std::iter::empty::<String>())?,
                hide_destructive: false,
                read_only_only: false,
            }));
        }

        let tool_filter = if !self.expose_tools.is_empty() {
            NameFilter::allow_list(self.expose_tools.iter().cloned())?
        } else if !self.hide_tools.is_empty() {
            NameFilter::deny_list(self.hide_tools.iter().cloned())?
        } else {
            NameFilter::PassAll
        };

        let resource_filter = if !self.expose_resources.is_empty() {
            NameFilter::allow_list(self.expose_resources.iter().cloned())?
        } else if !self.hide_resources.is_empty() {
            NameFilter::deny_list(self.hide_resources.iter().cloned())?
        } else {
            NameFilter::PassAll
        };

        let prompt_filter = if !self.expose_prompts.is_empty() {
            NameFilter::allow_list(self.expose_prompts.iter().cloned())?
        } else if !self.hide_prompts.is_empty() {
            NameFilter::deny_list(self.hide_prompts.iter().cloned())?
        } else {
            NameFilter::PassAll
        };

        // Only create a filter if at least one dimension has filtering
        if matches!(tool_filter, NameFilter::PassAll)
            && matches!(resource_filter, NameFilter::PassAll)
            && matches!(prompt_filter, NameFilter::PassAll)
            && !self.hide_destructive
            && !self.read_only_only
        {
            return Ok(None);
        }

        Ok(Some(BackendFilter {
            namespace: format!("{}{}", self.name, separator),
            tool_filter,
            resource_filter,
            prompt_filter,
            hide_destructive: self.hide_destructive,
            read_only_only: self.read_only_only,
        }))
    }
}

impl ProxyConfig {
    /// Load and validate a config from a file path.
    ///
    /// If `import_backends` is set in the config, backends from the referenced
    /// `.mcp.json` file are merged (TOML backends take precedence on name conflicts).
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

        let mut config: Self = match path.extension().and_then(|e| e.to_str()) {
            #[cfg(feature = "yaml")]
            Some("yaml" | "yml") => serde_yaml::from_str(&content)
                .with_context(|| format!("parsing YAML {}", path.display()))?,
            #[cfg(not(feature = "yaml"))]
            Some("yaml" | "yml") => {
                anyhow::bail!(
                    "YAML config requires the 'yaml' feature. Rebuild with: cargo install mcp-proxy --features yaml"
                );
            }
            _ => toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?,
        };

        // Import backends from .mcp.json if configured
        if let Some(ref mcp_json_path) = config.proxy.import_backends {
            let mcp_path = if std::path::Path::new(mcp_json_path).is_relative() {
                // Resolve relative to config file directory
                path.parent().unwrap_or(Path::new(".")).join(mcp_json_path)
            } else {
                std::path::PathBuf::from(mcp_json_path)
            };

            let mcp_json = crate::mcp_json::McpJsonConfig::load(&mcp_path)
                .with_context(|| format!("importing backends from {}", mcp_path.display()))?;

            let existing_names: HashSet<String> =
                config.backends.iter().map(|b| b.name.clone()).collect();

            for backend in mcp_json.into_backends()? {
                if !existing_names.contains(&backend.name) {
                    config.backends.push(backend);
                }
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Build a minimal `ProxyConfig` from a `.mcp.json` file.
    ///
    /// This is a convenience mode for quick local development. The proxy name
    /// is derived from the file's parent directory (or the filename itself),
    /// and the server listens on `127.0.0.1:8080` with no middleware or auth.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use mcp_proxy::ProxyConfig;
    ///
    /// let config = ProxyConfig::from_mcp_json(Path::new(".mcp.json")).unwrap();
    /// assert_eq!(config.proxy.listen.host, "127.0.0.1");
    /// assert_eq!(config.proxy.listen.port, 8080);
    /// ```
    pub fn from_mcp_json(path: &Path) -> Result<Self> {
        let mcp_json = crate::mcp_json::McpJsonConfig::load(path)?;
        let backends = mcp_json.into_backends()?;

        // Derive a proxy name from the parent directory or filename
        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .or_else(|| path.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mcp-proxy".to_string());

        let config = Self {
            proxy: ProxySettings {
                name,
                version: default_version(),
                separator: default_separator(),
                listen: ListenConfig {
                    host: default_host(),
                    port: default_port(),
                },
                instructions: None,
                shutdown_timeout_seconds: default_shutdown_timeout(),
                hot_reload: false,
                import_backends: None,
                rate_limit: None,
                tool_discovery: false,
                tool_exposure: ToolExposure::default(),
            },
            backends,
            auth: None,
            performance: PerformanceConfig::default(),
            security: SecurityConfig::default(),
            cache: CacheBackendConfig::default(),
            observability: ObservabilityConfig::default(),
            composite_tools: Vec::new(),
        };

        config.validate()?;
        Ok(config)
    }

    /// Parse and validate a config from a TOML string.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_proxy::ProxyConfig;
    ///
    /// let config = ProxyConfig::parse(r#"
    ///     [proxy]
    ///     name = "my-proxy"
    ///     [proxy.listen]
    ///
    ///     [[backends]]
    ///     name = "echo"
    ///     transport = "stdio"
    ///     command = "echo"
    /// "#).unwrap();
    ///
    /// assert_eq!(config.proxy.name, "my-proxy");
    /// assert_eq!(config.backends.len(), 1);
    /// ```
    pub fn parse(toml: &str) -> Result<Self> {
        let config: Self = toml::from_str(toml).context("parsing config")?;
        config.validate()?;
        Ok(config)
    }

    /// Parse and validate a config from a YAML string.
    ///
    /// # Examples
    ///
    /// ```
    /// use mcp_proxy::ProxyConfig;
    ///
    /// let config = ProxyConfig::parse_yaml(r#"
    /// proxy:
    ///   name: my-proxy
    ///   listen:
    ///     host: "127.0.0.1"
    ///     port: 8080
    /// backends:
    ///   - name: echo
    ///     transport: stdio
    ///     command: echo
    /// "#).unwrap();
    ///
    /// assert_eq!(config.proxy.name, "my-proxy");
    /// ```
    #[cfg(feature = "yaml")]
    pub fn parse_yaml(yaml: &str) -> Result<Self> {
        let config: Self = serde_yaml::from_str(yaml).context("parsing YAML config")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.backends.is_empty() {
            anyhow::bail!("at least one backend is required");
        }

        // Validate cache backend
        match self.cache.backend.as_str() {
            "memory" => {}
            "redis" => {
                if self.cache.url.is_none() {
                    anyhow::bail!(
                        "cache.url is required when cache.backend = \"{}\"",
                        self.cache.backend
                    );
                }
                #[cfg(not(feature = "redis-cache"))]
                anyhow::bail!(
                    "cache.backend = \"redis\" requires the 'redis-cache' feature. \
                     Rebuild with: cargo install mcp-proxy --features redis-cache"
                );
            }
            "sqlite" => {
                if self.cache.url.is_none() {
                    anyhow::bail!(
                        "cache.url is required when cache.backend = \"{}\"",
                        self.cache.backend
                    );
                }
                #[cfg(not(feature = "sqlite-cache"))]
                anyhow::bail!(
                    "cache.backend = \"sqlite\" requires the 'sqlite-cache' feature. \
                     Rebuild with: cargo install mcp-proxy --features sqlite-cache"
                );
            }
            other => {
                anyhow::bail!(
                    "unknown cache backend \"{}\", expected \"memory\", \"redis\", or \"sqlite\"",
                    other
                );
            }
        }

        // Validate global rate limit
        if let Some(rl) = &self.proxy.rate_limit {
            if rl.requests == 0 {
                anyhow::bail!("proxy.rate_limit.requests must be > 0");
            }
            if rl.period_seconds == 0 {
                anyhow::bail!("proxy.rate_limit.period_seconds must be > 0");
            }
        }

        // Validate bearer auth config
        if let Some(AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) = &self.auth
        {
            if tokens.is_empty() && scoped_tokens.is_empty() {
                anyhow::bail!(
                    "bearer auth requires at least one token in 'tokens' or 'scoped_tokens'"
                );
            }
            // Check for duplicate tokens across both lists
            let mut seen_tokens = HashSet::new();
            for t in tokens {
                if !seen_tokens.insert(t.as_str()) {
                    anyhow::bail!("duplicate bearer token in 'tokens'");
                }
            }
            for st in scoped_tokens {
                if !seen_tokens.insert(st.token.as_str()) {
                    anyhow::bail!(
                        "duplicate bearer token (appears in both 'tokens' and 'scoped_tokens' or duplicated within 'scoped_tokens')"
                    );
                }
                if !st.allow_tools.is_empty() && !st.deny_tools.is_empty() {
                    anyhow::bail!(
                        "scoped_tokens: cannot specify both allow_tools and deny_tools for the same token"
                    );
                }
            }
        }

        // Validate OAuth config
        if let Some(AuthConfig::OAuth {
            token_validation,
            client_id,
            client_secret,
            ..
        }) = &self.auth
            && matches!(
                token_validation,
                TokenValidationStrategy::Introspection | TokenValidationStrategy::Both
            )
            && (client_id.is_none() || client_secret.is_none())
        {
            anyhow::bail!("OAuth introspection requires both 'client_id' and 'client_secret'");
        }

        // Check for duplicate backend names
        let mut seen_names = HashSet::new();
        for backend in &self.backends {
            if !seen_names.insert(&backend.name) {
                anyhow::bail!("duplicate backend name '{}'", backend.name);
            }
        }

        for backend in &self.backends {
            match backend.transport {
                TransportType::Stdio => {
                    if backend.command.is_none() {
                        anyhow::bail!(
                            "backend '{}': stdio transport requires 'command'",
                            backend.name
                        );
                    }
                }
                TransportType::Http => {
                    if backend.url.is_none() {
                        anyhow::bail!("backend '{}': http transport requires 'url'", backend.name);
                    }
                }
                TransportType::Websocket => {
                    if backend.url.is_none() {
                        anyhow::bail!(
                            "backend '{}': websocket transport requires 'url'",
                            backend.name
                        );
                    }
                }
            }

            if let Some(cb) = &backend.circuit_breaker
                && (cb.failure_rate_threshold <= 0.0 || cb.failure_rate_threshold > 1.0)
            {
                anyhow::bail!(
                    "backend '{}': circuit_breaker.failure_rate_threshold must be in (0.0, 1.0]",
                    backend.name
                );
            }

            if let Some(rl) = &backend.rate_limit
                && rl.requests == 0
            {
                anyhow::bail!(
                    "backend '{}': rate_limit.requests must be > 0",
                    backend.name
                );
            }

            if let Some(cc) = &backend.concurrency
                && cc.max_concurrent == 0
            {
                anyhow::bail!(
                    "backend '{}': concurrency.max_concurrent must be > 0",
                    backend.name
                );
            }

            if !backend.expose_tools.is_empty() && !backend.hide_tools.is_empty() {
                anyhow::bail!(
                    "backend '{}': cannot specify both expose_tools and hide_tools",
                    backend.name
                );
            }
            if !backend.expose_resources.is_empty() && !backend.hide_resources.is_empty() {
                anyhow::bail!(
                    "backend '{}': cannot specify both expose_resources and hide_resources",
                    backend.name
                );
            }
            if !backend.expose_prompts.is_empty() && !backend.hide_prompts.is_empty() {
                anyhow::bail!(
                    "backend '{}': cannot specify both expose_prompts and hide_prompts",
                    backend.name
                );
            }
        }

        // Validate mirror_of references
        let backend_names: HashSet<&str> = self.backends.iter().map(|b| b.name.as_str()).collect();
        for backend in &self.backends {
            if let Some(ref source) = backend.mirror_of {
                if !backend_names.contains(source.as_str()) {
                    anyhow::bail!(
                        "backend '{}': mirror_of references unknown backend '{}'",
                        backend.name,
                        source
                    );
                }
                if source == &backend.name {
                    anyhow::bail!(
                        "backend '{}': mirror_of cannot reference itself",
                        backend.name
                    );
                }
            }
        }

        // Validate failover_for references
        for backend in &self.backends {
            if let Some(ref primary) = backend.failover_for {
                if !backend_names.contains(primary.as_str()) {
                    anyhow::bail!(
                        "backend '{}': failover_for references unknown backend '{}'",
                        backend.name,
                        primary
                    );
                }
                if primary == &backend.name {
                    anyhow::bail!(
                        "backend '{}': failover_for cannot reference itself",
                        backend.name
                    );
                }
            }
        }

        // Validate composite tools
        {
            let mut composite_names = HashSet::new();
            for ct in &self.composite_tools {
                if ct.name.is_empty() {
                    anyhow::bail!("composite_tools: name must not be empty");
                }
                if ct.tools.is_empty() {
                    anyhow::bail!(
                        "composite_tools '{}': must reference at least one tool",
                        ct.name
                    );
                }
                if !composite_names.insert(&ct.name) {
                    anyhow::bail!("duplicate composite_tools name '{}'", ct.name);
                }
            }
        }

        // Validate canary_of references
        for backend in &self.backends {
            if let Some(ref primary) = backend.canary_of {
                if !backend_names.contains(primary.as_str()) {
                    anyhow::bail!(
                        "backend '{}': canary_of references unknown backend '{}'",
                        backend.name,
                        primary
                    );
                }
                if primary == &backend.name {
                    anyhow::bail!(
                        "backend '{}': canary_of cannot reference itself",
                        backend.name
                    );
                }
                if backend.weight == 0 {
                    anyhow::bail!("backend '{}': weight must be > 0", backend.name);
                }
            }
        }

        // Validate tool_exposure = "search" requires the discovery feature
        #[cfg(not(feature = "discovery"))]
        if self.proxy.tool_exposure == ToolExposure::Search {
            anyhow::bail!(
                "tool_exposure = \"search\" requires the 'discovery' feature. \
                 Rebuild with: cargo install mcp-proxy --features discovery"
            );
        }

        // Validate param_overrides
        for backend in &self.backends {
            let mut seen_tools = HashSet::new();
            for po in &backend.param_overrides {
                if po.tool.is_empty() {
                    anyhow::bail!(
                        "backend '{}': param_overrides.tool must not be empty",
                        backend.name
                    );
                }
                if !seen_tools.insert(&po.tool) {
                    anyhow::bail!(
                        "backend '{}': duplicate param_overrides for tool '{}'",
                        backend.name,
                        po.tool
                    );
                }
                // Hidden params that have no default are a warning-level concern,
                // but renamed params that conflict with hide are an error.
                for hidden in &po.hide {
                    if po.rename.contains_key(hidden) {
                        anyhow::bail!(
                            "backend '{}': param_overrides for tool '{}': \
                             parameter '{}' cannot be both hidden and renamed",
                            backend.name,
                            po.tool,
                            hidden
                        );
                    }
                }
                // Check for rename target conflicts (two originals mapping to same name)
                let mut rename_targets = HashSet::new();
                for target in po.rename.values() {
                    if !rename_targets.insert(target) {
                        anyhow::bail!(
                            "backend '{}': param_overrides for tool '{}': \
                             duplicate rename target '{}'",
                            backend.name,
                            po.tool,
                            target
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Resolve environment variable references in config values.
    /// Replaces `${VAR_NAME}` with the value of the environment variable.
    pub fn resolve_env_vars(&mut self) {
        for backend in &mut self.backends {
            for value in backend.env.values_mut() {
                if let Some(var_name) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                    && let Ok(env_val) = std::env::var(var_name)
                {
                    *value = env_val;
                }
            }
            if let Some(ref mut token) = backend.bearer_token
                && let Some(var_name) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                && let Ok(env_val) = std::env::var(var_name)
            {
                *token = env_val;
            }
        }

        // Resolve env vars in auth config
        if let Some(AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) = &mut self.auth
        {
            for token in tokens.iter_mut() {
                if let Some(var_name) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                    && let Ok(env_val) = std::env::var(var_name)
                {
                    *token = env_val;
                }
            }
            for st in scoped_tokens.iter_mut() {
                if let Some(var_name) = st
                    .token
                    .strip_prefix("${")
                    .and_then(|s| s.strip_suffix('}'))
                    && let Ok(env_val) = std::env::var(var_name)
                {
                    st.token = env_val;
                }
            }
        }

        // Resolve env vars in OAuth config
        if let Some(AuthConfig::OAuth { client_secret, .. }) = &mut self.auth
            && let Some(secret) = client_secret
            && let Some(var_name) = secret.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
            && let Ok(env_val) = std::env::var(var_name)
        {
            *secret = env_val;
        }
    }

    /// Check for `${VAR}` references where the environment variable is not set.
    ///
    /// Returns a list of human-readable warning strings. This method does not
    /// modify the config or fail -- it only reports potential issues.
    ///
    /// # Example
    ///
    /// ```
    /// use mcp_proxy::config::ProxyConfig;
    ///
    /// let toml = r#"
    /// [proxy]
    /// name = "test"
    /// [proxy.listen]
    ///
    /// [[backends]]
    /// name = "svc"
    /// transport = "stdio"
    /// command = "echo"
    /// bearer_token = "${UNSET_VAR}"
    /// "#;
    ///
    /// let config = ProxyConfig::parse(toml).unwrap();
    /// let warnings = config.check_env_vars();
    /// assert!(!warnings.is_empty());
    /// ```
    pub fn check_env_vars(&self) -> Vec<String> {
        fn is_unset_env_ref(value: &str) -> Option<&str> {
            let var_name = value.strip_prefix("${").and_then(|s| s.strip_suffix('}'))?;
            if std::env::var(var_name).is_err() {
                Some(var_name)
            } else {
                None
            }
        }

        let mut warnings = Vec::new();

        for backend in &self.backends {
            // backend.bearer_token
            if let Some(ref token) = backend.bearer_token
                && let Some(var) = is_unset_env_ref(token)
            {
                warnings.push(format!(
                    "backend '{}': bearer_token references unset env var '{}'",
                    backend.name, var
                ));
            }
            // backend.env values
            for (key, value) in &backend.env {
                if let Some(var) = is_unset_env_ref(value) {
                    warnings.push(format!(
                        "backend '{}': env.{} references unset env var '{}'",
                        backend.name, key, var
                    ));
                }
            }
        }

        match &self.auth {
            Some(AuthConfig::Bearer {
                tokens,
                scoped_tokens,
            }) => {
                for (i, token) in tokens.iter().enumerate() {
                    if let Some(var) = is_unset_env_ref(token) {
                        warnings.push(format!(
                            "auth.bearer: tokens[{}] references unset env var '{}'",
                            i, var
                        ));
                    }
                }
                for (i, st) in scoped_tokens.iter().enumerate() {
                    if let Some(var) = is_unset_env_ref(&st.token) {
                        warnings.push(format!(
                            "auth.bearer: scoped_tokens[{}] references unset env var '{}'",
                            i, var
                        ));
                    }
                }
            }
            Some(AuthConfig::OAuth {
                client_secret: Some(secret),
                ..
            }) => {
                if let Some(var) = is_unset_env_ref(secret) {
                    warnings.push(format!(
                        "auth.oauth: client_secret references unset env var '{}'",
                        var
                    ));
                }
            }
            _ => {}
        }

        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> &'static str {
        r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#
    }

    #[test]
    fn test_parse_minimal_config() {
        let config = ProxyConfig::parse(minimal_config()).unwrap();
        assert_eq!(config.proxy.name, "test");
        assert_eq!(config.proxy.version, "0.1.0"); // default
        assert_eq!(config.proxy.separator, "/"); // default
        assert_eq!(config.proxy.listen.host, "127.0.0.1"); // default
        assert_eq!(config.proxy.listen.port, 8080); // default
        assert_eq!(config.proxy.shutdown_timeout_seconds, 30); // default
        assert!(!config.proxy.hot_reload); // default false
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].name, "echo");
        assert!(config.auth.is_none());
        assert!(!config.observability.audit);
        assert!(!config.observability.metrics.enabled);
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
        [proxy]
        name = "full-gw"
        version = "2.0.0"
        separator = "."
        shutdown_timeout_seconds = 60
        hot_reload = true
        instructions = "A test proxy"
        [proxy.listen]
        host = "0.0.0.0"
        port = 9090

        [[backends]]
        name = "files"
        transport = "stdio"
        command = "file-server"
        args = ["--root", "/tmp"]
        expose_tools = ["read_file"]

        [backends.env]
        LOG_LEVEL = "debug"

        [backends.timeout]
        seconds = 30

        [backends.concurrency]
        max_concurrent = 5

        [backends.rate_limit]
        requests = 100
        period_seconds = 10

        [backends.circuit_breaker]
        failure_rate_threshold = 0.5
        minimum_calls = 10
        wait_duration_seconds = 60
        permitted_calls_in_half_open = 2

        [backends.cache]
        resource_ttl_seconds = 300
        tool_ttl_seconds = 60
        max_entries = 500

        [[backends.aliases]]
        from = "read_file"
        to = "read"

        [[backends]]
        name = "remote"
        transport = "http"
        url = "http://localhost:3000"

        [observability]
        audit = true
        log_level = "debug"
        json_logs = true

        [observability.metrics]
        enabled = true

        [observability.tracing]
        enabled = true
        endpoint = "http://jaeger:4317"
        service_name = "test-gw"

        [performance]
        coalesce_requests = true

        [security]
        max_argument_size = 1048576
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.proxy.name, "full-gw");
        assert_eq!(config.proxy.version, "2.0.0");
        assert_eq!(config.proxy.separator, ".");
        assert_eq!(config.proxy.shutdown_timeout_seconds, 60);
        assert!(config.proxy.hot_reload);
        assert_eq!(config.proxy.instructions.as_deref(), Some("A test proxy"));
        assert_eq!(config.proxy.listen.host, "0.0.0.0");
        assert_eq!(config.proxy.listen.port, 9090);

        assert_eq!(config.backends.len(), 2);

        let files = &config.backends[0];
        assert_eq!(files.command.as_deref(), Some("file-server"));
        assert_eq!(files.args, vec!["--root", "/tmp"]);
        assert_eq!(files.expose_tools, vec!["read_file"]);
        assert_eq!(files.env.get("LOG_LEVEL").unwrap(), "debug");
        assert_eq!(files.timeout.as_ref().unwrap().seconds, 30);
        assert_eq!(files.concurrency.as_ref().unwrap().max_concurrent, 5);
        assert_eq!(files.rate_limit.as_ref().unwrap().requests, 100);
        assert_eq!(files.cache.as_ref().unwrap().resource_ttl_seconds, 300);
        assert_eq!(files.cache.as_ref().unwrap().tool_ttl_seconds, 60);
        assert_eq!(files.cache.as_ref().unwrap().max_entries, 500);
        assert_eq!(files.aliases.len(), 1);
        assert_eq!(files.aliases[0].from, "read_file");
        assert_eq!(files.aliases[0].to, "read");

        let cb = files.circuit_breaker.as_ref().unwrap();
        assert_eq!(cb.failure_rate_threshold, 0.5);
        assert_eq!(cb.minimum_calls, 10);
        assert_eq!(cb.wait_duration_seconds, 60);
        assert_eq!(cb.permitted_calls_in_half_open, 2);

        let remote = &config.backends[1];
        assert_eq!(remote.url.as_deref(), Some("http://localhost:3000"));

        assert!(config.observability.audit);
        assert_eq!(config.observability.log_level, "debug");
        assert!(config.observability.json_logs);
        assert!(config.observability.metrics.enabled);
        assert!(config.observability.tracing.enabled);
        assert_eq!(config.observability.tracing.endpoint, "http://jaeger:4317");

        assert!(config.performance.coalesce_requests);
        assert_eq!(config.security.max_argument_size, Some(1048576));
    }

    #[test]
    fn test_parse_bearer_auth() {
        let toml = r#"
        [proxy]
        name = "auth-gw"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        tokens = ["token-1", "token-2"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::Bearer { tokens, .. }) => {
                assert_eq!(tokens, &["token-1", "token-2"]);
            }
            other => panic!("expected Bearer auth, got: {:?}", other),
        }
    }

    #[test]
    fn test_parse_jwt_auth_with_rbac() {
        let toml = r#"
        [proxy]
        name = "jwt-gw"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "jwt"
        issuer = "https://auth.example.com"
        audience = "mcp-proxy"
        jwks_uri = "https://auth.example.com/.well-known/jwks.json"

        [[auth.roles]]
        name = "reader"
        allow_tools = ["echo/read"]

        [[auth.roles]]
        name = "admin"

        [auth.role_mapping]
        claim = "scope"
        mapping = { "mcp:read" = "reader", "mcp:admin" = "admin" }
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::Jwt {
                issuer,
                audience,
                jwks_uri,
                roles,
                role_mapping,
            }) => {
                assert_eq!(issuer, "https://auth.example.com");
                assert_eq!(audience, "mcp-proxy");
                assert_eq!(jwks_uri, "https://auth.example.com/.well-known/jwks.json");
                assert_eq!(roles.len(), 2);
                assert_eq!(roles[0].name, "reader");
                assert_eq!(roles[0].allow_tools, vec!["echo/read"]);
                let mapping = role_mapping.as_ref().unwrap();
                assert_eq!(mapping.claim, "scope");
                assert_eq!(mapping.mapping.get("mcp:read").unwrap(), "reader");
            }
            other => panic!("expected Jwt auth, got: {:?}", other),
        }
    }

    // ========================================================================
    // Validation errors
    // ========================================================================

    #[test]
    fn test_reject_no_backends() {
        let toml = r#"
        [proxy]
        name = "empty"
        [proxy.listen]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("at least one backend"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_stdio_without_command() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "broken"
        transport = "stdio"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("stdio transport requires 'command'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_http_without_url() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "broken"
        transport = "http"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("http transport requires 'url'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_invalid_circuit_breaker_threshold() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.circuit_breaker]
        failure_rate_threshold = 1.5
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("failure_rate_threshold must be in (0.0, 1.0]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_zero_rate_limit() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.rate_limit]
        requests = 0
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("rate_limit.requests must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_zero_concurrency() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.concurrency]
        max_concurrent = 0
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("concurrency.max_concurrent must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_tools() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_tools = ["read"]
        hide_tools = ["write"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot specify both expose_tools and hide_tools"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_resources() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_resources = ["file:///a"]
        hide_resources = ["file:///b"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot specify both expose_resources and hide_resources"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_prompts() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_prompts = ["help"]
        hide_prompts = ["admin"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot specify both expose_prompts and hide_prompts"),
            "unexpected error: {err}"
        );
    }

    // ========================================================================
    // Env var resolution
    // ========================================================================

    #[test]
    fn test_resolve_env_vars() {
        // SAFETY: test runs single-threaded, no other threads reading this var
        unsafe { std::env::set_var("MCP_GW_TEST_TOKEN", "secret-123") };

        let toml = r#"
        [proxy]
        name = "env-test"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.env]
        API_TOKEN = "${MCP_GW_TEST_TOKEN}"
        STATIC_VAL = "unchanged"
        "#;

        let mut config = ProxyConfig::parse(toml).unwrap();
        config.resolve_env_vars();

        assert_eq!(
            config.backends[0].env.get("API_TOKEN").unwrap(),
            "secret-123"
        );
        assert_eq!(
            config.backends[0].env.get("STATIC_VAL").unwrap(),
            "unchanged"
        );

        // SAFETY: same as above
        unsafe { std::env::remove_var("MCP_GW_TEST_TOKEN") };
    }

    #[test]
    fn test_parse_bearer_token_and_forward_auth() {
        let toml = r#"
        [proxy]
        name = "token-gw"
        [proxy.listen]

        [[backends]]
        name = "github"
        transport = "http"
        url = "http://localhost:3000"
        bearer_token = "ghp_abc123"
        forward_auth = true

        [[backends]]
        name = "db"
        transport = "http"
        url = "http://localhost:5432"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(
            config.backends[0].bearer_token.as_deref(),
            Some("ghp_abc123")
        );
        assert!(config.backends[0].forward_auth);
        assert!(config.backends[1].bearer_token.is_none());
        assert!(!config.backends[1].forward_auth);
    }

    #[test]
    fn test_resolve_bearer_token_env_var() {
        unsafe { std::env::set_var("MCP_GW_TEST_BEARER", "resolved-token") };

        let toml = r#"
        [proxy]
        name = "env-token"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:3000"
        bearer_token = "${MCP_GW_TEST_BEARER}"
        "#;

        let mut config = ProxyConfig::parse(toml).unwrap();
        config.resolve_env_vars();

        assert_eq!(
            config.backends[0].bearer_token.as_deref(),
            Some("resolved-token")
        );

        unsafe { std::env::remove_var("MCP_GW_TEST_BEARER") };
    }

    #[test]
    fn test_parse_outlier_detection() {
        let toml = r#"
        [proxy]
        name = "od-gw"
        [proxy.listen]

        [[backends]]
        name = "flaky"
        transport = "http"
        url = "http://localhost:8080"

        [backends.outlier_detection]
        consecutive_errors = 3
        interval_seconds = 5
        base_ejection_seconds = 60
        max_ejection_percent = 25
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let od = config.backends[0]
            .outlier_detection
            .as_ref()
            .expect("should have outlier_detection");
        assert_eq!(od.consecutive_errors, 3);
        assert_eq!(od.interval_seconds, 5);
        assert_eq!(od.base_ejection_seconds, 60);
        assert_eq!(od.max_ejection_percent, 25);
    }

    #[test]
    fn test_parse_outlier_detection_defaults() {
        let toml = r#"
        [proxy]
        name = "od-gw"
        [proxy.listen]

        [[backends]]
        name = "flaky"
        transport = "http"
        url = "http://localhost:8080"

        [backends.outlier_detection]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let od = config.backends[0]
            .outlier_detection
            .as_ref()
            .expect("should have outlier_detection");
        assert_eq!(od.consecutive_errors, 5);
        assert_eq!(od.interval_seconds, 10);
        assert_eq!(od.base_ejection_seconds, 30);
        assert_eq!(od.max_ejection_percent, 50);
    }

    #[test]
    fn test_parse_mirror_config() {
        let toml = r#"
        [proxy]
        name = "mirror-gw"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [[backends]]
        name = "api-v2"
        transport = "http"
        url = "http://localhost:8081"
        mirror_of = "api"
        mirror_percent = 10
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert!(config.backends[0].mirror_of.is_none());
        assert_eq!(config.backends[1].mirror_of.as_deref(), Some("api"));
        assert_eq!(config.backends[1].mirror_percent, 10);
    }

    #[test]
    fn test_mirror_percent_defaults_to_100() {
        let toml = r#"
        [proxy]
        name = "mirror-gw"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [[backends]]
        name = "api-v2"
        transport = "http"
        url = "http://localhost:8081"
        mirror_of = "api"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.backends[1].mirror_percent, 100);
    }

    #[test]
    fn test_reject_mirror_unknown_backend() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "api-v2"
        transport = "http"
        url = "http://localhost:8081"
        mirror_of = "nonexistent"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("mirror_of references unknown backend"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_mirror_self() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"
        mirror_of = "api"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("mirror_of cannot reference itself"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_hedging_config() {
        let toml = r#"
        [proxy]
        name = "hedge-gw"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [backends.hedging]
        delay_ms = 150
        max_hedges = 2
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let hedge = config.backends[0]
            .hedging
            .as_ref()
            .expect("should have hedging");
        assert_eq!(hedge.delay_ms, 150);
        assert_eq!(hedge.max_hedges, 2);
    }

    #[test]
    fn test_parse_hedging_defaults() {
        let toml = r#"
        [proxy]
        name = "hedge-gw"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [backends.hedging]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let hedge = config.backends[0]
            .hedging
            .as_ref()
            .expect("should have hedging");
        assert_eq!(hedge.delay_ms, 200);
        assert_eq!(hedge.max_hedges, 1);
    }

    // ========================================================================
    // Capability filter building
    // ========================================================================

    #[test]
    fn test_build_filter_allowlist() {
        let toml = r#"
        [proxy]
        name = "filter"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_tools = ["read", "list"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let filter = config.backends[0]
            .build_filter(&config.proxy.separator)
            .unwrap()
            .expect("should have filter");
        assert_eq!(filter.namespace, "svc/");
        assert!(filter.tool_filter.allows("read"));
        assert!(filter.tool_filter.allows("list"));
        assert!(!filter.tool_filter.allows("delete"));
    }

    #[test]
    fn test_build_filter_denylist() {
        let toml = r#"
        [proxy]
        name = "filter"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        hide_tools = ["delete", "write"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let filter = config.backends[0]
            .build_filter(&config.proxy.separator)
            .unwrap()
            .expect("should have filter");
        assert!(filter.tool_filter.allows("read"));
        assert!(!filter.tool_filter.allows("delete"));
        assert!(!filter.tool_filter.allows("write"));
    }

    #[test]
    fn test_parse_inject_args() {
        let toml = r#"
        [proxy]
        name = "inject-gw"
        [proxy.listen]

        [[backends]]
        name = "db"
        transport = "http"
        url = "http://localhost:8080"

        [backends.default_args]
        timeout = 30

        [[backends.inject_args]]
        tool = "query"
        args = { read_only = true, max_rows = 1000 }

        [[backends.inject_args]]
        tool = "dangerous_op"
        args = { dry_run = true }
        overwrite = true
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let backend = &config.backends[0];

        assert_eq!(backend.default_args.len(), 1);
        assert_eq!(backend.default_args["timeout"], 30);

        assert_eq!(backend.inject_args.len(), 2);
        assert_eq!(backend.inject_args[0].tool, "query");
        assert_eq!(backend.inject_args[0].args["read_only"], true);
        assert_eq!(backend.inject_args[0].args["max_rows"], 1000);
        assert!(!backend.inject_args[0].overwrite);

        assert_eq!(backend.inject_args[1].tool, "dangerous_op");
        assert_eq!(backend.inject_args[1].args["dry_run"], true);
        assert!(backend.inject_args[1].overwrite);
    }

    #[test]
    fn test_parse_inject_args_defaults_to_empty() {
        let config = ProxyConfig::parse(minimal_config()).unwrap();
        assert!(config.backends[0].default_args.is_empty());
        assert!(config.backends[0].inject_args.is_empty());
    }

    #[test]
    fn test_build_filter_none_when_no_filtering() {
        let config = ProxyConfig::parse(minimal_config()).unwrap();
        assert!(
            config.backends[0]
                .build_filter(&config.proxy.separator)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_validate_rejects_duplicate_backend_names() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "cat"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string().contains("duplicate backend name"),
            "expected duplicate error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_global_rate_limit_zero_requests() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        [proxy.rate_limit]
        requests = 0

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("requests must be > 0"));
    }

    #[test]
    fn test_parse_global_rate_limit() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        [proxy.rate_limit]
        requests = 500
        period_seconds = 1

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;
        let config = ProxyConfig::parse(toml).unwrap();
        let rl = config.proxy.rate_limit.unwrap();
        assert_eq!(rl.requests, 500);
        assert_eq!(rl.period_seconds, 1);
    }

    #[test]
    fn test_name_filter_glob_wildcard() {
        let filter = NameFilter::allow_list(["*_file".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(filter.allows("write_file"));
        assert!(!filter.allows("query"));
        assert!(!filter.allows("file_read"));
    }

    #[test]
    fn test_name_filter_glob_prefix() {
        let filter = NameFilter::allow_list(["list_*".to_string()]).unwrap();
        assert!(filter.allows("list_files"));
        assert!(filter.allows("list_users"));
        assert!(!filter.allows("get_files"));
    }

    #[test]
    fn test_name_filter_glob_question_mark() {
        let filter = NameFilter::allow_list(["get_?".to_string()]).unwrap();
        assert!(filter.allows("get_a"));
        assert!(filter.allows("get_1"));
        assert!(!filter.allows("get_ab"));
        assert!(!filter.allows("get_"));
    }

    #[test]
    fn test_name_filter_glob_deny_list() {
        let filter = NameFilter::deny_list(["*_delete*".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(filter.allows("create_issue"));
        assert!(!filter.allows("force_delete_all"));
        assert!(!filter.allows("soft_delete"));
    }

    #[test]
    fn test_name_filter_glob_exact_match_still_works() {
        let filter = NameFilter::allow_list(["read_file".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(!filter.allows("write_file"));
    }

    #[test]
    fn test_name_filter_glob_multiple_patterns() {
        let filter = NameFilter::allow_list(["read_*".to_string(), "list_*".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(filter.allows("list_users"));
        assert!(!filter.allows("delete_file"));
    }

    #[test]
    fn test_name_filter_regex_allow_list() {
        let filter =
            NameFilter::allow_list(["re:^list_.*$".to_string(), "re:^get_\\w+$".to_string()])
                .unwrap();
        assert!(filter.allows("list_files"));
        assert!(filter.allows("list_users"));
        assert!(filter.allows("get_item"));
        assert!(!filter.allows("delete_file"));
        assert!(!filter.allows("create_issue"));
    }

    #[test]
    fn test_name_filter_regex_deny_list() {
        let filter = NameFilter::deny_list(["re:^delete_".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(filter.allows("list_users"));
        assert!(!filter.allows("delete_file"));
        assert!(!filter.allows("delete_all"));
    }

    #[test]
    fn test_name_filter_mixed_glob_and_regex() {
        let filter =
            NameFilter::allow_list(["read_*".to_string(), "re:^list_\\w+$".to_string()]).unwrap();
        assert!(filter.allows("read_file"));
        assert!(filter.allows("read_dir"));
        assert!(filter.allows("list_users"));
        assert!(!filter.allows("delete_file"));
    }

    #[test]
    fn test_name_filter_regex_invalid_pattern() {
        let result = NameFilter::allow_list(["re:[invalid".to_string()]);
        assert!(result.is_err(), "invalid regex should produce an error");
    }

    #[test]
    fn test_name_filter_regex_partial_match() {
        // Regex without anchors matches substrings
        let filter = NameFilter::allow_list(["re:list".to_string()]).unwrap();
        assert!(filter.allows("list_files"));
        assert!(filter.allows("my_list_tool"));
        assert!(!filter.allows("read_file"));
    }

    #[test]
    fn test_config_parse_regex_filter() {
        let toml = r#"
        [proxy]
        name = "regex-gw"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_tools = ["*_issue", "re:^list_.*$"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let filter = config.backends[0]
            .build_filter(&config.proxy.separator)
            .unwrap()
            .expect("should have filter");
        assert!(filter.tool_filter.allows("create_issue"));
        assert!(filter.tool_filter.allows("list_files"));
        assert!(filter.tool_filter.allows("list_users"));
        assert!(!filter.tool_filter.allows("delete_file"));
    }

    #[test]
    fn test_parse_param_overrides() {
        let toml = r#"
        [proxy]
        name = "override-gw"
        [proxy.listen]

        [[backends]]
        name = "fs"
        transport = "http"
        url = "http://localhost:8080"

        [[backends.param_overrides]]
        tool = "list_directory"
        hide = ["path"]
        rename = { recursive = "deep_search" }

        [backends.param_overrides.defaults]
        path = "/home/docs"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.backends[0].param_overrides.len(), 1);
        let po = &config.backends[0].param_overrides[0];
        assert_eq!(po.tool, "list_directory");
        assert_eq!(po.hide, vec!["path"]);
        assert_eq!(po.defaults.get("path").unwrap(), "/home/docs");
        assert_eq!(po.rename.get("recursive").unwrap(), "deep_search");
    }

    #[test]
    fn test_reject_param_override_empty_tool() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "fs"
        transport = "http"
        url = "http://localhost:8080"

        [[backends.param_overrides]]
        tool = ""
        hide = ["path"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("tool must not be empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_param_override_duplicate_tool() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "fs"
        transport = "http"
        url = "http://localhost:8080"

        [[backends.param_overrides]]
        tool = "list_directory"
        hide = ["path"]

        [[backends.param_overrides]]
        tool = "list_directory"
        hide = ["pattern"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("duplicate param_overrides"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_param_override_hide_and_rename_same_param() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "fs"
        transport = "http"
        url = "http://localhost:8080"

        [[backends.param_overrides]]
        tool = "list_directory"
        hide = ["path"]
        rename = { path = "dir" }
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot be both hidden and renamed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_param_override_duplicate_rename_target() {
        let toml = r#"
        [proxy]
        name = "bad"
        [proxy.listen]

        [[backends]]
        name = "fs"
        transport = "http"
        url = "http://localhost:8080"

        [[backends.param_overrides]]
        tool = "list_directory"
        rename = { path = "location", dir = "location" }
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("duplicate rename target"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_cache_backend_defaults_to_memory() {
        let config = ProxyConfig::parse(minimal_config()).unwrap();
        assert_eq!(config.cache.backend, "memory");
        assert!(config.cache.url.is_none());
    }

    #[test]
    fn test_cache_backend_redis_requires_url() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        [cache]
        backend = "redis"

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("cache.url is required"));
    }

    #[test]
    fn test_cache_backend_unknown_rejected() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        [cache]
        backend = "memcached"

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("unknown cache backend"));
    }

    #[test]
    fn test_cache_backend_redis_with_url() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        [cache]
        backend = "redis"
        url = "redis://localhost:6379"
        prefix = "myapp:"

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;
        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.cache.backend, "redis");
        assert_eq!(config.cache.url.as_deref(), Some("redis://localhost:6379"));
        assert_eq!(config.cache.prefix, "myapp:");
    }

    #[test]
    fn test_parse_bearer_scoped_tokens() {
        let toml = r#"
        [proxy]
        name = "scoped"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"

        [[auth.scoped_tokens]]
        token = "frontend-token"
        allow_tools = ["echo/read_file"]

        [[auth.scoped_tokens]]
        token = "admin-token"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::Bearer {
                tokens,
                scoped_tokens,
            }) => {
                assert!(tokens.is_empty());
                assert_eq!(scoped_tokens.len(), 2);
                assert_eq!(scoped_tokens[0].token, "frontend-token");
                assert_eq!(scoped_tokens[0].allow_tools, vec!["echo/read_file"]);
                assert!(scoped_tokens[1].allow_tools.is_empty());
            }
            other => panic!("expected Bearer auth, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_bearer_mixed_tokens() {
        let toml = r#"
        [proxy]
        name = "mixed"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        tokens = ["simple-token"]

        [[auth.scoped_tokens]]
        token = "scoped-token"
        deny_tools = ["echo/delete"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::Bearer {
                tokens,
                scoped_tokens,
            }) => {
                assert_eq!(tokens, &["simple-token"]);
                assert_eq!(scoped_tokens.len(), 1);
                assert_eq!(scoped_tokens[0].deny_tools, vec!["echo/delete"]);
            }
            other => panic!("expected Bearer auth, got: {other:?}"),
        }
    }

    #[test]
    fn test_bearer_empty_tokens_rejected() {
        let toml = r#"
        [proxy]
        name = "empty"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string().contains("at least one token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_bearer_duplicate_across_lists_rejected() {
        let toml = r#"
        [proxy]
        name = "dup"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        tokens = ["shared-token"]

        [[auth.scoped_tokens]]
        token = "shared-token"
        allow_tools = ["echo/read"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string().contains("duplicate bearer token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_bearer_allow_and_deny_rejected() {
        let toml = r#"
        [proxy]
        name = "both"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"

        [[auth.scoped_tokens]]
        token = "conflict"
        allow_tools = ["echo/read"]
        deny_tools = ["echo/write"]
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string().contains("cannot specify both"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_websocket_transport() {
        let toml = r#"
        [proxy]
        name = "ws-proxy"
        [proxy.listen]

        [[backends]]
        name = "ws-backend"
        transport = "websocket"
        url = "ws://localhost:9090/ws"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert!(matches!(
            config.backends[0].transport,
            TransportType::Websocket
        ));
        assert_eq!(
            config.backends[0].url.as_deref(),
            Some("ws://localhost:9090/ws")
        );
    }

    #[test]
    fn test_websocket_transport_requires_url() {
        let toml = r#"
        [proxy]
        name = "ws-proxy"
        [proxy.listen]

        [[backends]]
        name = "ws-backend"
        transport = "websocket"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("websocket transport requires 'url'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_websocket_with_bearer_token() {
        let toml = r#"
        [proxy]
        name = "ws-proxy"
        [proxy.listen]

        [[backends]]
        name = "ws-backend"
        transport = "websocket"
        url = "wss://secure.example.com/mcp"
        bearer_token = "my-secret"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(
            config.backends[0].bearer_token.as_deref(),
            Some("my-secret")
        );
    }

    #[test]
    fn test_tool_discovery_defaults_false() {
        let config = ProxyConfig::parse(minimal_config()).unwrap();
        assert!(!config.proxy.tool_discovery);
    }

    #[test]
    fn test_tool_discovery_enabled() {
        let toml = r#"
        [proxy]
        name = "discovery"
        tool_discovery = true
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert!(config.proxy.tool_discovery);
    }

    #[test]
    fn test_parse_oauth_config() {
        let toml = r#"
        [proxy]
        name = "oauth-proxy"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "oauth"
        issuer = "https://accounts.google.com"
        audience = "mcp-proxy"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::OAuth {
                issuer,
                audience,
                token_validation,
                ..
            }) => {
                assert_eq!(issuer, "https://accounts.google.com");
                assert_eq!(audience, "mcp-proxy");
                assert_eq!(token_validation, &TokenValidationStrategy::Jwt);
            }
            other => panic!("expected OAuth auth, got: {other:?}"),
        }
    }

    #[test]
    fn test_parse_oauth_with_introspection() {
        let toml = r#"
        [proxy]
        name = "oauth-proxy"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "oauth"
        issuer = "https://auth.example.com"
        audience = "mcp-proxy"
        client_id = "my-client"
        client_secret = "my-secret"
        token_validation = "introspection"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::OAuth {
                token_validation,
                client_id,
                client_secret,
                ..
            }) => {
                assert_eq!(token_validation, &TokenValidationStrategy::Introspection);
                assert_eq!(client_id.as_deref(), Some("my-client"));
                assert_eq!(client_secret.as_deref(), Some("my-secret"));
            }
            other => panic!("expected OAuth auth, got: {other:?}"),
        }
    }

    #[test]
    fn test_oauth_introspection_requires_credentials() {
        let toml = r#"
        [proxy]
        name = "oauth-proxy"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "oauth"
        issuer = "https://auth.example.com"
        audience = "mcp-proxy"
        token_validation = "introspection"
        "#;

        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(
            err.to_string().contains("client_id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_oauth_with_overrides() {
        let toml = r#"
        [proxy]
        name = "oauth-proxy"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "oauth"
        issuer = "https://auth.example.com"
        audience = "mcp-proxy"
        jwks_uri = "https://auth.example.com/custom/jwks"
        introspection_endpoint = "https://auth.example.com/custom/introspect"
        client_id = "my-client"
        client_secret = "my-secret"
        token_validation = "both"
        required_scopes = ["read", "write"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::OAuth {
                jwks_uri,
                introspection_endpoint,
                token_validation,
                required_scopes,
                ..
            }) => {
                assert_eq!(
                    jwks_uri.as_deref(),
                    Some("https://auth.example.com/custom/jwks")
                );
                assert_eq!(
                    introspection_endpoint.as_deref(),
                    Some("https://auth.example.com/custom/introspect")
                );
                assert_eq!(token_validation, &TokenValidationStrategy::Both);
                assert_eq!(required_scopes, &["read", "write"]);
            }
            other => panic!("expected OAuth auth, got: {other:?}"),
        }
    }

    #[test]
    fn test_check_env_vars_warns_on_unset() {
        let toml = r#"
        [proxy]
        name = "env-check"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        bearer_token = "${TOTALLY_UNSET_VAR_1}"

        [backends.env]
        API_KEY = "${TOTALLY_UNSET_VAR_2}"
        STATIC = "plain-value"

        [auth]
        type = "bearer"
        tokens = ["${TOTALLY_UNSET_VAR_3}", "literal-token"]

        [[auth.scoped_tokens]]
        token = "${TOTALLY_UNSET_VAR_4}"
        allow_tools = ["svc/echo"]
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let warnings = config.check_env_vars();

        assert_eq!(warnings.len(), 4, "warnings: {warnings:?}");
        assert!(warnings[0].contains("TOTALLY_UNSET_VAR_1"));
        assert!(warnings[0].contains("bearer_token"));
        assert!(warnings[1].contains("TOTALLY_UNSET_VAR_2"));
        assert!(warnings[1].contains("env.API_KEY"));
        assert!(warnings[2].contains("TOTALLY_UNSET_VAR_3"));
        assert!(warnings[2].contains("tokens[0]"));
        assert!(warnings[3].contains("TOTALLY_UNSET_VAR_4"));
        assert!(warnings[3].contains("scoped_tokens[0]"));
    }

    #[test]
    fn test_check_env_vars_no_warnings_when_set() {
        // SAFETY: test runs single-threaded
        unsafe { std::env::set_var("MCP_CHECK_TEST_VAR", "value") };

        let toml = r#"
        [proxy]
        name = "env-check"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        bearer_token = "${MCP_CHECK_TEST_VAR}"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let warnings = config.check_env_vars();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

        // SAFETY: same as above
        unsafe { std::env::remove_var("MCP_CHECK_TEST_VAR") };
    }

    #[test]
    fn test_check_env_vars_no_warnings_for_literals() {
        let toml = r#"
        [proxy]
        name = "env-check"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        bearer_token = "literal-token"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let warnings = config.check_env_vars();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn test_check_env_vars_oauth_client_secret() {
        let toml = r#"
        [proxy]
        name = "oauth-check"
        [proxy.listen]

        [[backends]]
        name = "svc"
        transport = "http"
        url = "http://localhost:3000"

        [auth]
        type = "oauth"
        issuer = "https://auth.example.com"
        audience = "mcp-proxy"
        client_id = "my-client"
        client_secret = "${TOTALLY_UNSET_OAUTH_SECRET}"
        token_validation = "introspection"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        let warnings = config.check_env_vars();
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("TOTALLY_UNSET_OAUTH_SECRET"));
        assert!(warnings[0].contains("client_secret"));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn test_parse_yaml_config() {
        let yaml = r#"
proxy:
  name: yaml-proxy
  listen:
    host: "127.0.0.1"
    port: 8080
backends:
  - name: echo
    transport: stdio
    command: echo
"#;
        let config = ProxyConfig::parse_yaml(yaml).unwrap();
        assert_eq!(config.proxy.name, "yaml-proxy");
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].name, "echo");
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn test_parse_yaml_with_auth() {
        let yaml = r#"
proxy:
  name: auth-proxy
  listen:
    host: "127.0.0.1"
    port: 9090
backends:
  - name: api
    transport: stdio
    command: echo
auth:
  type: bearer
  tokens:
    - token-1
    - token-2
"#;
        let config = ProxyConfig::parse_yaml(yaml).unwrap();
        match &config.auth {
            Some(AuthConfig::Bearer { tokens, .. }) => {
                assert_eq!(tokens, &["token-1", "token-2"]);
            }
            other => panic!("expected Bearer auth, got: {other:?}"),
        }
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn test_parse_yaml_with_middleware() {
        let yaml = r#"
proxy:
  name: mw-proxy
  listen:
    host: "127.0.0.1"
    port: 8080
backends:
  - name: api
    transport: stdio
    command: echo
    timeout:
      seconds: 30
    rate_limit:
      requests: 100
      period_seconds: 1
    expose_tools:
      - read_file
      - list_directory
"#;
        let config = ProxyConfig::parse_yaml(yaml).unwrap();
        assert_eq!(config.backends[0].timeout.as_ref().unwrap().seconds, 30);
        assert_eq!(
            config.backends[0].rate_limit.as_ref().unwrap().requests,
            100
        );
        assert_eq!(
            config.backends[0].expose_tools,
            vec!["read_file", "list_directory"]
        );
    }

    #[test]
    fn test_from_mcp_json() {
        let dir = std::env::temp_dir().join("mcp_proxy_test_from_mcp_json");
        let project_dir = dir.join("my-project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let mcp_json_path = project_dir.join(".mcp.json");
        std::fs::write(
            &mcp_json_path,
            r#"{
                "mcpServers": {
                    "github": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-github"]
                    },
                    "api": {
                        "url": "http://localhost:9000"
                    }
                }
            }"#,
        )
        .unwrap();

        let config = ProxyConfig::from_mcp_json(&mcp_json_path).unwrap();

        // Name derived from parent directory
        assert_eq!(config.proxy.name, "my-project");
        // Sensible defaults
        assert_eq!(config.proxy.listen.host, "127.0.0.1");
        assert_eq!(config.proxy.listen.port, 8080);
        assert_eq!(config.proxy.version, "0.1.0");
        assert_eq!(config.proxy.separator, "/");
        // No auth or middleware
        assert!(config.auth.is_none());
        assert!(config.composite_tools.is_empty());
        // Backends imported
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[0].name, "api");
        assert_eq!(config.backends[1].name, "github");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_from_mcp_json_empty_rejects() {
        let dir = std::env::temp_dir().join("mcp_proxy_test_from_mcp_json_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let mcp_json_path = dir.join(".mcp.json");
        std::fs::write(&mcp_json_path, r#"{ "mcpServers": {} }"#).unwrap();

        let err = ProxyConfig::from_mcp_json(&mcp_json_path).unwrap_err();
        assert!(
            err.to_string().contains("at least one backend"),
            "unexpected error: {err}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_priority_defaults_to_zero() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "stdio"
        command = "echo"
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.backends[0].priority, 0);
    }

    #[test]
    fn test_priority_parsed_from_config() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "api-backup-1"
        transport = "stdio"
        command = "echo"
        failover_for = "api"
        priority = 10

        [[backends]]
        name = "api-backup-2"
        transport = "stdio"
        command = "echo"
        failover_for = "api"
        priority = 5
        "#;

        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.backends[0].priority, 0);
        assert_eq!(config.backends[1].priority, 10);
        assert_eq!(config.backends[2].priority, 5);
    }
}
