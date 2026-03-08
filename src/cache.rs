//! Response caching middleware for the gateway.
//!
//! Caches `ReadResource` and `CallTool` responses with per-backend TTL.
//! Cache keys are derived from the request type, name/URI, and arguments.
//!
//! # Configuration
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
use tower::Service;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

use crate::config::BackendCacheConfig;

/// Per-backend cache with separate resource and tool caches (different TTLs).
#[derive(Clone)]
struct BackendCache {
    namespace: String,
    resource_cache: Option<Cache<String, RouterResponse>>,
    tool_cache: Option<Cache<String, RouterResponse>>,
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
    pub fn stats(&self) -> Vec<CacheStatsSnapshot> {
        self.caches
            .iter()
            .map(|bc| {
                let hits = bc.stats.hits.load(Ordering::Relaxed);
                let misses = bc.stats.misses.load(Ordering::Relaxed);
                let total = hits + misses;
                let entry_count = bc.resource_cache.as_ref().map_or(0, |c| c.entry_count())
                    + bc.tool_cache.as_ref().map_or(0, |c| c.entry_count());
                CacheStatsSnapshot {
                    namespace: bc.namespace.clone(),
                    hits,
                    misses,
                    hit_rate: if total > 0 {
                        hits as f64 / total as f64
                    } else {
                        0.0
                    },
                    entry_count,
                }
            })
            .collect()
    }

    /// Clear all cache entries and reset stats.
    pub fn clear(&self) {
        for bc in self.caches.iter() {
            if let Some(c) = &bc.resource_cache {
                c.invalidate_all();
            }
            if let Some(c) = &bc.tool_cache {
                c.invalidate_all();
            }
            bc.stats.hits.store(0, Ordering::Relaxed);
            bc.stats.misses.store(0, Ordering::Relaxed);
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
    pub fn new(inner: S, configs: Vec<(String, &BackendCacheConfig)>) -> (Self, CacheHandle) {
        let caches: Vec<BackendCache> = configs
            .into_iter()
            .map(|(namespace, cfg)| {
                let resource_cache = if cfg.resource_ttl_seconds > 0 {
                    Some(
                        Cache::builder()
                            .max_capacity(cfg.max_entries)
                            .time_to_live(Duration::from_secs(cfg.resource_ttl_seconds))
                            .build(),
                    )
                } else {
                    None
                };
                let tool_cache = if cfg.tool_ttl_seconds > 0 {
                    Some(
                        Cache::builder()
                            .max_capacity(cfg.max_entries)
                            .time_to_live(Duration::from_secs(cfg.tool_ttl_seconds))
                            .build(),
                    )
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
        let caches = Arc::new(caches);
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
) -> Option<(
    &'a Cache<String, RouterResponse>,
    String,
    &'a Arc<CacheStats>,
)> {
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

        if let Some((cache, key, stats)) = resolve_cache(&caches, &req.inner) {
            let cache = cache.clone();
            let stats = Arc::clone(stats);
            let mut inner = self.inner.clone();

            return Box::pin(async move {
                // Cache hit -- return with current request ID
                if let Some(cached) = cache.get(&key).await {
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
                    cache.insert(key, resp.clone()).await;
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
    use crate::config::BackendCacheConfig;
    use crate::test_util::{MockService, call_service};

    fn tool_call(name: &str) -> McpRequest {
        McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: name.to_string(),
            arguments: serde_json::json!({"key": "value"}),
            meta: None,
            task: None,
        })
    }

    #[tokio::test]
    async fn test_cache_hit_returns_same_result() {
        let mock = MockService::with_tools(&["fs/read"]);
        let cfg = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };
        let (mut svc, _handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

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
        let (mut svc, _handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

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
        let (mut svc, _handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

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
        let (mut svc, _handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

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
        let (mut svc, handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

        // First call = miss
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 1);

        // Second call = hit
        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let stats = handle.stats();
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
        let (mut svc, handle) = CacheService::new(mock, vec![("fs/".to_string(), &cfg)]);

        let _ = call_service(&mut svc, tool_call("fs/read")).await;
        let _ = call_service(&mut svc, tool_call("fs/read")).await;

        handle.clear();
        let stats = handle.stats();
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 0);
    }
}
