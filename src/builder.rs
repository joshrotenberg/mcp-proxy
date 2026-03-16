//! Programmatic proxy builder for library users.
//!
//! Constructs a [`ProxyConfig`] via a fluent API, avoiding the need for
//! TOML files. The resulting config is passed to [`Proxy::from_config()`]
//! as usual.
//!
//! # Example
//!
//! ```rust,no_run
//! use mcp_proxy::builder::ProxyBuilder;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let proxy = ProxyBuilder::new("my-proxy")
//!     .version("1.0.0")
//!     .listen("0.0.0.0", 9090)
//!     .stdio_backend("files", "npx", &["-y", "@modelcontextprotocol/server-filesystem"])
//!     .http_backend("api", "http://api:8080")
//!     .build()
//!     .await?;
//!
//! // Embed in an existing axum app
//! let (router, _session_handle) = proxy.into_router();
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;

use crate::Proxy;
use crate::config::*;

/// Fluent builder for constructing an MCP proxy without TOML config files.
///
/// Call [`build()`](Self::build) to connect backends and produce a
/// ready-to-serve [`Proxy`].
pub struct ProxyBuilder {
    config: ProxyConfig,
}

impl ProxyBuilder {
    /// Create a new proxy builder with the given name.
    ///
    /// Defaults: version "0.1.0", separator "/", listen 127.0.0.1:8080.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            config: ProxyConfig {
                proxy: ProxySettings {
                    name: name.into(),
                    version: "0.1.0".to_string(),
                    separator: "/".to_string(),
                    listen: ListenConfig {
                        host: "127.0.0.1".to_string(),
                        port: 8080,
                    },
                    instructions: None,
                    shutdown_timeout_seconds: 30,
                    hot_reload: false,
                    import_backends: None,
                    rate_limit: None,
                },
                backends: Vec::new(),
                auth: None,
                performance: PerformanceConfig::default(),
                security: SecurityConfig::default(),
                cache: CacheBackendConfig::default(),
                observability: ObservabilityConfig::default(),
            },
        }
    }

    /// Set the proxy version (default: "0.1.0").
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.config.proxy.version = version.into();
        self
    }

    /// Set the namespace separator (default: "/").
    pub fn separator(mut self, separator: impl Into<String>) -> Self {
        self.config.proxy.separator = separator.into();
        self
    }

    /// Set the listen address and port (default: 127.0.0.1:8080).
    pub fn listen(mut self, host: impl Into<String>, port: u16) -> Self {
        self.config.proxy.listen = ListenConfig {
            host: host.into(),
            port,
        };
        self
    }

    /// Set instructions text sent to MCP clients.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.config.proxy.instructions = Some(instructions.into());
        self
    }

    /// Set the graceful shutdown timeout (default: 30s).
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.config.proxy.shutdown_timeout_seconds = timeout.as_secs();
        self
    }

    /// Enable hot reload for watching config file changes.
    pub fn hot_reload(mut self, enabled: bool) -> Self {
        self.config.proxy.hot_reload = enabled;
        self
    }

    /// Set a global rate limit across all backends.
    pub fn global_rate_limit(mut self, requests: usize, period: Duration) -> Self {
        self.config.proxy.rate_limit = Some(GlobalRateLimitConfig {
            requests,
            period_seconds: period.as_secs(),
        });
        self
    }

    /// Add a stdio backend (subprocess).
    pub fn stdio_backend(
        mut self,
        name: impl Into<String>,
        command: impl Into<String>,
        args: &[&str],
    ) -> Self {
        self.config.backends.push(BackendConfig {
            name: name.into(),
            transport: TransportType::Stdio,
            command: Some(command.into()),
            args: args.iter().map(|s| s.to_string()).collect(),
            url: None,
            ..default_backend()
        });
        self
    }

    /// Add a stdio backend with environment variables.
    pub fn stdio_backend_with_env(
        mut self,
        name: impl Into<String>,
        command: impl Into<String>,
        args: &[&str],
        env: HashMap<String, String>,
    ) -> Self {
        self.config.backends.push(BackendConfig {
            name: name.into(),
            transport: TransportType::Stdio,
            command: Some(command.into()),
            args: args.iter().map(|s| s.to_string()).collect(),
            url: None,
            env,
            ..default_backend()
        });
        self
    }

    /// Add an HTTP backend.
    pub fn http_backend(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        self.config.backends.push(BackendConfig {
            name: name.into(),
            transport: TransportType::Http,
            command: None,
            url: Some(url.into()),
            ..default_backend()
        });
        self
    }

    /// Add an HTTP backend with a bearer token.
    pub fn http_backend_with_token(
        mut self,
        name: impl Into<String>,
        url: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        self.config.backends.push(BackendConfig {
            name: name.into(),
            transport: TransportType::Http,
            command: None,
            url: Some(url.into()),
            bearer_token: Some(token.into()),
            ..default_backend()
        });
        self
    }

    /// Configure the last added backend with a per-backend modifier.
    ///
    /// # Panics
    ///
    /// Panics if no backends have been added.
    pub fn configure_backend(mut self, f: impl FnOnce(&mut BackendConfig)) -> Self {
        let backend = self
            .config
            .backends
            .last_mut()
            .expect("configure_backend called with no backends");
        f(backend);
        self
    }

    /// Enable bearer token authentication.
    pub fn bearer_auth(mut self, tokens: Vec<String>) -> Self {
        self.config.auth = Some(AuthConfig::Bearer { tokens });
        self
    }

    /// Enable request coalescing.
    pub fn coalesce_requests(mut self, enabled: bool) -> Self {
        self.config.performance.coalesce_requests = enabled;
        self
    }

    /// Set the maximum argument size for validation.
    pub fn max_argument_size(mut self, max_bytes: usize) -> Self {
        self.config.security.max_argument_size = Some(max_bytes);
        self
    }

    /// Enable audit logging.
    pub fn audit_logging(mut self, enabled: bool) -> Self {
        self.config.observability.audit = enabled;
        self
    }

    /// Enable access logging.
    pub fn access_logging(mut self, enabled: bool) -> Self {
        self.config.observability.access_log.enabled = enabled;
        self
    }

    /// Set the log level (default: "info").
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.config.observability.log_level = level.into();
        self
    }

    /// Enable structured JSON logging.
    pub fn json_logs(mut self, enabled: bool) -> Self {
        self.config.observability.json_logs = enabled;
        self
    }

    /// Enable Prometheus metrics.
    pub fn metrics(mut self, enabled: bool) -> Self {
        self.config.observability.metrics.enabled = enabled;
        self
    }

    /// Extract the built [`ProxyConfig`] without connecting to backends.
    ///
    /// Useful for inspection, serialization, or passing to
    /// [`Proxy::from_config()`] manually.
    pub fn into_config(self) -> ProxyConfig {
        self.config
    }

    /// Build the proxy: validate config, connect to all backends, and
    /// construct the middleware stack.
    pub async fn build(self) -> Result<Proxy> {
        Proxy::from_config(self.config).await
    }
}

/// Default backend config with all optional fields set to `None`/empty.
fn default_backend() -> BackendConfig {
    BackendConfig {
        name: String::new(),
        transport: TransportType::Stdio,
        command: None,
        args: Vec::new(),
        url: None,
        env: HashMap::new(),
        bearer_token: None,
        forward_auth: false,
        timeout: None,
        circuit_breaker: None,
        rate_limit: None,
        concurrency: None,
        retry: None,
        outlier_detection: None,
        hedging: None,
        cache: None,
        default_args: serde_json::Map::new(),
        inject_args: Vec::new(),
        param_overrides: Vec::new(),
        expose_tools: Vec::new(),
        hide_tools: Vec::new(),
        expose_resources: Vec::new(),
        hide_resources: Vec::new(),
        expose_prompts: Vec::new(),
        hide_prompts: Vec::new(),
        failover_for: None,
        canary_of: None,
        weight: 100,
        aliases: Vec::new(),
        mirror_of: None,
        mirror_percent: 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_minimal() {
        let config = ProxyBuilder::new("test-proxy").into_config();
        assert_eq!(config.proxy.name, "test-proxy");
        assert_eq!(config.proxy.version, "0.1.0");
        assert_eq!(config.proxy.separator, "/");
        assert_eq!(config.proxy.listen.host, "127.0.0.1");
        assert_eq!(config.proxy.listen.port, 8080);
        assert!(config.backends.is_empty());
    }

    #[test]
    fn test_builder_with_backends() {
        let config = ProxyBuilder::new("test")
            .stdio_backend("files", "npx", &["-y", "@mcp/server-files"])
            .http_backend("api", "http://localhost:8080")
            .into_config();

        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[0].name, "files");
        assert!(matches!(config.backends[0].transport, TransportType::Stdio));
        assert_eq!(config.backends[0].command.as_deref(), Some("npx"));
        assert_eq!(config.backends[1].name, "api");
        assert!(matches!(config.backends[1].transport, TransportType::Http));
        assert_eq!(
            config.backends[1].url.as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn test_builder_configure_backend() {
        let config = ProxyBuilder::new("test")
            .http_backend("api", "http://localhost:8080")
            .configure_backend(|b| {
                b.timeout = Some(TimeoutConfig { seconds: 30 });
                b.rate_limit = Some(RateLimitConfig {
                    requests: 100,
                    period_seconds: 1,
                });
                b.hide_tools = vec!["dangerous_op".to_string()];
            })
            .into_config();

        assert!(config.backends[0].timeout.is_some());
        assert!(config.backends[0].rate_limit.is_some());
        assert_eq!(config.backends[0].hide_tools, vec!["dangerous_op"]);
    }

    #[test]
    fn test_builder_auth_and_observability() {
        let config = ProxyBuilder::new("test")
            .bearer_auth(vec!["token1".into(), "token2".into()])
            .audit_logging(true)
            .access_logging(true)
            .metrics(true)
            .json_logs(true)
            .log_level("debug")
            .into_config();

        assert!(config.auth.is_some());
        assert!(config.observability.audit);
        assert!(config.observability.access_log.enabled);
        assert!(config.observability.metrics.enabled);
        assert!(config.observability.json_logs);
        assert_eq!(config.observability.log_level, "debug");
    }

    #[test]
    fn test_builder_global_rate_limit() {
        let config = ProxyBuilder::new("test")
            .global_rate_limit(500, Duration::from_secs(1))
            .into_config();

        let rl = config.proxy.rate_limit.unwrap();
        assert_eq!(rl.requests, 500);
        assert_eq!(rl.period_seconds, 1);
    }

    #[test]
    fn test_builder_all_settings() {
        let config = ProxyBuilder::new("enterprise")
            .version("2.0.0")
            .separator("::")
            .listen("0.0.0.0", 9090)
            .instructions("Enterprise MCP gateway")
            .shutdown_timeout(Duration::from_secs(60))
            .coalesce_requests(true)
            .max_argument_size(1_048_576)
            .into_config();

        assert_eq!(config.proxy.name, "enterprise");
        assert_eq!(config.proxy.version, "2.0.0");
        assert_eq!(config.proxy.separator, "::");
        assert_eq!(config.proxy.listen.host, "0.0.0.0");
        assert_eq!(config.proxy.listen.port, 9090);
        assert_eq!(
            config.proxy.instructions.as_deref(),
            Some("Enterprise MCP gateway")
        );
        assert_eq!(config.proxy.shutdown_timeout_seconds, 60);
        assert!(config.performance.coalesce_requests);
        assert_eq!(config.security.max_argument_size, Some(1_048_576));
    }

    #[test]
    fn test_builder_http_backend_with_token() {
        let config = ProxyBuilder::new("test")
            .http_backend_with_token("api", "http://api:8080", "secret")
            .into_config();

        assert_eq!(config.backends[0].bearer_token.as_deref(), Some("secret"));
    }

    #[test]
    fn test_builder_stdio_backend_with_env() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string());

        let config = ProxyBuilder::new("test")
            .stdio_backend_with_env("github", "npx", &["-y", "@mcp/github"], env)
            .into_config();

        assert_eq!(
            config.backends[0].env.get("GITHUB_TOKEN").unwrap(),
            "ghp_xxx"
        );
    }
}
