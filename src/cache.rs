//! Response caching middleware for the proxy.
//!
//! Caches `ReadResource` and `CallTool` responses with per-backend TTL.
//! Cache keys are derived from the request type, name/URI, arguments, and MCP
//! continuation state.
//!
//! # Cache Backends
//!
//! The cache backend is configurable via `[cache]` in the proxy config:
//!
//! - `"memory"` (default): In-process moka cache. Fast, no external deps.
//! - `"redis"`: External Redis cache; explicit generations enable sharing. Requires the
//!   `redis-cache` feature flag.
//! - `"sqlite"`: Local SQLite cache; explicit generations enable restart reuse. Requires the
//!   `sqlite-cache` feature flag.
//!
//! # Isolation, lifetime and migration
//!
//! Every cache service starts with a fresh generation unless `[cache].generation`
//! explicitly opts into persistent reuse. External scopes hash the exact prefix,
//! generation and backend namespace; tool/resource entries use separate keys and
//! share `max_entries`. Clear/count affect only the exact scope, never a prefix
//! wildcard. Request identities are hashed before storage. Hashes do not add
//! caller authorization: caller-aware caching remains a separate concern.
//!
//! Set a non-secret generation only when all sharing instances have equivalent
//! backend configuration, response and authorization semantics. Bump it when any
//! of those change. Default generations isolate restarts and replacement owners;
//! cached work is never migrated between generations automatically. Low-level
//! Rust `CacheBackendConfig` literals need `generation: None` (or default update
//! syntax). Cloned services/layers continue sharing their original cache state.
//!
//! SQLite uses `mcp_cache_v2`, indexed expiry cleanup on every operation, and
//! transactional oldest-write eviction across resource/tool entries. WAL/NORMAL
//! journaling and one schema transaction avoid per-statement full fsync costs;
//! cache writes may be lost on an OS crash, which is safe for cache data. The old
//! `cache_entries` table is ignored; retire it manually after stopping old
//! deployments. Redis uses exact hashes and sorted indexes with atomic expiry,
//! capacity and clearing scripts, plus TTLs on all index/data keys. Legacy Redis
//! keys are ignored and expire under their original TTL; avoid broad deletion.
//! Existing v2 scopes are pruned on access; inactive Redis scopes expire without
//! a reader, and later SQLite operations reclaim expired inactive generations.
//!
//! SQLite opens/queries on blocking workers: one outstanding job per kind/store,
//! including cancelled waiters, a 25 ms busy timeout and 200 ms async wait budget
//! for initialized stores. Cold connection/schema/WAL setup has a separate bounded
//! two-second budget for filesystem syncs; it never runs on Tokio workers.
//! Redis reuses one multiplexed connection per kind/store, at most 16 admitted
//! operations, 75 ms connection/response deadlines and a 200 ms overall budget.
//! Busy, unavailable or invalid stores fail open as misses/bypassed writes; stats
//! report zero when unavailable. SQLite OS filesystem calls are not cancellable;
//! their admission permit remains held until the worker actually finishes.
//!
//! Persistent eviction is oldest-write, memory uses moka admission/eviction;
//! reads do not renew TTLs. `max_entries` bounds live entries per backend scope,
//! across both response kinds. It does not bound response bytes, the number of
//! explicitly distinct scopes, or the SQLite file's high-water allocation. Use
//! Redis memory limits/filesystem quotas for hard byte budgets. SQLite reuses
//! freed pages and does not automatically VACUUM. In-flight backend calls can
//! refill a cleared cache. Only successful completed responses are cached.
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
use tower_mcp_types::protocol::{InputResponses, McpRequest};

use crate::config::{BackendCacheConfig, CacheBackendConfig};

#[cfg(feature = "redis-cache")]
mod redis;
#[cfg(feature = "sqlite-cache")]
mod sqlite;

/// Pluggable, scoped storage. External I/O fails open as cache misses.
#[derive(Clone)]
pub(crate) enum CacheStore {
    Memory(Cache<String, RouterResponse>),
    #[cfg(feature = "redis-cache")]
    Redis(redis::Store),
    #[cfg(feature = "sqlite-cache")]
    Sqlite(sqlite::Store),
}
impl CacheStore {
    async fn get(&self, key: &str) -> Option<RouterResponse> {
        match self {
            Self::Memory(cache) => cache.get(key).await,
            #[cfg(feature = "redis-cache")]
            Self::Redis(store) => serde_json::from_str(&store.get(&digest(key)).await?).ok(),
            #[cfg(feature = "sqlite-cache")]
            Self::Sqlite(store) => serde_json::from_str(&store.get(&digest(key)).await?).ok(),
        }
    }
    async fn insert(&self, key: String, value: RouterResponse) {
        match self {
            Self::Memory(cache) => {
                cache.insert(key, value).await;
            }
            #[cfg(feature = "redis-cache")]
            Self::Redis(store) => {
                if let Ok(value) = serde_json::to_string(&value) {
                    store.insert(digest(&key), value).await;
                }
            }
            #[cfg(feature = "sqlite-cache")]
            Self::Sqlite(store) => {
                if let Ok(value) = serde_json::to_string(&value) {
                    store.insert(digest(&key), value).await;
                }
            }
        }
    }
    async fn invalidate_all(&self) {
        match self {
            Self::Memory(cache) => cache.invalidate_all(),
            #[cfg(feature = "redis-cache")]
            Self::Redis(store) => store.clear().await,
            #[cfg(feature = "sqlite-cache")]
            Self::Sqlite(store) => store.clear().await,
        }
    }
    async fn entry_count(&self) -> u64 {
        match self {
            Self::Memory(cache) => {
                cache.run_pending_tasks().await;
                cache.entry_count()
            }
            #[cfg(feature = "redis-cache")]
            Self::Redis(store) => store.count().await,
            #[cfg(feature = "sqlite-cache")]
            Self::Sqlite(store) => store.count().await,
        }
    }
}
#[cfg(any(feature = "sqlite-cache", feature = "redis-cache"))]
fn digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn build_cache_store(
    config: &CacheBackendConfig,
    ttl: Duration,
    max_entries: u64,
    scope: &str,
    kind: &'static str,
) -> CacheStore {
    let _ = (scope, kind); // Builds without external cache features use memory only.
    match config.backend.as_str() {
        #[cfg(feature = "redis-cache")]
        "redis" => CacheStore::Redis(redis::Store::new(
            config.url.as_deref().unwrap_or("redis://127.0.0.1"),
            scope.into(),
            kind,
            ttl,
            max_entries,
        )),
        #[cfg(feature = "sqlite-cache")]
        "sqlite" => CacheStore::Sqlite(sqlite::Store::new(
            config.url.clone().unwrap_or_else(|| "cache.db".into()),
            scope.into(),
            kind,
            ttl,
            max_entries,
        )),
        _ => CacheStore::Memory(
            Cache::builder()
                .max_capacity(max_entries)
                .time_to_live(ttl)
                .build(),
        ),
    }
}
struct KindExpiry {
    resource: Duration,
    tool: Duration,
}
impl moka::Expiry<String, RouterResponse> for KindExpiry {
    fn expire_after_create(
        &self,
        key: &String,
        _: &RouterResponse,
        _: std::time::Instant,
    ) -> Option<Duration> {
        Some(if key.starts_with("res:") {
            self.resource
        } else {
            self.tool
        })
    }
    fn expire_after_update(
        &self,
        key: &String,
        value: &RouterResponse,
        updated_at: std::time::Instant,
        _: Option<Duration>,
    ) -> Option<Duration> {
        self.expire_after_create(key, value, updated_at)
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
            let tool_count = match (&bc.tool_cache, &bc.resource_cache) {
                (Some(CacheStore::Memory(_)), Some(CacheStore::Memory(_))) => 0,
                (Some(store), _) => store.entry_count().await,
                (None, _) => 0,
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
    let generation = backend_config
        .generation
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let caches: Vec<BackendCache> = configs
        .into_iter()
        .map(|(namespace, cfg)| {
            #[cfg(any(feature = "sqlite-cache", feature = "redis-cache"))]
            let scope = digest(
                &serde_json::to_string(&(&backend_config.prefix, &generation, &namespace))
                    .expect("cache scope"),
            );
            #[cfg(not(any(feature = "sqlite-cache", feature = "redis-cache")))]
            let scope = {
                let _ = &generation;
                String::new()
            };
            let memory = Cache::builder()
                .max_capacity(cfg.max_entries)
                .expire_after(KindExpiry {
                    resource: Duration::from_secs(cfg.resource_ttl_seconds),
                    tool: Duration::from_secs(cfg.tool_ttl_seconds),
                })
                .build();
            let resource_cache = if cfg.resource_ttl_seconds > 0 {
                Some(build_cache_store(
                    backend_config,
                    Duration::from_secs(cfg.resource_ttl_seconds),
                    cfg.max_entries,
                    &scope,
                    "resource",
                ))
            } else {
                None
            };
            let tool_cache = if cfg.tool_ttl_seconds > 0 {
                Some(build_cache_store(
                    backend_config,
                    Duration::from_secs(cfg.tool_ttl_seconds),
                    cfg.max_entries,
                    &scope,
                    "tool",
                ))
            } else {
                None
            };
            BackendCache {
                namespace,
                resource_cache: resource_cache.map(|store| {
                    if matches!(&store, CacheStore::Memory(_)) {
                        CacheStore::Memory(memory.clone())
                    } else {
                        store
                    }
                }),
                tool_cache: tool_cache.map(|store| {
                    if matches!(&store, CacheStore::Memory(_)) {
                        CacheStore::Memory(memory.clone())
                    } else {
                        store
                    }
                }),
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
fn continuation_identity(
    input_responses: &Option<InputResponses>,
    request_state: &Option<String>,
) -> String {
    serde_json::to_string(&(input_responses, request_state)).unwrap_or_default()
}

fn resolve_cache<'a>(
    caches: &'a [BackendCache],
    req: &McpRequest,
) -> Option<(&'a CacheStore, String, &'a Arc<CacheStats>)> {
    match req {
        McpRequest::ReadResource(params) => {
            let continuation =
                continuation_identity(&params.input_responses, &params.request_state);
            let key = format!("res:{}:{continuation}", params.uri);
            for bc in caches {
                if params.uri.starts_with(&bc.namespace) {
                    return bc.resource_cache.as_ref().map(|c| (c, key, &bc.stats));
                }
            }
            None
        }
        McpRequest::CallTool(params) => {
            let args = serde_json::to_string(&params.arguments).unwrap_or_default();
            let continuation =
                continuation_identity(&params.input_responses, &params.request_state);
            let key = format!("tool:{}:{args}:{continuation}", params.name);
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
                if let Some(cached) = store.get(&key).await
                    && crate::outcome::cacheable(&cached)
                {
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
                if crate::outcome::cacheable(resp) {
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

    fn tool_call_with_state(name: &str, request_state: Option<&str>) -> McpRequest {
        McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: name.to_string(),
            arguments: serde_json::json!({"key": "value"}),
            input_responses: None,
            request_state: request_state.map(str::to_owned),
            meta: None,
            task: None,
        })
    }

    fn tool_call(name: &str) -> McpRequest {
        tool_call_with_state(name, None)
    }

    fn default_backend_config() -> CacheBackendConfig {
        CacheBackendConfig::default()
    }

    #[tokio::test]
    async fn legacy_cached_tool_failure_is_ignored_and_recovered_result_is_cached() {
        use crate::outcome::tests::{request, service};
        use tower::{Service, ServiceExt};
        use tower_mcp::protocol::{CallToolResult, RequestId};
        let (base, calls) = service(Ok(McpResponse::CallTool(CallToolResult::text("recovered"))));
        let config = BackendCacheConfig {
            tool_ttl_seconds: 2,
            resource_ttl_seconds: 60,
            max_entries: 10,
        };
        let (mut cache, handle) = CacheService::new(
            base,
            vec![("api/".into(), &config)],
            &CacheBackendConfig::default(),
        );
        let (store, key, _) = super::resolve_cache(&cache.caches, &request(0).inner).unwrap();
        store
            .insert(
                key,
                tower_mcp::RouterResponse {
                    id: RequestId::Number(0),
                    inner: Ok(McpResponse::CallTool(CallToolResult::error(
                        "legacy failure",
                    ))),
                },
            )
            .await;
        // Replacing a still-live failed entry must reset TTL, not retain its deadline.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        for id in [1, 2] {
            if id == 2 {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            let response = cache
                .ready()
                .await
                .unwrap()
                .call(request(id))
                .await
                .unwrap();
            assert_eq!(response.id, RequestId::Number(id));
            let McpResponse::CallTool(result) = response.inner.unwrap() else {
                panic!("expected tool result")
            };
            assert!(!result.is_error);
            assert_eq!(result.all_text(), "recovered");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 1);
        assert_eq!(stats[0].misses, 1);
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
    async fn test_cache_separates_continuation_state() {
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

        let _ = call_service(&mut svc, tool_call_with_state("fs/read", Some("first"))).await;
        let _ = call_service(&mut svc, tool_call_with_state("fs/read", Some("second"))).await;
        let _ = call_service(&mut svc, tool_call_with_state("fs/read", Some("first"))).await;

        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 2);
        assert_eq!(stats[0].hits, 1);
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
    async fn memory_tool_and_resource_entries_share_capacity_but_keep_distinct_ttls() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let calls = executions.clone();
        let backend = tower::service_fn(move |req: tower_mcp::RouterRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            let value = if matches!(req.inner, McpRequest::ReadResource(_)) {
                McpResponse::ReadResource(
                    serde_json::from_value(
                        serde_json::json!({"contents":[{"uri":"api/item","text":"resource"}]}),
                    )
                    .unwrap(),
                )
            } else {
                McpResponse::CallTool(tower_mcp::CallToolResult::text("tool"))
            };
            std::future::ready(Ok::<_, std::convert::Infallible>(
                tower_mcp::RouterResponse {
                    id: req.id,
                    inner: Ok(value),
                },
            ))
        });
        let config = BackendCacheConfig {
            tool_ttl_seconds: 60,
            resource_ttl_seconds: 1,
            max_entries: 1,
        };
        let (mut service, handle) = CacheService::new(
            backend.clone(),
            vec![("api/".into(), &config)],
            &default_backend_config(),
        );
        let resource = || {
            McpRequest::ReadResource(
                serde_json::from_value(serde_json::json!({"uri":"api/item"})).unwrap(),
            )
        };
        call_service(&mut service, tool_call("api/query")).await;
        call_service(&mut service, resource()).await;
        assert!(
            handle.stats().await[0].entry_count <= 1,
            "tool/resource stores exceeded their combined budget"
        );
        handle.clear().await;
        call_service(&mut service, resource()).await;
        let before = executions.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        call_service(&mut service, resource()).await;
        assert_eq!(
            executions.load(Ordering::SeqCst),
            before + 1,
            "resource TTL was not applied"
        );
        handle.clear().await;
        call_service(&mut service, tool_call("api/query")).await;
        let before = executions.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        call_service(&mut service, tool_call("api/query")).await;
        assert_eq!(
            executions.load(Ordering::SeqCst),
            before,
            "resource TTL expired a tool entry"
        );
        // Moka may evict/reject entries during maintenance; residency is not guaranteed.
        assert!(
            handle.stats().await[0].entry_count <= 1,
            "shared memory cache was double counted"
        );
        // A fresh store avoids admission/eviction effects when checking TTL renewal.
        let config = BackendCacheConfig {
            max_entries: 100,
            ..config
        };
        let (mut renewed, _) = CacheService::new(
            backend,
            vec![("api/".into(), &config)],
            &default_backend_config(),
        );
        call_service(&mut renewed, resource()).await;
        let before = executions.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        call_service(&mut renewed, resource()).await;
        call_service(&mut renewed, resource()).await;
        assert_eq!(
            executions.load(Ordering::SeqCst),
            before + 1,
            "refilled resource did not receive its full new TTL"
        );
    }

    #[tokio::test]
    async fn test_cache_store_memory_get_insert() {
        use super::{CacheStore, build_cache_store};

        let store = build_cache_store(
            &default_backend_config(),
            Duration::from_secs(60),
            100,
            "construction-test",
            "tool",
        );
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
            generation: None,
        };
        let store = build_cache_store(
            &cfg,
            Duration::from_secs(60),
            100,
            "construction-test",
            "tool",
        );
        assert!(matches!(store, super::CacheStore::Redis(_)));
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
            generation: None,
        };
        let store = build_cache_store(
            &cfg,
            Duration::from_secs(60),
            100,
            "construction-test",
            "tool",
        );
        assert!(matches!(store, super::CacheStore::Sqlite(_)));
        assert_eq!(store.entry_count().await, 0);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    use std::time::Duration;
}
