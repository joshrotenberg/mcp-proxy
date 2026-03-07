//! Admin API for gateway introspection.
//!
//! Provides endpoints for checking backend health and gateway status.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Extension;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;
use tower_mcp::SessionHandle;
use tower_mcp::proxy::McpProxy;

/// Cached health status, updated periodically by a background task.
#[derive(Clone)]
pub struct AdminState {
    health: Arc<RwLock<Vec<BackendStatus>>>,
    gateway_name: String,
    gateway_version: String,
    backend_count: usize,
}

impl AdminState {
    /// Get a snapshot of backend health status.
    pub async fn health(&self) -> Vec<BackendStatus> {
        self.health.read().await.clone()
    }

    /// Gateway name from config.
    pub fn gateway_name(&self) -> &str {
        &self.gateway_name
    }

    /// Gateway version from config.
    pub fn gateway_version(&self) -> &str {
        &self.gateway_version
    }

    /// Number of configured backends.
    pub fn backend_count(&self) -> usize {
        self.backend_count
    }
}

#[derive(Serialize, Clone)]
pub struct BackendStatus {
    pub namespace: String,
    pub healthy: bool,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub error: Option<String>,
    pub transport: Option<String>,
}

#[derive(Serialize)]
struct AdminBackendsResponse {
    gateway: GatewayInfo,
    backends: Vec<BackendStatus>,
}

#[derive(Serialize)]
struct GatewayInfo {
    name: String,
    version: String,
    backend_count: usize,
    active_sessions: usize,
}

/// Per-backend metadata passed in from config at startup.
#[derive(Clone)]
pub struct BackendMeta {
    pub transport: String,
}

/// Spawn a background task that periodically health-checks backends.
/// Returns the AdminState that admin endpoints read from.
pub fn spawn_health_checker(
    proxy: McpProxy,
    gateway_name: String,
    gateway_version: String,
    backend_count: usize,
    backend_meta: HashMap<String, BackendMeta>,
) -> AdminState {
    let health: Arc<RwLock<Vec<BackendStatus>>> = Arc::new(RwLock::new(Vec::new()));
    let health_writer = Arc::clone(&health);

    // McpProxy is Send+Clone but not Sync, so &McpProxy is not Send.
    // health_check(&self) borrows across .await, making futures !Send.
    // Workaround: run health checks on a dedicated single-threaded runtime
    // where Send is not required.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("admin health check runtime");

        // Track consecutive failure counts across check cycles.
        let mut failure_counts: HashMap<String, u32> = HashMap::new();

        rt.block_on(async move {
            loop {
                let results = proxy.health_check().await;
                let now = Utc::now();
                let statuses: Vec<BackendStatus> = results
                    .into_iter()
                    .map(|h| {
                        let count = failure_counts.entry(h.namespace.clone()).or_insert(0);
                        if h.healthy {
                            *count = 0;
                        } else {
                            *count += 1;
                        }
                        let meta = backend_meta.get(&h.namespace);
                        BackendStatus {
                            namespace: h.namespace,
                            healthy: h.healthy,
                            last_checked_at: Some(now),
                            consecutive_failures: *count,
                            error: if h.healthy {
                                None
                            } else {
                                Some("ping failed".to_string())
                            },
                            transport: meta.map(|m| m.transport.clone()),
                        }
                    })
                    .collect();
                *health_writer.write().await = statuses;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    });

    AdminState {
        health,
        gateway_name,
        gateway_version,
        backend_count,
    }
}

async fn handle_backends(
    Extension(state): Extension<AdminState>,
    Extension(session_handle): Extension<SessionHandle>,
) -> Json<AdminBackendsResponse> {
    let backends = state.health.read().await.clone();
    let active_sessions = session_handle.session_count().await;

    Json(AdminBackendsResponse {
        gateway: GatewayInfo {
            name: state.gateway_name,
            version: state.gateway_version,
            backend_count: state.backend_count,
            active_sessions,
        },
        backends,
    })
}

async fn handle_health(Extension(state): Extension<AdminState>) -> Json<HealthResponse> {
    let backends = state.health.read().await;
    let all_healthy = backends.iter().all(|b| b.healthy);
    let unhealthy: Vec<String> = backends
        .iter()
        .filter(|b| !b.healthy)
        .map(|b| b.namespace.clone())
        .collect();
    Json(HealthResponse {
        status: if all_healthy { "healthy" } else { "degraded" }.to_string(),
        unhealthy_backends: unhealthy,
    })
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    unhealthy_backends: Vec<String>,
}

async fn handle_metrics(
    Extension(handle): Extension<Option<metrics_exporter_prometheus::PrometheusHandle>>,
) -> impl IntoResponse {
    match handle {
        Some(h) => h.render(),
        None => String::new(),
    }
}

async fn handle_cache_stats(
    Extension(cache_handle): Extension<Option<crate::cache::CacheHandle>>,
) -> Json<Vec<crate::cache::CacheStatsSnapshot>> {
    match cache_handle {
        Some(h) => Json(h.stats()),
        None => Json(vec![]),
    }
}

async fn handle_cache_clear(
    Extension(cache_handle): Extension<Option<crate::cache::CacheHandle>>,
) -> &'static str {
    if let Some(h) = cache_handle {
        h.clear();
        "caches cleared"
    } else {
        "no caches configured"
    }
}

/// Build the admin API router.
pub fn admin_router(
    state: AdminState,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    session_handle: SessionHandle,
    cache_handle: Option<crate::cache::CacheHandle>,
) -> Router {
    Router::new()
        .route("/backends", get(handle_backends))
        .route("/health", get(handle_health))
        .route("/cache/stats", get(handle_cache_stats))
        .route("/cache/clear", axum::routing::post(handle_cache_clear))
        .route("/metrics", get(handle_metrics))
        .layer(Extension(state))
        .layer(Extension(metrics_handle))
        .layer(Extension(session_handle))
        .layer(Extension(cache_handle))
}
