use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
pub struct ListenConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    Stdio,
    Http,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TimeoutConfig {
    pub seconds: u64,
}

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

#[derive(Debug, Deserialize, Serialize)]
pub struct RateLimitConfig {
    /// Maximum requests per period
    pub requests: usize,
    /// Period in seconds (default: 1)
    #[serde(default = "default_rate_period")]
    pub period_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConcurrencyConfig {
    /// Maximum concurrent requests
    pub max_concurrent: usize,
}

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
pub struct RoleConfig {
    pub name: String,
    /// Tools this role can access (namespaced, e.g. "files/read_file")
    #[serde(default)]
    pub allow_tools: Vec<String>,
    /// Tools this role cannot access
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RoleMappingConfig {
    /// JWT claim to read for role resolution (e.g. "scope", "role", "groups")
    pub claim: String,
    /// Map claim values to role names
    pub mapping: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AliasConfig {
    /// Original tool name (backend-local, without namespace prefix)
    pub from: String,
    /// New tool name to expose (will be namespaced as backend/to)
    pub to: String,
}

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

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PerformanceConfig {
    /// Deduplicate identical concurrent tool calls and resource reads
    #[serde(default)]
    pub coalesce_requests: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SecurityConfig {
    /// Maximum size of tool call arguments in bytes (default: unlimited)
    pub max_argument_size: Option<usize>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
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

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MetricsConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> &'static str {
        r#"
        [gateway]
        name = "test"
        [gateway.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#
    }

    #[test]
    fn test_parse_minimal_config() {
        let config = GatewayConfig::parse(minimal_config()).unwrap();
        assert_eq!(config.gateway.name, "test");
        assert_eq!(config.gateway.version, "0.1.0"); // default
        assert_eq!(config.gateway.separator, "/"); // default
        assert_eq!(config.gateway.listen.host, "127.0.0.1"); // default
        assert_eq!(config.gateway.listen.port, 8080); // default
        assert_eq!(config.gateway.shutdown_timeout_seconds, 30); // default
        assert!(!config.gateway.hot_reload); // default false
        assert_eq!(config.backends.len(), 1);
        assert_eq!(config.backends[0].name, "echo");
        assert!(config.auth.is_none());
        assert!(!config.observability.audit);
        assert!(!config.observability.metrics.enabled);
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
        [gateway]
        name = "full-gw"
        version = "2.0.0"
        separator = "."
        shutdown_timeout_seconds = 60
        hot_reload = true
        instructions = "A test gateway"
        [gateway.listen]
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

        let config = GatewayConfig::parse(toml).unwrap();
        assert_eq!(config.gateway.name, "full-gw");
        assert_eq!(config.gateway.version, "2.0.0");
        assert_eq!(config.gateway.separator, ".");
        assert_eq!(config.gateway.shutdown_timeout_seconds, 60);
        assert!(config.gateway.hot_reload);
        assert_eq!(
            config.gateway.instructions.as_deref(),
            Some("A test gateway")
        );
        assert_eq!(config.gateway.listen.host, "0.0.0.0");
        assert_eq!(config.gateway.listen.port, 9090);

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
        [gateway]
        name = "auth-gw"
        [gateway.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        tokens = ["token-1", "token-2"]
        "#;

        let config = GatewayConfig::parse(toml).unwrap();
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
        [gateway]
        name = "jwt-gw"
        [gateway.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "jwt"
        issuer = "https://auth.example.com"
        audience = "mcp-gateway"
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

        let config = GatewayConfig::parse(toml).unwrap();
        match &config.auth {
            Some(AuthConfig::Jwt {
                issuer,
                audience,
                jwks_uri,
                roles,
                role_mapping,
            }) => {
                assert_eq!(issuer, "https://auth.example.com");
                assert_eq!(audience, "mcp-gateway");
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
        [gateway]
        name = "empty"
        [gateway.listen]
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("at least one backend"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_stdio_without_command() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "broken"
        transport = "stdio"
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("stdio transport requires 'command'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_http_without_url() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "broken"
        transport = "http"
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("http transport requires 'url'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_invalid_circuit_breaker_threshold() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.circuit_breaker]
        failure_rate_threshold = 1.5
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("failure_rate_threshold must be in (0.0, 1.0]"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_zero_rate_limit() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.rate_limit]
        requests = 0
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("rate_limit.requests must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_zero_concurrency() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.concurrency]
        max_concurrent = 0
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("concurrency.max_concurrent must be > 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_tools() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_tools = ["read"]
        hide_tools = ["write"]
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot specify both expose_tools and hide_tools"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_resources() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_resources = ["file:///a"]
        hide_resources = ["file:///b"]
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
        assert!(
            format!("{err}").contains("cannot specify both expose_resources and hide_resources"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_reject_expose_and_hide_prompts() {
        let toml = r#"
        [gateway]
        name = "bad"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_prompts = ["help"]
        hide_prompts = ["admin"]
        "#;

        let err = GatewayConfig::parse(toml).unwrap_err();
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
        [gateway]
        name = "env-test"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"

        [backends.env]
        API_TOKEN = "${MCP_GW_TEST_TOKEN}"
        STATIC_VAL = "unchanged"
        "#;

        let mut config = GatewayConfig::parse(toml).unwrap();
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

    // ========================================================================
    // Capability filter building
    // ========================================================================

    #[test]
    fn test_build_filter_allowlist() {
        let toml = r#"
        [gateway]
        name = "filter"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        expose_tools = ["read", "list"]
        "#;

        let config = GatewayConfig::parse(toml).unwrap();
        let filter = config.backends[0]
            .build_filter(&config.gateway.separator)
            .expect("should have filter");
        assert_eq!(filter.namespace, "svc/");
        assert!(filter.tool_filter.allows("read"));
        assert!(filter.tool_filter.allows("list"));
        assert!(!filter.tool_filter.allows("delete"));
    }

    #[test]
    fn test_build_filter_denylist() {
        let toml = r#"
        [gateway]
        name = "filter"
        [gateway.listen]

        [[backends]]
        name = "svc"
        transport = "stdio"
        command = "echo"
        hide_tools = ["delete", "write"]
        "#;

        let config = GatewayConfig::parse(toml).unwrap();
        let filter = config.backends[0]
            .build_filter(&config.gateway.separator)
            .expect("should have filter");
        assert!(filter.tool_filter.allows("read"));
        assert!(!filter.tool_filter.allows("delete"));
        assert!(!filter.tool_filter.allows("write"));
    }

    #[test]
    fn test_build_filter_none_when_no_filtering() {
        let config = GatewayConfig::parse(minimal_config()).unwrap();
        assert!(
            config.backends[0]
                .build_filter(&config.gateway.separator)
                .is_none()
        );
    }
}
