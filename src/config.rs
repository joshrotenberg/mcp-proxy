use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    pub gateway: Gateway,
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
}

#[derive(Debug, Deserialize)]
pub struct Gateway {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_separator")]
    pub separator: String,
    pub listen: ListenConfig,
    pub instructions: Option<String>,
    /// Graceful shutdown timeout in seconds (default: 30)
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_seconds: u64,
    /// Enable hot reload: watch config file for new backends
    #[serde(default)]
    pub hot_reload: bool,
}

#[derive(Debug, Deserialize)]
pub struct ListenConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct BackendConfig {
    pub name: String,
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
    /// Per-backend cache policy
    pub cache: Option<BackendCacheConfig>,
    /// Tool aliases: rename tools exposed by this backend
    #[serde(default)]
    pub aliases: Vec<AliasConfig>,
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
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    Stdio,
    Http,
}

#[derive(Debug, Deserialize)]
pub struct TimeoutConfig {
    pub seconds: u64,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum requests per period
    pub requests: usize,
    /// Period in seconds (default: 1)
    #[serde(default = "default_rate_period")]
    pub period_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct ConcurrencyConfig {
    /// Maximum concurrent requests
    pub max_concurrent: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    Bearer {
        tokens: Vec<String>,
    },
    Jwt {
        issuer: String,
        audience: String,
        jwks_uri: String,
        /// RBAC role definitions
        #[serde(default)]
        roles: Vec<RoleConfig>,
        /// Map JWT claims to roles
        role_mapping: Option<RoleMappingConfig>,
    },
}

#[derive(Debug, Deserialize)]
pub struct RoleConfig {
    pub name: String,
    /// Tools this role can access (namespaced, e.g. "files/read_file")
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools this role cannot access
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RoleMappingConfig {
    /// JWT claim to read for role resolution (e.g. "scope", "role", "groups")
    pub claim: String,
    /// Map claim values to role names
    pub mapping: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct AliasConfig {
    /// Original tool name (backend-local, without namespace prefix)
    pub from: String,
    /// New tool name to expose (will be namespaced as backend/to)
    pub to: String,
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Default, Deserialize)]
pub struct PerformanceConfig {
    /// Deduplicate identical concurrent tool calls and resource reads
    #[serde(default)]
    pub coalesce_requests: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct SecurityConfig {
    /// Maximum size of tool call arguments in bytes (default: unlimited)
    pub max_argument_size: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub audit: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub json_logs: bool,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub tracing: TracingConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct MetricsConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct TracingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// OTLP endpoint (default: http://localhost:4317)
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
    /// Service name for traces (default: "mcp-gateway")
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
    "mcp-gateway".to_string()
}

/// Resolved filter rules for a backend's capabilities.
#[derive(Debug, Clone)]
pub struct BackendFilter {
    pub namespace: String,
    pub tool_filter: NameFilter,
    pub resource_filter: NameFilter,
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
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::PassAll => true,
            Self::AllowList(set) => set.contains(name),
            Self::DenyList(set) => !set.contains(name),
        }
    }
}

impl BackendConfig {
    pub fn build_filter(&self, separator: &str) -> Option<BackendFilter> {
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

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
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
        }
    }
}
