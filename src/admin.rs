//! Admin API for proxy introspection.
//!
//! Provides endpoints for checking backend health and proxy status.

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
    proxy_name: String,
    proxy_version: String,
    backend_count: usize,
}

impl AdminState {
    /// Get a snapshot of backend health status.
    pub async fn health(&self) -> Vec<BackendStatus> {
        self.health.read().await.clone()
    }

    /// Proxy name from config.
    pub fn proxy_name(&self) -> &str {
        &self.proxy_name
    }

    /// Proxy version from config.
    pub fn proxy_version(&self) -> &str {
        &self.proxy_version
    }

    /// Number of configured backends.
    pub fn backend_count(&self) -> usize {
        self.backend_count
    }
}

/// Health status of a single backend, updated by the background health checker.
#[derive(Serialize, Clone)]
pub struct BackendStatus {
    /// Backend namespace (e.g. "db/").
    pub namespace: String,
    /// Whether the backend responded to the last health check.
    pub healthy: bool,
    /// Timestamp of the last health check.
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Number of consecutive failed health checks.
    pub consecutive_failures: u32,
    /// Last error message from a failed health check.
    pub error: Option<String>,
    /// Transport type (e.g. "stdio", "http").
    pub transport: Option<String>,
}

#[derive(Serialize)]
struct AdminBackendsResponse {
    proxy: ProxyInfo,
    backends: Vec<BackendStatus>,
}

#[derive(Serialize)]
struct ProxyInfo {
    name: String,
    version: String,
    backend_count: usize,
    active_sessions: usize,
}

/// Per-backend metadata passed in from config at startup.
#[derive(Clone)]
pub struct BackendMeta {
    /// Transport type string (e.g. "stdio", "http").
    pub transport: String,
}

/// Spawn a background task that periodically health-checks backends.
/// Returns the AdminState that admin endpoints read from.
pub fn spawn_health_checker(
    proxy: McpProxy,
    proxy_name: String,
    proxy_version: String,
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
        proxy_name,
        proxy_version,
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
        proxy: ProxyInfo {
            name: state.proxy_name,
            version: state.proxy_version,
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

/// Create an `AdminState` directly for testing.
#[cfg(test)]
fn test_admin_state(
    proxy_name: &str,
    proxy_version: &str,
    backend_count: usize,
    statuses: Vec<BackendStatus>,
) -> AdminState {
    AdminState {
        health: Arc::new(RwLock::new(statuses)),
        proxy_name: proxy_name.to_string(),
        proxy_version: proxy_version.to_string(),
        backend_count,
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn make_state(statuses: Vec<BackendStatus>) -> AdminState {
        test_admin_state("test-gw", "1.0.0", statuses.len(), statuses)
    }

    fn healthy_backend(name: &str) -> BackendStatus {
        BackendStatus {
            namespace: name.to_string(),
            healthy: true,
            last_checked_at: Some(Utc::now()),
            consecutive_failures: 0,
            error: None,
            transport: Some("http".to_string()),
        }
    }

    fn unhealthy_backend(name: &str) -> BackendStatus {
        BackendStatus {
            namespace: name.to_string(),
            healthy: false,
            last_checked_at: Some(Utc::now()),
            consecutive_failures: 3,
            error: Some("ping failed".to_string()),
            transport: Some("stdio".to_string()),
        }
    }

    fn make_session_handle() -> SessionHandle {
        // Create a session handle via HttpTransport (the only public way)
        let svc = tower::util::BoxCloneService::new(tower::service_fn(
            |_req: tower_mcp::RouterRequest| async {
                Ok::<_, std::convert::Infallible>(tower_mcp::RouterResponse {
                    id: tower_mcp::protocol::RequestId::Number(1),
                    inner: Ok(tower_mcp::protocol::McpResponse::Pong(Default::default())),
                })
            },
        ));
        let (_, handle) =
            tower_mcp::transport::http::HttpTransport::from_service(svc).into_router_with_handle();
        handle
    }

    async fn get_json(router: &Router, path: &str) -> serde_json::Value {
        let resp = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn test_admin_state_accessors() {
        let state = make_state(vec![healthy_backend("db/")]);
        assert_eq!(state.proxy_name(), "test-gw");
        assert_eq!(state.proxy_version(), "1.0.0");
        assert_eq!(state.backend_count(), 1);

        let health = state.health().await;
        assert_eq!(health.len(), 1);
        assert!(health[0].healthy);
    }

    #[tokio::test]
    async fn test_health_endpoint_all_healthy() {
        let state = make_state(vec![healthy_backend("db/"), healthy_backend("api/")]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let json = get_json(&router, "/health").await;
        assert_eq!(json["status"], "healthy");
        assert!(json["unhealthy_backends"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_health_endpoint_degraded() {
        let state = make_state(vec![healthy_backend("db/"), unhealthy_backend("flaky/")]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let json = get_json(&router, "/health").await;
        assert_eq!(json["status"], "degraded");
        let unhealthy = json["unhealthy_backends"].as_array().unwrap();
        assert_eq!(unhealthy.len(), 1);
        assert_eq!(unhealthy[0], "flaky/");
    }

    #[tokio::test]
    async fn test_backends_endpoint() {
        let state = make_state(vec![healthy_backend("db/")]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let json = get_json(&router, "/backends").await;
        assert_eq!(json["proxy"]["name"], "test-gw");
        assert_eq!(json["proxy"]["version"], "1.0.0");
        assert_eq!(json["proxy"]["backend_count"], 1);
        assert_eq!(json["backends"].as_array().unwrap().len(), 1);
        assert_eq!(json["backends"][0]["namespace"], "db/");
        assert!(json["backends"][0]["healthy"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_cache_stats_no_cache() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let json = get_json(&router, "/cache/stats").await;
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_cache_clear_no_cache() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/cache/clear")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"no caches configured");
    }

    #[tokio::test]
    async fn test_metrics_endpoint_no_recorder() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(state, None, session_handle, None);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }
}
