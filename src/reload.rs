//! Hot reload: watch the config file for changes and add new backends dynamically.
//!
//! Only new backends are added. Removed or modified backends are logged as warnings
//! since the proxy currently supports add-only dynamic updates.

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::process::Command;
use tower::util::BoxCloneService;
use tower_mcp::proxy::{BackendService, McpProxy};
use tower_mcp::{RouterRequest, RouterResponse};

use crate::config::{BackendConfig, GatewayConfig, TransportType};

/// Spawn a background task that watches the config file and adds new backends.
pub fn spawn_config_watcher(config_path: PathBuf, proxy: McpProxy) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("hot reload runtime");
        rt.block_on(watch_loop(config_path, proxy));
    });
}

async fn watch_loop(config_path: PathBuf, proxy: McpProxy) {
    let (tx, rx) = std_mpsc::channel();

    let mut debouncer = match new_debouncer(Duration::from_secs(2), tx) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create file watcher, hot reload disabled");
            return;
        }
    };

    if let Err(e) = debouncer
        .watcher()
        .watch(&config_path, notify::RecursiveMode::NonRecursive)
    {
        tracing::error!(
            path = %config_path.display(),
            error = %e,
            "Failed to watch config file, hot reload disabled"
        );
        return;
    }

    tracing::info!(path = %config_path.display(), "Hot reload watching config file");

    // Track known backend names
    let mut known_backends: HashSet<String> = {
        if let Ok(config) = GatewayConfig::load(&config_path) {
            config.backends.iter().map(|b| b.name.clone()).collect()
        } else {
            HashSet::new()
        }
    };

    loop {
        // Block until a file event arrives (this is a std::sync channel)
        let events = match rx.recv() {
            Ok(Ok(events)) => events,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "File watcher error");
                continue;
            }
            Err(_) => {
                tracing::info!("File watcher channel closed, stopping hot reload");
                break;
            }
        };

        // Only process write events
        let has_write = events
            .iter()
            .any(|e| matches!(e.kind, DebouncedEventKind::Any));
        if !has_write {
            continue;
        }

        tracing::info!("Config file changed, checking for new backends");

        let mut new_config = match GatewayConfig::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse updated config, skipping reload");
                continue;
            }
        };
        new_config.resolve_env_vars();

        let new_names: HashSet<String> =
            new_config.backends.iter().map(|b| b.name.clone()).collect();

        // Detect removed backends
        for removed in known_backends.difference(&new_names) {
            tracing::warn!(
                backend = %removed,
                "Backend removed from config but cannot be removed at runtime (add-only)"
            );
        }

        // Detect modified backends (name exists but config may have changed)
        // We don't track config content, so we can't detect modifications.
        // Just log that existing backends are not updated.

        // Add new backends
        for backend in &new_config.backends {
            if known_backends.contains(&backend.name) {
                continue;
            }

            tracing::info!(
                name = %backend.name,
                transport = ?backend.transport,
                "Adding new backend via hot reload"
            );

            if let Err(e) = add_backend(&proxy, backend).await {
                tracing::error!(
                    backend = %backend.name,
                    error = %e,
                    "Failed to add backend via hot reload"
                );
            } else {
                tracing::info!(backend = %backend.name, "Backend added via hot reload");
                known_backends.insert(backend.name.clone());
            }
        }
    }
}

/// Connect and add a single backend to the proxy, including per-backend middleware.
async fn add_backend(proxy: &McpProxy, backend: &BackendConfig) -> anyhow::Result<()> {
    let has_middleware = backend.timeout.is_some()
        || backend.circuit_breaker.is_some()
        || backend.rate_limit.is_some()
        || backend.concurrency.is_some()
        || backend.retry.is_some()
        || backend.hedging.is_some()
        || backend.outlier_detection.is_some();

    match backend.transport {
        TransportType::Stdio => {
            let command = backend
                .command
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("stdio backend requires 'command'"))?;
            let args: Vec<&str> = backend.args.iter().map(|s| s.as_str()).collect();

            let mut cmd = Command::new(command);
            cmd.args(&args);
            for (key, value) in &backend.env {
                cmd.env(key, value);
            }

            let transport =
                tower_mcp::client::StdioClientTransport::spawn_command(&mut cmd).await?;

            if has_middleware {
                let layer = build_backend_layer(backend);
                proxy
                    .add_backend_with_layer(&backend.name, transport, layer)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            } else {
                proxy
                    .add_backend(&backend.name, transport)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            }
        }
        TransportType::Http => {
            let url = backend
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("http backend requires 'url'"))?;
            let mut transport = tower_mcp::client::HttpClientTransport::new(url);
            if let Some(token) = &backend.bearer_token {
                transport = transport.bearer_token(token);
            }

            if has_middleware {
                let layer = build_backend_layer(backend);
                proxy
                    .add_backend_with_layer(&backend.name, transport, layer)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            } else {
                proxy
                    .add_backend(&backend.name, transport)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
            }
        }
    }

    if has_middleware {
        tracing::info!(
            backend = %backend.name,
            timeout = backend.timeout.is_some(),
            circuit_breaker = backend.circuit_breaker.is_some(),
            rate_limit = backend.rate_limit.is_some(),
            concurrency = backend.concurrency.is_some(),
            "Per-backend middleware applied to hot-reloaded backend"
        );
    }

    Ok(())
}

/// A type-erasing layer that builds the full per-backend middleware stack.
///
/// Uses `BoxCloneService` to erase the composed middleware types, allowing
/// arbitrary combinations of optional layers.
struct BackendMiddlewareLayer {
    build_fn: Box<
        dyn Fn(BackendService) -> BoxCloneService<RouterRequest, RouterResponse, Infallible> + Send,
    >,
}

impl tower::Layer<BackendService> for BackendMiddlewareLayer {
    type Service = BoxCloneService<RouterRequest, RouterResponse, Infallible>;

    fn layer(&self, inner: BackendService) -> Self::Service {
        (self.build_fn)(inner)
    }
}

/// Build a type-erased layer for per-backend middleware from config.
///
/// Layers are applied inner to outer:
/// retry -> concurrency -> rate limit -> timeout -> circuit breaker -> outlier detection.
fn build_backend_layer(backend: &BackendConfig) -> BackendMiddlewareLayer {
    let retry_config = backend.retry.clone();
    let concurrency = backend.concurrency.as_ref().map(|cc| cc.max_concurrent);
    let rate_limit = backend
        .rate_limit
        .as_ref()
        .map(|rl| (rl.requests, rl.period_seconds));
    let timeout_secs = backend.timeout.as_ref().map(|t| t.seconds);
    let circuit_breaker = backend.circuit_breaker.as_ref().map(|cb| {
        (
            cb.failure_rate_threshold,
            cb.minimum_calls,
            cb.wait_duration_seconds,
            cb.permitted_calls_in_half_open,
        )
    });
    let hedging = backend.hedging.clone();
    let outlier = backend.outlier_detection.clone();
    let name = backend.name.clone();

    BackendMiddlewareLayer {
        build_fn: Box::new(move |inner: BackendService| {
            let mut svc: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
                BoxCloneService::new(inner);

            // Retry (innermost)
            if let Some(ref retry_cfg) = retry_config {
                let policy = crate::retry::McpRetryPolicy::from_config(retry_cfg);
                let retried = tower::Layer::layer(&tower::retry::RetryLayer::new(policy), svc);
                svc = BoxCloneService::new(tower_mcp::CatchError::new(retried));
            }

            // Hedging
            if let Some(ref hedge_cfg) = hedging {
                let delay = Duration::from_millis(hedge_cfg.delay_ms);
                let max_attempts = hedge_cfg.max_hedges + 1;
                let layer = if delay.is_zero() {
                    tower_resilience::hedge::HedgeLayer::builder()
                        .no_delay()
                        .max_hedged_attempts(max_attempts)
                        .name(format!("{}-hedge", name))
                        .build()
                } else {
                    tower_resilience::hedge::HedgeLayer::builder()
                        .delay(delay)
                        .max_hedged_attempts(max_attempts)
                        .name(format!("{}-hedge", name))
                        .build()
                };
                let hedged = tower::Layer::layer(&layer, svc);
                svc = BoxCloneService::new(tower_mcp::CatchError::new(hedged));
            }

            // Concurrency limit
            if let Some(max) = concurrency {
                let limited =
                    tower::Layer::layer(&tower::limit::ConcurrencyLimitLayer::new(max), svc);
                svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
            }

            // Rate limit
            if let Some((requests, period_seconds)) = rate_limit {
                let layer = tower_resilience::ratelimiter::RateLimiterLayer::builder()
                    .limit_for_period(requests)
                    .refresh_period(Duration::from_secs(period_seconds))
                    .name(format!("{}-ratelimit", name))
                    .build();
                let limited = tower::Layer::layer(&layer, svc);
                svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
            }

            // Timeout
            if let Some(seconds) = timeout_secs {
                let limited = tower::Layer::layer(
                    &tower::timeout::TimeoutLayer::new(Duration::from_secs(seconds)),
                    svc,
                );
                svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
            }

            // Circuit breaker
            if let Some((failure_rate, min_calls, wait_secs, half_open)) = circuit_breaker {
                let layer = tower_resilience::circuitbreaker::CircuitBreakerLayer::builder()
                    .failure_rate_threshold(failure_rate)
                    .minimum_number_of_calls(min_calls)
                    .wait_duration_in_open(Duration::from_secs(wait_secs))
                    .permitted_calls_in_half_open(half_open)
                    .name(format!("{}-cb", name))
                    .build();
                let limited = tower::Layer::layer(&layer, svc);
                svc = BoxCloneService::new(tower_mcp::CatchError::new(limited));
            }

            // Outlier detection (outermost)
            if let Some(ref od_config) = outlier {
                // Hot-reloaded backends get their own detector (single-backend scope).
                // The main gateway build path uses a shared detector across all backends.
                let detector = crate::outlier::OutlierDetector::new(od_config.max_ejection_percent);
                let layer = crate::outlier::OutlierDetectionLayer::new(
                    name.clone(),
                    od_config.clone(),
                    detector,
                );
                let od_svc = tower::Layer::layer(&layer, svc);
                svc = BoxCloneService::new(od_svc);
            }

            svc
        }),
    }
}
