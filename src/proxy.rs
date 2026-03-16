//! Core proxy construction and serving.

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
use crate::config::{AuthConfig, ProxyConfig, TransportType};
use crate::filter::CapabilityFilterService;
#[cfg(feature = "oauth")]
use crate::rbac::{RbacConfig, RbacService};
use crate::validation::{ValidationConfig, ValidationService};

/// A fully constructed MCP proxy ready to serve or embed.
pub struct Proxy {
    router: Router,
    session_handle: SessionHandle,
    inner: McpProxy,
    config: ProxyConfig,
}

impl Proxy {
    /// Build a proxy from a [`ProxyConfig`].
    ///
    /// Connects to all backends, builds the middleware stack, and prepares
    /// the axum router. Call [`serve()`](Self::serve) to run standalone or
    /// [`into_router()`](Self::into_router) to embed in an existing app.
    pub async fn from_config(config: ProxyConfig) -> Result<Self> {
        let mcp_proxy = build_mcp_proxy(&config).await?;
        let proxy_for_admin = mcp_proxy.clone();
        let proxy_for_caller = mcp_proxy.clone();

        // Install Prometheus metrics recorder (must happen before middleware)
        #[cfg(feature = "metrics")]
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
        #[cfg(not(feature = "metrics"))]
        let metrics_handle = None;

        let (service, cache_handle) = build_middleware_stack(&config, mcp_proxy)?;

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
            config.proxy.name.clone(),
            config.proxy.version.clone(),
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

        // MCP admin tools (proxy/ namespace)
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
            tracing::info!("MCP admin tools registered under proxy/ namespace");
        }

        Ok(Self {
            router,
            session_handle,
            inner: proxy_for_caller,
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
    pub fn mcp_proxy(&self) -> &McpProxy {
        &self.inner
    }

    /// Enable hot reload by watching the given config file path.
    ///
    /// New backends added to the config file will be connected dynamically
    /// without restarting the proxy.
    pub fn enable_hot_reload(&self, config_path: std::path::PathBuf) {
        tracing::info!("Hot reload enabled, watching config file for changes");
        crate::reload::spawn_config_watcher(config_path, self.inner.clone());
    }

    /// Consume the proxy and return the axum Router and SessionHandle.
    ///
    /// Use this to embed the proxy in an existing axum application:
    ///
    /// ```rust,ignore
    /// let (proxy_router, session_handle) = proxy.into_router();
    ///
    /// let app = Router::new()
    ///     .nest("/mcp", proxy_router)
    ///     .route("/health", get(|| async { "ok" }));
    /// ```
    pub fn into_router(self) -> (Router, SessionHandle) {
        (self.router, self.session_handle)
    }

    /// Serve the proxy on the configured listen address.
    ///
    /// Blocks until a shutdown signal (SIGTERM/SIGINT) is received,
    /// then drains connections for the configured timeout period.
    pub async fn serve(self) -> Result<()> {
        let addr = format!(
            "{}:{}",
            self.config.proxy.listen.host, self.config.proxy.listen.port
        );

        tracing::info!(listen = %addr, "Proxy ready");

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("binding to {}", addr))?;

        let shutdown_timeout = Duration::from_secs(self.config.proxy.shutdown_timeout_seconds);
        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown_signal(shutdown_timeout))
            .await
            .context("server error")?;

        tracing::info!("Proxy shut down");
        Ok(())
    }
}

/// Build the McpProxy with all backends and per-backend middleware.
async fn build_mcp_proxy(config: &ProxyConfig) -> Result<McpProxy> {
    let mut builder = McpProxy::builder(&config.proxy.name, &config.proxy.version)
        .separator(&config.proxy.separator);

    if let Some(instructions) = &config.proxy.instructions {
        builder = builder.instructions(instructions);
    }

    // Create shared outlier detector if any backend has outlier_detection configured.
    // Use the max of all max_ejection_percent values.
    let outlier_detector = {
        let max_pct = config
            .backends
            .iter()
            .filter_map(|b| b.outlier_detection.as_ref())
            .map(|od| od.max_ejection_percent)
            .max();
        max_pct.map(crate::outlier::OutlierDetector::new)
    };

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
                let mut transport = tower_mcp::client::HttpClientTransport::new(url);
                if let Some(token) = &backend.bearer_token {
                    transport = transport.bearer_token(token);
                }

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
            let layer = crate::retry::build_retry_layer(retry_cfg, &backend.name);
            builder = builder.backend_layer(layer);
        }

        // Hedging (after retry, before concurrency -- hedges are separate requests)
        if let Some(hedge_cfg) = &backend.hedging {
            let delay = Duration::from_millis(hedge_cfg.delay_ms);
            let max_attempts = hedge_cfg.max_hedges + 1; // +1 for the primary request
            tracing::info!(
                backend = %backend.name,
                delay_ms = hedge_cfg.delay_ms,
                max_hedges = hedge_cfg.max_hedges,
                "Applying request hedging"
            );
            let layer = if delay.is_zero() {
                tower_resilience::hedge::HedgeLayer::builder()
                    .no_delay()
                    .max_hedged_attempts(max_attempts)
                    .name(format!("{}-hedge", backend.name))
                    .build()
            } else {
                tower_resilience::hedge::HedgeLayer::builder()
                    .delay(delay)
                    .max_hedged_attempts(max_attempts)
                    .name(format!("{}-hedge", backend.name))
                    .build()
            };
            builder = builder.backend_layer(layer);
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

        // Circuit breaker
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

        // Outlier detection (outermost -- observes errors after all other middleware)
        if let Some(od) = &backend.outlier_detection
            && let Some(ref detector) = outlier_detector
        {
            tracing::info!(
                backend = %backend.name,
                consecutive_errors = od.consecutive_errors,
                base_ejection_seconds = od.base_ejection_seconds,
                max_ejection_percent = od.max_ejection_percent,
                "Applying outlier detection"
            );
            let layer = crate::outlier::OutlierDetectionLayer::new(
                backend.name.clone(),
                od.clone(),
                detector.clone(),
            );
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
    config: &ProxyConfig,
    proxy: McpProxy,
) -> Result<(
    BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    Option<cache::CacheHandle>,
)> {
    let mut service: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
        BoxCloneService::new(proxy);
    let mut cache_handle: Option<cache::CacheHandle> = None;

    // Argument injection (innermost -- merges default/per-tool args into CallTool requests)
    let injection_rules: Vec<_> = config
        .backends
        .iter()
        .filter(|b| !b.default_args.is_empty() || !b.inject_args.is_empty())
        .map(|b| {
            let namespace = format!("{}{}", b.name, config.proxy.separator);
            tracing::info!(
                backend = %b.name,
                default_args = b.default_args.len(),
                tool_rules = b.inject_args.len(),
                "Applying argument injection"
            );
            crate::inject::InjectionRules::new(
                namespace,
                b.default_args.clone(),
                b.inject_args.clone(),
            )
        })
        .collect();

    if !injection_rules.is_empty() {
        service = BoxCloneService::new(crate::inject::InjectArgsService::new(
            service,
            injection_rules,
        ));
    }

    // Canary routing (rewrites requests from primary to canary namespace based on weight)
    let canary_mappings: std::collections::HashMap<String, (String, u32, u32)> = config
        .backends
        .iter()
        .filter_map(|b| {
            b.canary_of.as_ref().map(|primary_name| {
                // Find the primary backend's weight
                let primary_weight = config
                    .backends
                    .iter()
                    .find(|p| p.name == *primary_name)
                    .map(|p| p.weight)
                    .unwrap_or(100);
                (
                    primary_name.clone(),
                    (b.name.clone(), primary_weight, b.weight),
                )
            })
        })
        .collect();

    if !canary_mappings.is_empty() {
        for (primary, (canary, pw, cw)) in &canary_mappings {
            tracing::info!(
                primary = %primary,
                canary = %canary,
                primary_weight = pw,
                canary_weight = cw,
                "Enabling canary routing"
            );
        }
        service = BoxCloneService::new(crate::canary::CanaryService::new(
            service,
            canary_mappings,
            &config.proxy.separator,
        ));
    }

    // Failover routing (deterministic fallback on primary error)
    let failover_mappings: std::collections::HashMap<String, String> = config
        .backends
        .iter()
        .filter_map(|b| {
            b.failover_for
                .as_ref()
                .map(|primary| (primary.clone(), b.name.clone()))
        })
        .collect();

    if !failover_mappings.is_empty() {
        for (primary, failover) in &failover_mappings {
            tracing::info!(
                primary = %primary,
                failover = %failover,
                "Enabling failover routing"
            );
        }
        service = BoxCloneService::new(crate::failover::FailoverService::new(
            service,
            failover_mappings,
            &config.proxy.separator,
        ));
    }

    // Traffic mirroring (sends cloned requests through the proxy)
    let mirror_mappings: std::collections::HashMap<String, (String, u32)> = config
        .backends
        .iter()
        .filter_map(|b| {
            b.mirror_of
                .as_ref()
                .map(|source| (source.clone(), (b.name.clone(), b.mirror_percent)))
        })
        .collect();

    if !mirror_mappings.is_empty() {
        for (source, (mirror, pct)) in &mirror_mappings {
            tracing::info!(
                source = %source,
                mirror = %mirror,
                percent = pct,
                "Enabling traffic mirroring"
            );
        }
        service = BoxCloneService::new(crate::mirror::MirrorService::new(
            service,
            mirror_mappings,
            &config.proxy.separator,
        ));
    }

    // Response caching
    let cache_configs: Vec<_> = config
        .backends
        .iter()
        .filter_map(|b| {
            b.cache
                .as_ref()
                .map(|c| (format!("{}{}", b.name, config.proxy.separator), c))
        })
        .collect();

    if !cache_configs.is_empty() {
        for (ns, cfg) in &cache_configs {
            tracing::info!(
                backend = %ns.trim_end_matches(&config.proxy.separator),
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
        .filter_map(|b| b.build_filter(&config.proxy.separator))
        .collect();

    if !filters.is_empty() {
        for f in &filters {
            tracing::info!(
                backend = %f.namespace.trim_end_matches(&config.proxy.separator),
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
            let ns = format!("{}{}", b.name, config.proxy.separator);
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
    #[cfg(feature = "oauth")]
    {
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

        // Token passthrough (inject ClientToken for forward_auth backends)
        let forward_namespaces: std::collections::HashSet<String> = config
            .backends
            .iter()
            .filter(|b| b.forward_auth)
            .map(|b| format!("{}{}", b.name, config.proxy.separator))
            .collect();

        if !forward_namespaces.is_empty() {
            tracing::info!(
                backends = ?forward_namespaces,
                "Enabling token passthrough for forward_auth backends"
            );
            service = BoxCloneService::new(crate::token::TokenPassthroughService::new(
                service,
                forward_namespaces,
            ));
        }
    }

    // Metrics
    #[cfg(feature = "metrics")]
    if config.observability.metrics.enabled {
        service = BoxCloneService::new(crate::metrics::MetricsService::new(service));
    }

    // Structured access logging
    if config.observability.access_log.enabled {
        tracing::info!("Access logging enabled (target: mcp::access)");
        service = BoxCloneService::new(crate::access_log::AccessLogService::new(service));
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
async fn apply_auth(config: &ProxyConfig, router: Router) -> Result<Router> {
    let router = if let Some(auth) = &config.auth {
        match auth {
            AuthConfig::Bearer { tokens } => {
                tracing::info!(token_count = tokens.len(), "Enabling bearer token auth");
                let validator = StaticBearerValidator::new(tokens.iter().cloned());
                let layer = AuthLayer::new(validator);
                router.layer(layer)
            }
            #[cfg(feature = "oauth")]
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
                    config.proxy.listen.host, config.proxy.listen.port
                );
                let metadata = tower_mcp::oauth::ProtectedResourceMetadata::new(&addr)
                    .authorization_server(issuer);

                let layer = tower_mcp::oauth::OAuthLayer::new(validator, metadata);
                router.layer(layer)
            }
            #[cfg(not(feature = "oauth"))]
            AuthConfig::Jwt { .. } => {
                anyhow::bail!(
                    "JWT auth requires the 'oauth' feature. Rebuild with: cargo install mcp-proxy --features oauth"
                );
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
