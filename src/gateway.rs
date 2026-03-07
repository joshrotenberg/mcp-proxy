//! Core gateway construction and serving.

use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use tokio::process::Command;
use tower::timeout::TimeoutLayer;
use tower::util::BoxCloneService;
use tower_mcp::SessionHandle;
use tower_mcp::auth::{AuthLayer, StaticBearerValidator};
use tower_mcp::client::StdioClientTransport;
use tower_mcp::proxy::McpProxy;
use tower_mcp::{RouterRequest, RouterResponse};

use crate::admin::BackendMeta;
use crate::alias;
use crate::cache;
use crate::coalesce;
use crate::config::{AuthConfig, GatewayConfig, TransportType};
use crate::filter::CapabilityFilterService;
use crate::metrics;
use crate::rbac::{RbacConfig, RbacService};
use crate::validation::{ValidationConfig, ValidationService};

/// A fully constructed MCP gateway ready to serve or embed.
pub struct Gateway {
    router: Router,
    session_handle: SessionHandle,
    proxy: McpProxy,
    config: GatewayConfig,
}

impl Gateway {
    /// Build a gateway from a [`GatewayConfig`].
    ///
    /// Connects to all backends, builds the middleware stack, and prepares
    /// the axum router. Call [`serve()`](Self::serve) to run standalone or
    /// [`into_router()`](Self::into_router) to embed in an existing app.
    pub async fn from_config(config: GatewayConfig) -> Result<Self> {
        let proxy = build_proxy(&config).await?;
        let proxy_for_admin = proxy.clone();
        let proxy_for_caller = proxy.clone();

        // Install Prometheus metrics recorder (must happen before middleware)
        let metrics_handle = if config.observability.metrics.enabled {
            tracing::info!("Prometheus metrics enabled at /admin/metrics");
            let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
            let handle = builder
                .install_recorder()
                .context("installing Prometheus metrics recorder")?;
            Some(handle)
        } else {
            None
        };

        let (service, cache_handle) = build_middleware_stack(&config, proxy)?;

        let (router, session_handle) =
            tower_mcp::transport::http::HttpTransport::from_service(service)
                .into_router_with_handle();

        // Inbound authentication (axum-level middleware)
        let router = apply_auth(&config, router).await?;

        // Collect backend metadata for the health checker
        let backend_meta: std::collections::HashMap<String, BackendMeta> = config
            .backends
            .iter()
            .map(|b| {
                (
                    b.name.clone(),
                    BackendMeta {
                        transport: format!("{:?}", b.transport).to_lowercase(),
                    },
                )
            })
            .collect();

        // Admin API
        let admin_state = crate::admin::spawn_health_checker(
            proxy_for_admin,
            config.gateway.name.clone(),
            config.gateway.version.clone(),
            config.backends.len(),
            backend_meta,
        );
        let router = router.nest(
            "/admin",
            crate::admin::admin_router(
                admin_state.clone(),
                metrics_handle,
                session_handle.clone(),
                cache_handle,
            ),
        );
        tracing::info!("Admin API enabled at /admin/backends");

        // MCP admin tools (gateway/ namespace)
        if let Err(e) = crate::admin_tools::register_admin_tools(
            &proxy_for_caller,
            admin_state,
            session_handle.clone(),
            &config,
        )
        .await
        {
            tracing::warn!("Failed to register admin tools: {e}");
        } else {
            tracing::info!("MCP admin tools registered under gateway/ namespace");
        }

        Ok(Self {
            router,
            session_handle,
            proxy: proxy_for_caller,
            config,
        })
    }

    /// Get a reference to the session handle for monitoring active sessions.
    pub fn session_handle(&self) -> &SessionHandle {
        &self.session_handle
    }

    /// Get a reference to the underlying [`McpProxy`] for dynamic operations.
    ///
    /// Use this to add backends dynamically via [`McpProxy::add_backend()`].
    pub fn proxy(&self) -> &McpProxy {
        &self.proxy
    }

    /// Enable hot reload by watching the given config file path.
    ///
    /// New backends added to the config file will be connected dynamically
    /// without restarting the gateway.
    pub fn enable_hot_reload(&self, config_path: std::path::PathBuf) {
        tracing::info!("Hot reload enabled, watching config file for changes");
        crate::reload::spawn_config_watcher(config_path, self.proxy.clone());
    }

    /// Consume the gateway and return the axum Router and SessionHandle.
    ///
    /// Use this to embed the gateway in an existing axum application:
    ///
    /// ```rust,ignore
    /// let (gateway_router, session_handle) = gateway.into_router();
    ///
    /// let app = Router::new()
    ///     .nest("/mcp", gateway_router)
    ///     .route("/health", get(|| async { "ok" }));
    /// ```
    pub fn into_router(self) -> (Router, SessionHandle) {
        (self.router, self.session_handle)
    }

    /// Serve the gateway on the configured listen address.
    ///
    /// Blocks until a shutdown signal (SIGTERM/SIGINT) is received,
    /// then drains connections for the configured timeout period.
    pub async fn serve(self) -> Result<()> {
        let addr = format!(
            "{}:{}",
            self.config.gateway.listen.host, self.config.gateway.listen.port
        );

        tracing::info!(listen = %addr, "Gateway ready");

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("binding to {}", addr))?;

        let shutdown_timeout = Duration::from_secs(self.config.gateway.shutdown_timeout_seconds);
        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown_signal(shutdown_timeout))
            .await
            .context("server error")?;

        tracing::info!("Gateway shut down");
        Ok(())
    }
}

/// Build the McpProxy with all backends and per-backend middleware.
async fn build_proxy(config: &GatewayConfig) -> Result<McpProxy> {
    let mut builder = McpProxy::builder(&config.gateway.name, &config.gateway.version)
        .separator(&config.gateway.separator);

    if let Some(instructions) = &config.gateway.instructions {
        builder = builder.instructions(instructions);
    }

    for backend in &config.backends {
        tracing::info!(name = %backend.name, transport = ?backend.transport, "Adding backend");

        match backend.transport {
            TransportType::Stdio => {
                let command = backend.command.as_deref().unwrap();
                let args: Vec<&str> = backend.args.iter().map(|s| s.as_str()).collect();

                let mut cmd = Command::new(command);
                cmd.args(&args);

                for (key, value) in &backend.env {
                    cmd.env(key, value);
                }

                let transport = StdioClientTransport::spawn_command(&mut cmd)
                    .await
                    .with_context(|| format!("spawning backend '{}'", backend.name))?;

                builder = builder.backend(&backend.name, transport).await;
            }
            TransportType::Http => {
                let url = backend.url.as_deref().unwrap();
                let transport = tower_mcp::client::HttpClientTransport::new(url);

                builder = builder.backend(&backend.name, transport).await;
            }
        }

        // Per-backend middleware stack (applied in order: inner -> outer)

        // Retry (innermost -- retries happen before other middleware)
        if let Some(retry_cfg) = &backend.retry {
            tracing::info!(
                backend = %backend.name,
                max_retries = retry_cfg.max_retries,
                initial_backoff_ms = retry_cfg.initial_backoff_ms,
                max_backoff_ms = retry_cfg.max_backoff_ms,
                "Applying retry policy"
            );
            let policy = crate::retry::McpRetryPolicy::from_config(retry_cfg);
            builder = builder.backend_layer(tower::retry::RetryLayer::new(policy));
        }

        // Concurrency limit
        if let Some(cc) = &backend.concurrency {
            tracing::info!(
                backend = %backend.name,
                max = cc.max_concurrent,
                "Applying concurrency limit"
            );
            builder =
                builder.backend_layer(tower::limit::ConcurrencyLimitLayer::new(cc.max_concurrent));
        }

        // Rate limit
        if let Some(rl) = &backend.rate_limit {
            tracing::info!(
                backend = %backend.name,
                requests = rl.requests,
                period_seconds = rl.period_seconds,
                "Applying rate limit"
            );
            let layer = tower_resilience::ratelimiter::RateLimiterLayer::builder()
                .limit_for_period(rl.requests)
                .refresh_period(Duration::from_secs(rl.period_seconds))
                .name(format!("{}-ratelimit", backend.name))
                .build();
            builder = builder.backend_layer(layer);
        }

        // Timeout
        if let Some(timeout) = &backend.timeout {
            tracing::info!(
                backend = %backend.name,
                seconds = timeout.seconds,
                "Applying timeout"
            );
            builder =
                builder.backend_layer(TimeoutLayer::new(Duration::from_secs(timeout.seconds)));
        }

        // Circuit breaker (outermost)
        if let Some(cb) = &backend.circuit_breaker {
            tracing::info!(
                backend = %backend.name,
                failure_rate = cb.failure_rate_threshold,
                wait_seconds = cb.wait_duration_seconds,
                "Applying circuit breaker"
            );
            let layer = tower_resilience::circuitbreaker::CircuitBreakerLayer::builder()
                .failure_rate_threshold(cb.failure_rate_threshold)
                .minimum_number_of_calls(cb.minimum_calls)
                .wait_duration_in_open(Duration::from_secs(cb.wait_duration_seconds))
                .permitted_calls_in_half_open(cb.permitted_calls_in_half_open)
                .name(format!("{}-cb", backend.name))
                .build();
            builder = builder.backend_layer(layer);
        }
    }

    let result = builder.build().await?;

    if !result.skipped.is_empty() {
        for s in &result.skipped {
            tracing::warn!("Skipped backend: {s}");
        }
    }

    Ok(result.proxy)
}

/// Build the MCP-level middleware stack around the proxy.
fn build_middleware_stack(
    config: &GatewayConfig,
    proxy: McpProxy,
) -> Result<(
    BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    Option<cache::CacheHandle>,
)> {
    let mut service: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
        BoxCloneService::new(proxy);
    let mut cache_handle: Option<cache::CacheHandle> = None;

    // Response caching (innermost)
    let cache_configs: Vec<_> = config
        .backends
        .iter()
        .filter_map(|b| {
            b.cache
                .as_ref()
                .map(|c| (format!("{}{}", b.name, config.gateway.separator), c))
        })
        .collect();

    if !cache_configs.is_empty() {
        for (ns, cfg) in &cache_configs {
            tracing::info!(
                backend = %ns.trim_end_matches(&config.gateway.separator),
                resource_ttl = cfg.resource_ttl_seconds,
                tool_ttl = cfg.tool_ttl_seconds,
                max_entries = cfg.max_entries,
                "Applying response cache"
            );
        }
        let (cache_svc, handle) = cache::CacheService::new(service, cache_configs);
        service = BoxCloneService::new(cache_svc);
        cache_handle = Some(handle);
    }

    // Request coalescing
    if config.performance.coalesce_requests {
        tracing::info!("Request coalescing enabled");
        service = BoxCloneService::new(coalesce::CoalesceService::new(service));
    }

    // Request validation
    if config.security.max_argument_size.is_some() {
        let validation = ValidationConfig {
            max_argument_size: config.security.max_argument_size,
        };
        if let Some(max) = validation.max_argument_size {
            tracing::info!(max_argument_size = max, "Applying request validation");
        }
        service = BoxCloneService::new(ValidationService::new(service, validation));
    }

    // Static capability filtering
    let filters: Vec<_> = config
        .backends
        .iter()
        .filter_map(|b| b.build_filter(&config.gateway.separator))
        .collect();

    if !filters.is_empty() {
        for f in &filters {
            tracing::info!(
                backend = %f.namespace.trim_end_matches(&config.gateway.separator),
                tool_filter = ?f.tool_filter,
                resource_filter = ?f.resource_filter,
                prompt_filter = ?f.prompt_filter,
                "Applying capability filter"
            );
        }
        service = BoxCloneService::new(CapabilityFilterService::new(service, filters));
    }

    // Tool aliasing
    let alias_mappings: Vec<_> = config
        .backends
        .iter()
        .flat_map(|b| {
            let ns = format!("{}{}", b.name, config.gateway.separator);
            b.aliases
                .iter()
                .map(move |a| (ns.clone(), a.from.clone(), a.to.clone()))
        })
        .collect();

    if let Some(alias_map) = alias::AliasMap::new(alias_mappings) {
        let count = alias_map.forward.len();
        tracing::info!(aliases = count, "Applying tool aliases");
        service = BoxCloneService::new(alias::AliasService::new(service, alias_map));
    }

    // RBAC (JWT auth only)
    let rbac_config = match &config.auth {
        Some(AuthConfig::Jwt {
            roles,
            role_mapping: Some(mapping),
            ..
        }) if !roles.is_empty() => {
            tracing::info!(
                roles = roles.len(),
                claim = %mapping.claim,
                "Enabling RBAC"
            );
            Some(RbacConfig::new(roles, mapping))
        }
        _ => None,
    };

    if let Some(rbac) = rbac_config {
        service = BoxCloneService::new(RbacService::new(service, rbac));
    }

    // Metrics
    if config.observability.metrics.enabled {
        service = BoxCloneService::new(metrics::MetricsService::new(service));
    }

    // Audit logging
    if config.observability.audit {
        tracing::info!("Audit logging enabled (target: mcp::audit)");
        let audited = tower::Layer::layer(&tower_mcp::AuditLayer::new(), service);
        service = BoxCloneService::new(tower_mcp::CatchError::new(audited));
    }

    Ok((service, cache_handle))
}

/// Apply inbound authentication middleware to the router.
async fn apply_auth(config: &GatewayConfig, router: Router) -> Result<Router> {
    let router = if let Some(auth) = &config.auth {
        match auth {
            AuthConfig::Bearer { tokens } => {
                tracing::info!(token_count = tokens.len(), "Enabling bearer token auth");
                let validator = StaticBearerValidator::new(tokens.iter().cloned());
                let layer = AuthLayer::new(validator);
                router.layer(layer)
            }
            AuthConfig::Jwt {
                issuer,
                audience,
                jwks_uri,
                ..
            } => {
                tracing::info!(
                    issuer = %issuer,
                    audience = %audience,
                    jwks_uri = %jwks_uri,
                    "Enabling JWT auth (JWKS)"
                );
                let validator = tower_mcp::oauth::JwksValidator::builder(jwks_uri)
                    .expected_audience(audience)
                    .expected_issuer(issuer)
                    .build()
                    .await
                    .context("building JWKS validator")?;

                let addr = format!(
                    "http://{}:{}",
                    config.gateway.listen.host, config.gateway.listen.port
                );
                let metadata = tower_mcp::oauth::ProtectedResourceMetadata::new(&addr)
                    .authorization_server(issuer);

                let layer = tower_mcp::oauth::OAuthLayer::new(validator, metadata);
                router.layer(layer)
            }
        }
    } else {
        router
    };
    Ok(router)
}

/// Wait for SIGTERM or SIGINT, then log and return.
pub async fn shutdown_signal(timeout: Duration) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
    tracing::info!(
        timeout_seconds = timeout.as_secs(),
        "Shutdown signal received, draining connections"
    );
}
