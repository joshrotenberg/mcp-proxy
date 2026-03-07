//! Admin API for gateway introspection.
//!
//! Provides endpoints for checking backend health and gateway status.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Extension;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
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

/// Spawn a background task that periodically health-checks backends.
/// Returns the AdminState that admin endpoints read from.
pub fn spawn_health_checker(
    proxy: McpProxy,
    gateway_name: String,
    gateway_version: String,
    backend_count: usize,
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
        rt.block_on(async move {
            loop {
                let results = proxy.health_check().await;
                let statuses: Vec<BackendStatus> = results
                    .into_iter()
                    .map(|h| BackendStatus {
                        namespace: h.namespace,
                        healthy: h.healthy,
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

async fn handle_metrics(
    Extension(handle): Extension<Option<metrics_exporter_prometheus::PrometheusHandle>>,
) -> impl IntoResponse {
    match handle {
        Some(h) => h.render(),
        None => String::new(),
    }
}

/// Build the admin API router.
pub fn admin_router(
    state: AdminState,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    session_handle: SessionHandle,
) -> Router {
    Router::new()
        .route("/backends", get(handle_backends))
        .route("/metrics", get(handle_metrics))
        .layer(Extension(state))
        .layer(Extension(metrics_handle))
        .layer(Extension(session_handle))
}
