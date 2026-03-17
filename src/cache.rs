//! Response caching middleware for the proxy.
//!
//! Caches `ReadResource` and `CallTool` responses with per-backend TTL.
//! Cache keys are derived from the request type, name/URI, and arguments.
//!
//! # Cache Backends
//!
//! The cache backend is configurable via `[cache]` in the proxy config:
//!
//! - `"memory"` (default): In-process moka cache. Fast, no external deps.
//! - `"redis"`: External Redis cache. Shared across instances. Requires the
//!   `redis-cache` feature flag.
//! - `"sqlite"`: Local SQLite cache. Persistent across restarts. Requires the
//!   `sqlite-cache` feature flag.
//!
//! # Per-Backend Configuration
//!
//! ```toml
//! [[backends]]
//! name = "slow-api"
//! transport = "http"
//! url = "http://localhost:8080"
//!
//! [backends.cache]
//! resource_ttl_seconds = 300
//! tool_ttl_seconds = 60
//! max_entries = 1000
//! ```

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use moka::future::Cache;
use serde::Serialize;
use tower::{Layer, Service};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

use crate::config::{BackendCacheConfig, CacheBackendConfig};

/// Pluggable cache storage backend.
///
/// Each variant provides the same logical operations (get, insert, invalidate,
/// count) but differs in where entries are stored:
///
/// - [`Memory`](CacheStore::Memory): in-process moka cache (default)
/// - [`Redis`](CacheStore::Redis): external Redis server (requires `redis-cache` feature)
/// - [`Sqlite`](CacheStore::Sqlite): local SQLite database (requires `sqlite-cache` feature)
#[derive(Clone)]
pub(crate) enum CacheStore {
    /// In-process moka cache.
    Memory(Cache<String, RouterResponse>),
    /// Redis-backed cache.
    #[cfg(feature = "redis-cache")]
    Redis {
        client: redis::Client,
        prefix: String,
        ttl: Duration,
    },
    /// SQLite-backed cache.
    #[cfg(feature = "sqlite-cache")]
    Sqlite {
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        ttl: Duration,
    },
}

impl CacheStore {
    /// Retrieve a cached response by key.
    async fn get(&self, key: &str) -> Option<RouterResponse> {
        match self {
            CacheStore::Memory(cache) => cache.get(key).await,
            #[cfg(feature = "redis-cache")]
            CacheStore::Redis {
                client,
                prefix,
                ttl: _,
            } => {
                let full_key = format!("{prefix}{key}");
                let mut conn = client.get_multiplexed_async_connection().await.ok()?;
                let data: Option<String> =
                    redis::AsyncCommands::get(&mut conn, &full_key).await.ok()?;
                data.and_then(|s| serde_json::from_str(&s).ok())
            }
            #[cfg(feature = "sqlite-cache")]
            CacheStore::Sqlite { conn, ttl: _ } => {
                let key = key.to_string();
                let conn = conn.lock().ok()?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let result: Option<String> = conn
                    .query_row(
                        "SELECT value FROM cache_entries WHERE key = ?1 AND expires_at > ?2",
                        rusqlite::params![key, now],
                        |row| row.get(0),
                    )
                    .ok();
                result.and_then(|s| serde_json::from_str(&s).ok())
            }
        }
    }

    /// Insert a response into the cache.
    async fn insert(&self, key: String, value: RouterResponse) {
        match self {
            CacheStore::Memory(cache) => {
                cache.insert(key, value).await;
            }
            #[cfg(feature = "redis-cache")]
            CacheStore::Redis {
                client,
                prefix,
                ttl,
            } => {
                let full_key = format!("{prefix}{key}");
                if let Ok(json) = serde_json::to_string(&value)
                    && let Ok(mut conn) = client.get_multiplexed_async_connection().await
                {
                    let _: Result<(), _> =
                        redis::AsyncCommands::set_ex(&mut conn, &full_key, &json, ttl.as_secs())
                            .await;
                }
            }
            #[cfg(feature = "sqlite-cache")]
            CacheStore::Sqlite { conn, ttl } => {
                if let Ok(json) = serde_json::to_string(&value) {
                    let expires_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64
                        + ttl.as_secs() as i64;
                    if let Ok(conn) = conn.lock() {
                        let _ = conn.execute(
                            "INSERT OR REPLACE INTO cache_entries (key, value, expires_at) VALUES (?1, ?2, ?3)",
                            rusqlite::params![key, json, expires_at],
                        );
                    }
                }
            }
        }
    }

    /// Remove all entries from the cache.
    async fn invalidate_all(&self) {
        match self {
            CacheStore::Memory(cache) => {
                cache.invalidate_all();
            }
            #[cfg(feature = "redis-cache")]
            CacheStore::Redis {
                client,
                prefix,
                ttl: _,
            } => {
                if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
                    let pattern = format!("{prefix}*");
                    let keys: Vec<String> = redis::AsyncCommands::keys(&mut conn, &pattern)
                        .await
                        .unwrap_or_default();
                    if !keys.is_empty() {
                        let _: Result<(), _> = redis::AsyncCommands::del(&mut conn, &keys).await;
                    }
                }
            }
            #[cfg(feature = "sqlite-cache")]
            CacheStore::Sqlite { conn, ttl: _ } => {
                if let Ok(conn) = conn.lock() {
                    let _ = conn.execute("DELETE FROM cache_entries", []);
                }
            }
        }
    }

    /// Return the approximate number of entries in the cache.
    async fn entry_count(&self) -> u64 {
        match self {
            CacheStore::Memory(cache) => cache.entry_count(),
            #[cfg(feature = "redis-cache")]
            CacheStore::Redis {
                client,
                prefix,
                ttl: _,
            } => {
                if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
                    let pattern = format!("{prefix}*");
                    let keys: Vec<String> = redis::AsyncCommands::keys(&mut conn, &pattern)
                        .await
                        .unwrap_or_default();
                    keys.len() as u64
                } else {
                    0
                }
            }
            #[cfg(feature = "sqlite-cache")]
            CacheStore::Sqlite { conn, ttl: _ } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if let Ok(conn) = conn.lock() {
                    conn.query_row(
                        "SELECT COUNT(*) FROM cache_entries WHERE expires_at > ?1",
                        rusqlite::params![now],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0) as u64
                } else {
                    0
                }
            }
        }
    }
}

/// Build a [`CacheStore`] from the global cache backend configuration and
/// a per-backend TTL.
fn build_cache_store(
    backend_config: &CacheBackendConfig,
    ttl: Duration,
    max_entries: u64,
) -> CacheStore {
    match backend_config.backend.as_str() {
        #[cfg(feature = "redis-cache")]
        "redis" => {
            let url = backend_config.url.as_deref().unwrap_or("redis://127.0.0.1");
            let client =
                redis::Client::open(url).expect("invalid Redis URL in cache configuration");
            CacheStore::Redis {
                client,
                prefix: backend_config.prefix.clone(),
                ttl,
            }
        }
        #[cfg(feature = "sqlite-cache")]
        "sqlite" => {
            let path = backend_config.url.as_deref().unwrap_or("cache.db");
            let conn =
                rusqlite::Connection::open(path).expect("failed to open SQLite cache database");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS cache_entries (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    expires_at INTEGER NOT NULL
                )",
            )
            .expect("failed to create SQLite cache table");
            CacheStore::Sqlite {
                conn: Arc::new(std::sync::Mutex::new(conn)),
                ttl,
            }
        }
        // Default: memory backend (also handles "memory" explicitly)
        _ => CacheStore::Memory(
            Cache::builder()
                .max_capacity(max_entries)
                .time_to_live(ttl)
                .build(),
        ),
    }
}

/// Per-backend cache with separate resource and tool caches (different TTLs).
#[derive(Clone)]
struct BackendCache {
    namespace: String,
    resource_cache: Option<CacheStore>,
    tool_cache: Option<CacheStore>,
    stats: Arc<CacheStats>,
}

/// Atomic hit/miss counters for a backend cache.
struct CacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
}

impl CacheStats {
    fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}

/// Snapshot of cache statistics for a single backend.
///
/// Returned by [`CacheHandle::stats()`] to report hit/miss rates
/// and entry counts per cached namespace.
#[derive(Serialize, Clone)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CacheStatsSnapshot {
    /// Backend namespace this cache covers.
    pub namespace: String,
    /// Total cache hits.
    pub hits: u64,
    /// Total cache misses.
    pub misses: u64,
    /// Hit rate as a fraction (0.0-1.0).
    pub hit_rate: f64,
    /// Current number of cached entries.
    pub entry_count: u64,
}

/// Shared handle for querying cache stats and clearing caches.
#[derive(Clone)]
pub struct CacheHandle {
    caches: Arc<Vec<BackendCache>>,
}

impl CacheHandle {
    /// Get a snapshot of cache statistics for all backends.
    pub async fn stats(&self) -> Vec<CacheStatsSnapshot> {
        let mut snapshots = Vec::with_capacity(self.caches.len());
        for bc in self.caches.iter() {
            let hits = bc.stats.hits.load(Ordering::Relaxed);
            let misses = bc.stats.misses.load(Ordering::Relaxed);
            let total = hits + misses;
            let resource_count = match &bc.resource_cache {
                Some(store) => store.entry_count().await,
                None => 0,
            };
            let tool_count = match &bc.tool_cache {
                Some(store) => store.entry_count().await,
                None => 0,
            };
            snapshots.push(CacheStatsSnapshot {
                namespace: bc.namespace.clone(),
                hits,
                misses,
                hit_rate: if total > 0 {
                    hits as f64 / total as f64
                } else {
                    0.0
                },
                entry_count: resource_count + tool_count,
            });
        }
        snapshots
    }

    /// Clear all cache entries and reset stats.
    pub async fn clear(&self) {
        for bc in self.caches.iter() {
            if let Some(store) = &bc.resource_cache {
                store.invalidate_all().await;
            }
            if let Some(store) = &bc.tool_cache {
                store.invalidate_all().await;
            }
            bc.stats.hits.store(0, Ordering::Relaxed);
            bc.stats.misses.store(0, Ordering::Relaxed);
        }
    }
}

/// Build the shared cache state from per-backend configs.
///
/// Returns an `Arc<Vec<BackendCache>>` that can be shared between a
/// [`CacheLayer`] (or [`CacheService`]) and its [`CacheHandle`].
fn build_caches(
    configs: Vec<(String, &BackendCacheConfig)>,
    backend_config: &CacheBackendConfig,
) -> Arc<Vec<BackendCache>> {
    let caches: Vec<BackendCache> = configs
        .into_iter()
        .map(|(namespace, cfg)| {
            let resource_cache = if cfg.resource_ttl_seconds > 0 {
                Some(build_cache_store(
                    backend_config,
                    Duration::from_secs(cfg.resource_ttl_seconds),
                    cfg.max_entries,
                ))
            } else {
                None
            };
            let tool_cache = if cfg.tool_ttl_seconds > 0 {
                Some(build_cache_store(
                    backend_config,
                    Duration::from_secs(cfg.tool_ttl_seconds),
                    cfg.max_entries,
                ))
            } else {
                None
            };
            BackendCache {
                namespace,
                resource_cache,
                tool_cache,
                stats: Arc::new(CacheStats::new()),
            }
        })
        .collect();
    Arc::new(caches)
}

/// Tower [`Layer`] that produces [`CacheService`] instances sharing the same
/// cache state and [`CacheHandle`].
///
/// Because `CacheService::new()` returns a `(CacheService, CacheHandle)` tuple,
/// a standard `Layer` cannot propagate the side-channel handle. `CacheLayer`
/// solves this by creating the shared cache state up-front and handing out an
/// `Arc`-cloned handle to the caller while cloning the same `Arc` into every
/// service produced by [`Layer::layer`].
///
/// # Example
///
/// ```rust
/// use mcp_proxy::cache::{CacheLayer, CacheHandle};
/// use mcp_proxy::config::{BackendCacheConfig, CacheBackendConfig};
///
/// let cfg = BackendCacheConfig {
///     resource_ttl_seconds: 300,
///     tool_ttl_seconds: 60,
///     max_entries: 1000,
/// };
/// let backend_cfg = CacheBackendConfig::default();
///
/// let (layer, handle) = CacheLayer::new(
///     vec![("api/".to_string(), &cfg)],
///     &backend_cfg,
/// );
///
/// // `layer` implements `tower::Layer<S>` and can be used in a middleware stack.
/// // `handle` can be used to query stats or clear the cache.
/// ```
#[derive(Clone)]
pub struct CacheLayer {
    caches: Arc<Vec<BackendCache>>,
}

impl CacheLayer {
    /// Create a new cache layer and return it with a shareable [`CacheHandle`].
    ///
    /// The handle provides [`CacheHandle::stats()`] and [`CacheHandle::clear()`]
    /// over the same underlying cache state used by every service the layer
    /// produces.
    pub fn new(
        configs: Vec<(String, &BackendCacheConfig)>,
        backend_config: &CacheBackendConfig,
    ) -> (Self, CacheHandle) {
        let caches = build_caches(configs, backend_config);
        let handle = CacheHandle {
            caches: Arc::clone(&caches),
        };
        (Self { caches }, handle)
    }
}

impl<S> Layer<S> for CacheLayer {
    type Service = CacheService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CacheService {
            inner,
            caches: Arc::clone(&self.caches),
        }
    }
}

/// Tower service that caches resource reads and tool call results.
#[derive(Clone)]
pub struct CacheService<S> {
    inner: S,
    caches: Arc<Vec<BackendCache>>,
}

impl<S> CacheService<S> {
    /// Create a new cache service and return it with a shareable handle.
    pub fn new(
        inner: S,
        configs: Vec<(String, &BackendCacheConfig)>,
        backend_config: &CacheBackendConfig,
    ) -> (Self, CacheHandle) {
        let caches = build_caches(configs, backend_config);
        let handle = CacheHandle {
            caches: Arc::clone(&caches),
        };
        (Self { inner, caches }, handle)
    }
}

/// Extract cache key and find the matching backend cache + stats.
fn resolve_cache<'a>(
    caches: &'a [BackendCache],
    req: &McpRequest,
) -> Option<(&'a CacheStore, String, &'a Arc<CacheStats>)> {
    match req {
        McpRequest::ReadResource(params) => {
            let key = format!("res:{}", params.uri);
            for bc in caches {
                if params.uri.starts_with(&bc.namespace) {
                    return bc.resource_cache.as_ref().map(|c| (c, key, &bc.stats));
                }
            }
            None
        }
        McpRequest::CallTool(params) => {
            let args = serde_json::to_string(&params.arguments).unwrap_or_default();
            let key = format!("tool:{}:{}", params.name, args);
            for bc in caches {
                if params.name.starts_with(&bc.namespace) {
                    return bc.tool_cache.as_ref().map(|c| (c, key, &bc.stats));
                }
            }
            None
        }
        _ => None,
    }
}

impl<S> Service<RouterRequest> for CacheService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let caches = Arc::clone(&self.caches);

        if let Some((store, key, stats)) = resolve_cache(&caches, &req.inner) {
            let store = store.clone();
            let stats = Arc::clone(stats);
            let mut inner = self.inner.clone();

            return Box::pin(async move {
                // Cache hit -- return with current request ID
                if let Some(cached) = store.get(&key).await {
                    stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(RouterResponse {
                        id: req.id,
                        inner: cached.inner,
                    });
                }

                stats.misses.fetch_add(1, Ordering::Relaxed);
                let result = inner.call(req).await;

                // Only cache successful MCP responses
                let Ok(ref resp) = result;
                if resp.inner.is_ok() {
                    store.insert(key, resp.clone()).await;
                }

                result
            });
        }

        // No caching for this request type or backend
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::CacheService;
    use crate::config::{BackendCacheConfig, CacheBackendConfig};
    use crate::test_util::{MockService, call_service};

    fn tool_call(name: &str) -> McpRequest {
        McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: name.to_string(),
            arguments: serde_json::json!({"key": "value"}),
            meta: None,
            task: None,
        })
    }

    fn default_backend_config() -> CacheBackendConfig {
        CacheBackendConfig::default()
    }

    #[tokio::test]
    async fn test_cache_hit_returns_same_result() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, _handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        let resp1 = call_service(&mut svc, tool_call("fs/read")).await;
        let resp2 = call_service(&mut svc, tool_call("fs/read")).await;

        // Both should succeed with same content
        match (resp1.inner.unwrap(), resp2.inner.unwrap()) {
            (McpResponse::CallTool(r1), McpResponse::CallTool(r2)) => {
                assert_eq!(r1.all_text(), r2.all_text());
            }
            _ => panic!("expected CallTool responses"),
        }
    }

    #[tokio::test]
    async fn test_cache_disabled_passes_through() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 0,
            tool_ttl_seconds: 0,
            max_entries: 100,
        };
        let (mut svc, _handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        let resp = call_service(&mut svc, tool_call("fs/read")).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_cache_non_matching_namespace_passes_through() {
        let mock = MockService::with_tools(&["db/query"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, _handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        let resp = call_service(&mut svc, tool_call("db/query")).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_cache_list_tools_not_cached() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, _handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok(), "list_tools should pass through");
    }

    #[tokio::test]
    async fn test_cache_stats_tracks_hits_and_misses() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        // First call = miss
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats().await;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 1);

        // Second call = hit
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 1);
        assert_eq!(stats[0].misses, 1);
        assert!((stats[0].hit_rate - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_cache_clear_resets_stats() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, handle) = CacheService::new(
            mock,
            vec![("fs/".to_string(), &cfg)],
            &default_backend_config(),
        );

        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let _ = call_service(&mut svc, tool_call("fs/read")).await;

        handle.clear().await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 0);
    }

    #[tokio::test]
    async fn test_cache_layer_produces_working_service() {
        use super::CacheLayer;
        use tower::Layer;

        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (layer, handle) =
            CacheLayer::new(vec![("fs/".to_string(), &cfg)], &default_backend_config());

        let mock = MockService::with_tools(&["fs/read"]);
        let mut svc = layer.layer(mock);

        // First call = miss
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 0);

        // Second call = hit (cached)
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 1);
        assert_eq!(stats[0].misses, 1);
    }

    #[tokio::test]
    async fn test_cache_layer_shares_state_across_services() {
        use super::CacheLayer;
        use tower::Layer;

        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (layer, handle) =
            CacheLayer::new(vec![("fs/".to_string(), &cfg)], &default_backend_config());

        // Create two services from the same layer
        let mock1 = MockService::with_tools(&["fs/read"]);
        let mut svc1 = layer.layer(mock1);

        let mock2 = MockService::with_tools(&["fs/read"]);
        let mut svc2 = layer.layer(mock2);

        // Miss on svc1
        let _ = call_service(&mut svc1, tool_call("fs/read")).await;
        assert_eq!(handle.stats().await[0].misses, 1);

        // Hit on svc2 (same underlying cache)
        let _ = call_service(&mut svc2, tool_call("fs/read")).await;
        assert_eq!(handle.stats().await[0].hits, 1);
        assert_eq!(handle.stats().await[0].misses, 1);
    }

    #[tokio::test]
    async fn test_cache_layer_handle_clear() {
        use super::CacheLayer;
        use tower::Layer;

        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (layer, handle) =
            CacheLayer::new(vec![("fs/".to_string(), &cfg)], &default_backend_config());

        let mock = MockService::with_tools(&["fs/read"]);
        let mut svc = layer.layer(mock);

        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        assert_eq!(handle.stats().await[0].hits, 1);

        handle.clear().await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 0);
    }

    #[tokio::test]
    async fn test_cache_store_memory_get_insert() {
        use super::{CacheStore, build_cache_store};

        let store = build_cache_store(&default_backend_config(), Duration::from_secs(60), 100);
        assert!(matches!(store, CacheStore::Memory(_)));

        // Initially empty
        assert!(store.get("key1").await.is_none());
        assert_eq!(store.entry_count().await, 0);
    }

    #[cfg(feature = "redis-cache")]
    #[test]
    fn test_cache_store_redis_construction() {
        use super::build_cache_store;

        let cfg = CacheBackendConfig {
            backend: "redis".to_string(),
            url: Some("redis://127.0.0.1:6379".to_string()),
            prefix: "test:".to_string(),
        };
        let store = build_cache_store(&cfg, Duration::from_secs(60), 100);
        assert!(matches!(store, super::CacheStore::Redis { .. }));
    }

    #[cfg(feature = "sqlite-cache")]
    #[tokio::test]
    async fn test_cache_store_sqlite_construction() {
        use super::build_cache_store;

        let dir = std::env::temp_dir().join(format!("mcp-proxy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test_cache.db");

        let cfg = CacheBackendConfig {
            backend: "sqlite".to_string(),
            url: Some(db_path.to_string_lossy().to_string()),
            prefix: "test:".to_string(),
        };
        let store = build_cache_store(&cfg, Duration::from_secs(60), 100);
        assert!(matches!(store, super::CacheStore::Sqlite { .. }));
        assert_eq!(store.entry_count().await, 0);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    use std::time::Duration;
}
