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
    /// Logging, metrics, and tracing configuration.
    #[serde(default)]
    pub observability: ObservabilityConfig,
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
        /// Accepted bearer tokens.
        tokens: Vec<String>,
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
}

/// A name-based allow/deny filter.
#[derive(Debug, Clone)]
pub enum NameFilter {
    /// No filtering -- everything passes.
    PassAll,
    /// Only items in this set are allowed.
    AllowList(HashSet<String>),
    /// Items in this set are denied.
    DenyList(HashSet<String>),
}

impl NameFilter {
    /// Check if a capability name is allowed by this filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashSet;
    /// use mcp_proxy::config::NameFilter;
    ///
    /// let filter = NameFilter::DenyList(["delete".to_string()].into());
    /// assert!(filter.allows("read"));
    /// assert!(!filter.allows("delete"));
    ///
    /// let filter = NameFilter::AllowList(["read".to_string()].into());
    /// assert!(filter.allows("read"));
    /// assert!(!filter.allows("write"));
    ///
    /// assert!(NameFilter::PassAll.allows("anything"));
    /// ```
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::PassAll => true,
            Self::AllowList(set) => set.contains(name),
            Self::DenyList(set) => !set.contains(name),
        }
    }
}

impl BackendConfig {
    /// Build a [`BackendFilter`] from this backend's expose/hide lists.
    /// Returns `None` if no filtering is configured.
    ///
    /// Canary backends automatically hide all capabilities so their tools
    /// don't appear in `ListTools` responses (traffic reaches them via the
    /// canary routing middleware, not direct tool calls).
    pub fn build_filter(&self, separator: &str) -> Option<BackendFilter> {
        // Canary backends hide all capabilities -- tools are accessed via
        // the canary routing middleware rewriting the primary namespace.
        if self.canary_of.is_some() {
            return Some(BackendFilter {
                namespace: format!("{}{}", self.name, separator),
                tool_filter: NameFilter::AllowList(HashSet::new()),
                resource_filter: NameFilter::AllowList(HashSet::new()),
                prompt_filter: NameFilter::AllowList(HashSet::new()),
            });
        }

        let tool_filter = if !self.expose_tools.is_empty() {
            NameFilter::AllowList(self.expose_tools.iter().cloned().collect())
        } else if !self.hide_tools.is_empty() {
            NameFilter::DenyList(self.hide_tools.iter().cloned().collect())
        } else {
            NameFilter::PassAll
        };

        let resource_filter = if !self.expose_resources.is_empty() {
            NameFilter::AllowList(self.expose_resources.iter().cloned().collect())
        } else if !self.hide_resources.is_empty() {
            NameFilter::DenyList(self.hide_resources.iter().cloned().collect())
        } else {
            NameFilter::PassAll
        };

        let prompt_filter = if !self.expose_prompts.is_empty() {
            NameFilter::AllowList(self.expose_prompts.iter().cloned().collect())
        } else if !self.hide_prompts.is_empty() {
            NameFilter::DenyList(self.hide_prompts.iter().cloned().collect())
        } else {
            NameFilter::PassAll
        };

        // Only create a filter if at least one dimension has filtering
        if matches!(tool_filter, NameFilter::PassAll)
            && matches!(resource_filter, NameFilter::PassAll)
            && matches!(prompt_filter, NameFilter::PassAll)
        {
            return None;
        }

        Some(BackendFilter {
            namespace: format!("{}{}", self.name, separator),
            tool_filter,
            resource_filter,
            prompt_filter,
        })
    }
}

impl ProxyConfig {
    /// Load and validate a config from a file path.
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
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

    fn validate(&self) -> Result<()> {
        if self.backends.is_empty() {
            anyhow::bail!("at least one backend is required");
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
            Some(AuthConfig::Bearer { tokens }) => {
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
                .is_none()
        );
    }
}
