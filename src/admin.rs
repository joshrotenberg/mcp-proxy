//! Admin HTTP API for proxy introspection and management.
//!
//! The admin API runs on the same HTTP server as the MCP endpoint and is
//! mounted under the `/admin` path prefix. It provides read-only
//! introspection endpoints and management operations for backends, sessions,
//! caching, and configuration.
//!
//! # Endpoint reference
//!
//! | Method | Path | Description |
//! |--------|------|-------------|
//! | `GET` | `/admin/health` | Aggregated health status of all backends |
//! | `GET` | `/admin/backends` | List all backends with metadata |
//! | `GET` | `/admin/backends/{name}/health` | Health status for a single backend |
//! | `GET` | `/admin/backends/{name}/health/history` | Health state transition history for a backend |
//! | `POST` | `/admin/backends/add` | Add a new backend at runtime |
//! | `PUT` | `/admin/backends/{name}` | Update an existing backend's configuration |
//! | `DELETE` | `/admin/backends/{name}` | Remove a backend at runtime |
//! | `GET` | `/admin/sessions` | List active MCP sessions (summary) |
//! | `GET` | `/admin/sessions/detail` | List active MCP sessions (full detail) |
//! | `DELETE` | `/admin/sessions/{id}` | Terminate a session by ID |
//! | `GET` | `/admin/stats` | Aggregated proxy statistics |
//! | `GET` | `/admin/cache/stats` | Cache hit/miss statistics |
//! | `POST` | `/admin/cache/clear` | Flush the response cache |
//! | `GET` | `/admin/config` | Current proxy configuration (sanitized) |
//! | `PUT` | `/admin/config` | Hot-reload proxy configuration |
//! | `POST` | `/admin/config/validate` | Validate a config without applying it |
//! | `GET` | `/admin/metrics` | Prometheus metrics scrape endpoint (feature `metrics`) |
//! | `GET` | `/admin/openapi.json` | OpenAPI spec (feature `openapi`) |
//!
//! # Health checking
//!
//! Backend health is monitored by a background task ([`spawn_health_checker`])
//! that periodically sends `ping` requests to each backend and records
//! pass/fail status. The cached results are stored in [`AdminState`] and
//! served by the `/admin/health` endpoint without per-request latency.
//!
//! Health state transitions (healthy -> unhealthy or vice versa) are recorded
//! in a ring buffer of [`HealthEvent`] entries (capped at 100 events). The
//! per-backend history is available at `/admin/backends/{name}/health/history`.
//!
//! # Session management
//!
//! The proxy tracks active MCP sessions (SSE and Streamable HTTP transports).
//! The `/admin/sessions` endpoint lists session IDs and metadata, while
//! `/admin/sessions/{id}` (DELETE) terminates a session by closing its
//! transport.
//!
//! # Cache management
//!
//! When response caching is enabled, `/admin/cache/stats` reports hit/miss
//! counts and `/admin/cache/clear` flushes all cached entries.
//!
//! # Authentication
//!
//! Admin endpoints are protected by a token-based auth scheme. The proxy
//! resolves the admin token using a fallback chain:
//!
//! 1. `security.admin_token` -- explicit bearer token for admin access.
//! 2. Proxy's inbound auth config -- reuses the same auth as MCP endpoints.
//! 3. If neither is set, the admin API is open (suitable for local/dev use).
//!
//! The token supports `${ENV_VAR}` syntax for environment variable expansion.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_mcp::SessionHandle;
use tower_mcp::proxy::McpProxy;

/// Cached health status, updated periodically by a background task.
#[derive(Clone)]
pub struct AdminState {
    health: Arc<RwLock<Vec<BackendStatus>>>,
    health_history: Arc<RwLock<Vec<HealthEvent>>>,
    proxy_name: String,
    proxy_version: String,
    backend_count: usize,
}

/// A health state transition event.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HealthEvent {
    /// Backend namespace.
    pub namespace: String,
    /// New health state.
    pub healthy: bool,
    /// When the transition occurred.
    pub timestamp: DateTime<Utc>,
}

/// Maximum health history events to retain.
const MAX_HEALTH_HISTORY: usize = 100;

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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct AdminBackendsResponse {
    proxy: ProxyInfo,
    backends: Vec<BackendStatus>,
}

#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
    let health_history: Arc<RwLock<Vec<HealthEvent>>> = Arc::new(RwLock::new(Vec::new()));
    let history_writer = Arc::clone(&health_history);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("admin health check runtime");

        let mut failure_counts: HashMap<String, u32> = HashMap::new();
        // Track previous health state for transition detection
        let mut prev_healthy: HashMap<String, bool> = HashMap::new();

        rt.block_on(async move {
            loop {
                let results = proxy.health_check().await;
                let now = Utc::now();
                let mut transitions = Vec::new();

                let statuses: Vec<BackendStatus> = results
                    .into_iter()
                    .map(|h| {
                        let count = failure_counts.entry(h.namespace.clone()).or_insert(0);
                        if h.healthy {
                            *count = 0;
                        } else {
                            *count += 1;
                        }

                        // Detect health state transitions
                        let prev = prev_healthy.get(&h.namespace).copied();
                        if prev != Some(h.healthy) {
                            transitions.push(HealthEvent {
                                namespace: h.namespace.clone(),
                                healthy: h.healthy,
                                timestamp: now,
                            });
                            prev_healthy.insert(h.namespace.clone(), h.healthy);
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

                // Record transitions to history
                if !transitions.is_empty() {
                    let mut history = history_writer.write().await;
                    history.extend(transitions);
                    // Trim to max size
                    if history.len() > MAX_HEALTH_HISTORY {
                        let excess = history.len() - MAX_HEALTH_HISTORY;
                        history.drain(..excess);
                    }
                }

                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    });

    AdminState {
        health,
        health_history,
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct HealthResponse {
    status: String,
    unhealthy_backends: Vec<String>,
}

#[cfg(feature = "metrics")]
async fn handle_metrics(
    Extension(handle): Extension<Option<metrics_exporter_prometheus::PrometheusHandle>>,
) -> impl IntoResponse {
    match handle {
        Some(h) => h.render(),
        None => String::new(),
    }
}

#[cfg(not(feature = "metrics"))]
async fn handle_metrics() -> impl IntoResponse {
    String::new()
}

async fn handle_cache_stats(
    Extension(cache_handle): Extension<Option<crate::cache::CacheHandle>>,
) -> Json<Vec<crate::cache::CacheStatsSnapshot>> {
    match cache_handle {
        Some(h) => Json(h.stats().await),
        None => Json(vec![]),
    }
}

async fn handle_cache_clear(
    Extension(cache_handle): Extension<Option<crate::cache::CacheHandle>>,
) -> &'static str {
    if let Some(h) = cache_handle {
        h.clear().await;
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
        health_history: Arc::new(RwLock::new(Vec::new())),
        proxy_name: proxy_name.to_string(),
        proxy_version: proxy_version.to_string(),
        backend_count,
    }
}

/// Metrics handle type -- wraps the Prometheus handle when the feature is enabled.
#[cfg(feature = "metrics")]
pub type MetricsHandle = Option<metrics_exporter_prometheus::PrometheusHandle>;
/// Metrics handle type -- no-op when the metrics feature is disabled.
#[cfg(not(feature = "metrics"))]
pub type MetricsHandle = Option<()>;

// ---------------------------------------------------------------------------
// Management API handlers
// ---------------------------------------------------------------------------

/// Request body for adding an HTTP backend via REST.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct AddBackendRequest {
    /// Backend name (becomes the namespace prefix).
    name: String,
    /// Backend URL.
    url: String,
    /// Optional bearer token for the backend.
    bearer_token: Option<String>,
}

/// Response body for backend operations.
#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct BackendOpResponse {
    ok: bool,
    message: String,
}

async fn handle_add_backend(
    Extension(proxy): Extension<McpProxy>,
    Json(req): Json<AddBackendRequest>,
) -> (StatusCode, Json<BackendOpResponse>) {
    let mut transport = tower_mcp::client::HttpClientTransport::new(&req.url);
    if let Some(token) = &req.bearer_token {
        transport = transport.bearer_token(token);
    }

    match proxy.add_backend(&req.name, transport).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(BackendOpResponse {
                ok: true,
                message: format!("Backend '{}' added", req.name),
            }),
        ),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Failed to add backend: {e}"),
            }),
        ),
    }
}

async fn handle_remove_backend(
    Extension(proxy): Extension<McpProxy>,
    Path(name): Path<String>,
) -> (StatusCode, Json<BackendOpResponse>) {
    if proxy.remove_backend(&name).await {
        (
            StatusCode::OK,
            Json(BackendOpResponse {
                ok: true,
                message: format!("Backend '{}' removed", name),
            }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Backend '{}' not found", name),
            }),
        )
    }
}

async fn handle_get_config(
    Extension(config_toml): Extension<std::sync::Arc<String>>,
) -> impl IntoResponse {
    config_toml.as_str().to_string()
}

async fn handle_validate_config(body: String) -> (StatusCode, Json<BackendOpResponse>) {
    match crate::config::ProxyConfig::parse(&body) {
        Ok(config) => (
            StatusCode::OK,
            Json(BackendOpResponse {
                ok: true,
                message: format!("Valid config with {} backends", config.backends.len()),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Invalid config: {e}"),
            }),
        ),
    }
}

async fn handle_list_sessions(
    Extension(session_handle): Extension<SessionHandle>,
) -> Json<SessionsResponse> {
    Json(SessionsResponse {
        active_sessions: session_handle.session_count().await,
    })
}

#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct SessionsResponse {
    active_sessions: usize,
}

async fn handle_backend_health_history(
    Extension(state): Extension<AdminState>,
    Path(name): Path<String>,
) -> Json<Vec<HealthEvent>> {
    let history = state.health_history.read().await;
    let filtered: Vec<HealthEvent> = history
        .iter()
        .filter(|e| {
            e.namespace == name
                || e.namespace == format!("{name}/")
                || e.namespace.trim_end_matches('/') == name
        })
        .cloned()
        .collect();
    Json(filtered)
}

async fn handle_update_backend(
    Extension(proxy): Extension<McpProxy>,
    Path(name): Path<String>,
    Json(req): Json<UpdateBackendRequest>,
) -> (StatusCode, Json<BackendOpResponse>) {
    // Remove existing backend
    if !proxy.remove_backend(&name).await {
        return (
            StatusCode::NOT_FOUND,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Backend '{name}' not found"),
            }),
        );
    }

    // Re-add with new config
    let mut transport = tower_mcp::client::HttpClientTransport::new(&req.url);
    if let Some(token) = &req.bearer_token {
        transport = transport.bearer_token(token);
    }

    match proxy.add_backend(&name, transport).await {
        Ok(()) => (
            StatusCode::OK,
            Json(BackendOpResponse {
                ok: true,
                message: format!("Backend '{name}' updated"),
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Failed to re-add backend after removal: {e}"),
            }),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct UpdateBackendRequest {
    /// New backend URL.
    url: String,
    /// Optional bearer token for the backend.
    bearer_token: Option<String>,
}

async fn handle_single_backend_health(
    Extension(state): Extension<AdminState>,
    Path(name): Path<String>,
) -> Result<Json<BackendStatus>, StatusCode> {
    let backends = state.health.read().await;
    // Match by namespace (with or without trailing separator)
    backends
        .iter()
        .find(|b| {
            b.namespace == name
                || b.namespace == format!("{name}/")
                || b.namespace.trim_end_matches('/') == name
        })
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn handle_aggregate_stats(
    Extension(state): Extension<AdminState>,
    Extension(session_handle): Extension<SessionHandle>,
) -> Json<AggregateStats> {
    let backends = state.health.read().await;
    let total = backends.len();
    let healthy = backends.iter().filter(|b| b.healthy).count();
    let active_sessions = session_handle.session_count().await;
    Json(AggregateStats {
        total_backends: total,
        healthy_backends: healthy,
        unhealthy_backends: total - healthy,
        active_sessions,
    })
}

#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct AggregateStats {
    total_backends: usize,
    healthy_backends: usize,
    unhealthy_backends: usize,
    active_sessions: usize,
}

async fn handle_list_sessions_detail(
    Extension(session_handle): Extension<SessionHandle>,
) -> Json<Vec<SessionInfoResponse>> {
    let sessions = session_handle.list_sessions().await;
    Json(
        sessions
            .into_iter()
            .map(|s| SessionInfoResponse {
                id: s.id,
                uptime_seconds: s.created_at.as_secs(),
                idle_seconds: s.last_activity.as_secs(),
            })
            .collect(),
    )
}

#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
struct SessionInfoResponse {
    id: String,
    uptime_seconds: u64,
    idle_seconds: u64,
}

async fn handle_terminate_session(
    Extension(session_handle): Extension<SessionHandle>,
    Path(id): Path<String>,
) -> (StatusCode, Json<BackendOpResponse>) {
    if session_handle.terminate_session(&id).await {
        (
            StatusCode::OK,
            Json(BackendOpResponse {
                ok: true,
                message: format!("Session '{id}' terminated"),
            }),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Session '{id}' not found"),
            }),
        )
    }
}

async fn handle_update_config(
    Extension(config_path): Extension<Option<std::path::PathBuf>>,
    body: String,
) -> (StatusCode, Json<BackendOpResponse>) {
    // Detect format from config file extension (or try TOML then YAML)
    let is_yaml = config_path
        .as_ref()
        .and_then(|p| p.extension())
        .is_some_and(|ext| ext == "yaml" || ext == "yml");

    let config = if is_yaml {
        #[cfg(feature = "yaml")]
        {
            crate::config::ProxyConfig::parse_yaml(&body)
        }
        #[cfg(not(feature = "yaml"))]
        {
            Err(anyhow::anyhow!("YAML support requires the 'yaml' feature"))
        }
    } else {
        crate::config::ProxyConfig::parse(&body)
    };

    let config = match config {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(BackendOpResponse {
                    ok: false,
                    message: format!("Invalid config: {e}"),
                }),
            );
        }
    };

    // Write to disk if we have a config path (triggers hot reload if enabled)
    let Some(path) = config_path else {
        return (
            StatusCode::BAD_REQUEST,
            Json(BackendOpResponse {
                ok: false,
                message: "No config file path available (running in --from-mcp-json mode?)"
                    .to_string(),
            }),
        );
    };

    if let Err(e) = std::fs::write(&path, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(BackendOpResponse {
                ok: false,
                message: format!("Failed to write config: {e}"),
            }),
        );
    }

    (
        StatusCode::OK,
        Json(BackendOpResponse {
            ok: true,
            message: format!(
                "Config updated ({} backends). Hot reload will apply changes if enabled.",
                config.backends.len()
            ),
        }),
    )
}

/// OpenAPI spec for the admin API.
///
/// Available at `GET /admin/openapi.json` when the `openapi` feature is enabled.
#[cfg(feature = "openapi")]
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "mcp-proxy Admin API",
        description = "REST API for managing and monitoring the MCP proxy.",
        version = "0.1.0",
    ),
    components(schemas(
        AdminBackendsResponse,
        ProxyInfo,
        BackendStatus,
        HealthResponse,
        crate::cache::CacheStatsSnapshot,
        SessionsResponse,
        AddBackendRequest,
        BackendOpResponse,
    ))
)]
struct ApiDoc;

#[cfg(feature = "openapi")]
async fn handle_openapi() -> impl IntoResponse {
    axum::Json(<ApiDoc as utoipa::OpenApi>::openapi())
}

/// Build the admin API router.
pub fn admin_router(
    state: AdminState,
    metrics_handle: MetricsHandle,
    session_handle: SessionHandle,
    cache_handle: Option<crate::cache::CacheHandle>,
    proxy: McpProxy,
    config: &crate::config::ProxyConfig,
    config_path: Option<std::path::PathBuf>,
) -> Router {
    let config_toml = std::sync::Arc::new(toml::to_string_pretty(config).unwrap_or_default());

    let router = Router::new()
        // Read-only endpoints
        .route("/backends", get(handle_backends))
        .route("/health", get(handle_health))
        .route("/cache/stats", get(handle_cache_stats))
        .route("/cache/clear", axum::routing::post(handle_cache_clear))
        .route("/metrics", get(handle_metrics))
        .route("/sessions", get(handle_list_sessions))
        .route("/sessions/detail", get(handle_list_sessions_detail))
        .route("/sessions/{id}", delete(handle_terminate_session))
        .route("/stats", get(handle_aggregate_stats))
        .route("/config", get(handle_get_config).put(handle_update_config))
        .route("/config/validate", post(handle_validate_config))
        // Per-backend endpoints
        .route("/backends/{name}/health", get(handle_single_backend_health))
        .route(
            "/backends/{name}/health/history",
            get(handle_backend_health_history),
        )
        // Management endpoints
        .route("/backends/add", post(handle_add_backend))
        .route(
            "/backends/{name}",
            delete(handle_remove_backend).put(handle_update_backend),
        )
        .layer(Extension(state))
        .layer(Extension(session_handle))
        .layer(Extension(cache_handle))
        .layer(Extension(proxy))
        .layer(Extension(config_toml))
        .layer(Extension(config_path));

    #[cfg(feature = "metrics")]
    let router = router.layer(Extension(metrics_handle));
    #[cfg(not(feature = "metrics"))]
    let _ = metrics_handle;

    #[cfg(feature = "openapi")]
    let router = router.route("/openapi.json", get(handle_openapi));

    // Admin API auth: admin_token takes priority, then fall back to proxy bearer tokens
    let admin_tokens = resolve_admin_tokens(config);
    if !admin_tokens.is_empty() {
        tracing::info!(token_count = admin_tokens.len(), "Admin API auth enabled");
        let validator = tower_mcp::auth::StaticBearerValidator::new(admin_tokens);
        let layer = tower_mcp::auth::AuthLayer::new(validator);
        router.layer(layer)
    } else {
        router
    }
}

/// Resolve admin auth tokens: admin_token if set, otherwise proxy bearer tokens.
fn resolve_admin_tokens(config: &crate::config::ProxyConfig) -> Vec<String> {
    // Explicit admin token takes priority
    if let Some(ref token) = config.security.admin_token {
        return vec![token.clone()];
    }

    // Fall back to proxy bearer tokens
    match &config.auth {
        Some(crate::config::AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) => {
            let mut all: Vec<String> = tokens.clone();
            all.extend(scoped_tokens.iter().map(|st| st.token.clone()));
            all
        }
        _ => vec![],
    }
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

    async fn make_test_proxy() -> McpProxy {
        use tower_mcp::client::ChannelTransport;
        use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

        let router = McpRouter::new().server_info("test", "1.0.0").tool(
            ToolBuilder::new("ping")
                .description("Ping")
                .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("pong")) })
                .build(),
        );

        McpProxy::builder("test-proxy", "1.0.0")
            .backend("test", ChannelTransport::new(router))
            .await
            .build_strict()
            .await
            .unwrap()
    }

    fn make_test_config() -> crate::config::ProxyConfig {
        crate::config::ProxyConfig::parse(
            r#"
            [proxy]
            name = "test"
            [proxy.listen]

            [[backends]]
            name = "echo"
            transport = "stdio"
            command = "echo"
            "#,
        )
        .unwrap()
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
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/health").await;
        assert_eq!(json["status"], "healthy");
        assert!(json["unhealthy_backends"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_health_endpoint_degraded() {
        let state = make_state(vec![healthy_backend("db/"), unhealthy_backend("flaky/")]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

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
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

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
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/cache/stats").await;
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_cache_clear_no_cache() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

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
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

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

    #[tokio::test]
    async fn test_single_backend_health() {
        let state = make_state(vec![healthy_backend("db/"), unhealthy_backend("flaky/")]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/backends/db/health").await;
        assert_eq!(json["namespace"], "db/");
        assert!(json["healthy"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_single_backend_health_not_found() {
        let state = make_state(vec![healthy_backend("db/")]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/backends/nonexistent/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_aggregate_stats() {
        let state = make_state(vec![healthy_backend("db/"), unhealthy_backend("flaky/")]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/stats").await;
        assert_eq!(json["total_backends"], 2);
        assert_eq!(json["healthy_backends"], 1);
        assert_eq!(json["unhealthy_backends"], 1);
    }

    #[tokio::test]
    async fn test_health_history_empty() {
        let state = make_state(vec![healthy_backend("db/")]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/backends/db/health/history").await;
        assert!(json.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_health_history_with_events() {
        let state = make_state(vec![healthy_backend("db/")]);
        // Manually insert history events
        {
            let mut history = state.health_history.write().await;
            history.push(HealthEvent {
                namespace: "db/".to_string(),
                healthy: true,
                timestamp: Utc::now(),
            });
            history.push(HealthEvent {
                namespace: "db/".to_string(),
                healthy: false,
                timestamp: Utc::now(),
            });
            history.push(HealthEvent {
                namespace: "other/".to_string(),
                healthy: false,
                timestamp: Utc::now(),
            });
        }

        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        // Should only return events for "db"
        let json = get_json(&router, "/backends/db/health/history").await;
        let events = json.as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0]["healthy"].as_bool().unwrap());
        assert!(!events[1]["healthy"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_sessions_endpoint() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/sessions").await;
        assert_eq!(json["active_sessions"], 0);
    }

    #[tokio::test]
    async fn test_sessions_detail_empty() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let json = get_json(&router, "/sessions/detail").await;
        let sessions = json.as_array().unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_terminate_session_not_found() {
        let state = make_state(vec![]);
        let session_handle = make_session_handle();
        let router = admin_router(
            state,
            None,
            session_handle,
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/sessions/nonexistent-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!json["ok"].as_bool().unwrap());
        assert!(json["message"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_sessions_detail_with_active_session() {
        // Create an HTTP transport and send an initialize request to establish a session
        let svc = tower::util::BoxCloneService::new(tower::service_fn(
            |_req: tower_mcp::RouterRequest| async {
                Ok::<_, std::convert::Infallible>(tower_mcp::RouterResponse {
                    id: tower_mcp::protocol::RequestId::Number(1),
                    inner: Ok(tower_mcp::protocol::McpResponse::Initialize(
                        tower_mcp::protocol::InitializeResult {
                            protocol_version: "2025-03-26".to_string(),
                            server_info: tower_mcp::protocol::Implementation {
                                name: "test".to_string(),
                                version: "1.0.0".to_string(),
                                ..Default::default()
                            },
                            capabilities: Default::default(),
                            instructions: None,
                            meta: None,
                        },
                    )),
                })
            },
        ));

        let (http_router, session_handle) =
            tower_mcp::transport::http::HttpTransport::from_service(svc).into_router_with_handle();

        // Send an initialize request to create a session
        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "clientInfo": { "name": "test", "version": "1.0.0" },
                "capabilities": {}
            }
        });

        let resp = http_router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&init_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Extract session ID from response header
        let session_id = resp
            .headers()
            .get("mcp-session-id")
            .expect("initialize response should include mcp-session-id header")
            .to_str()
            .unwrap()
            .to_string();

        // Verify sessions are visible via admin endpoint
        let state = make_state(vec![]);
        let admin = admin_router(
            state,
            None,
            session_handle.clone(),
            None,
            make_test_proxy().await,
            &make_test_config(),
            None,
        );

        // Check session count
        let json = get_json(&admin, "/sessions").await;
        assert_eq!(json["active_sessions"], 1);

        // Check session detail
        let json = get_json(&admin, "/sessions/detail").await;
        let sessions = json.as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0]["id"].as_str().unwrap().is_empty());
        assert!(sessions[0]["uptime_seconds"].is_number());
        assert!(sessions[0]["idle_seconds"].is_number());

        // Terminate the session
        let resp = admin
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["ok"].as_bool().unwrap());

        // Verify session is gone
        let json = get_json(&admin, "/sessions").await;
        assert_eq!(json["active_sessions"], 0);

        // Terminating again returns not found
        let resp = admin
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/sessions/{}", session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
