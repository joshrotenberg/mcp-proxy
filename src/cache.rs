//! Response caching middleware for the gateway.
//!
//! Caches `ReadResource` and `CallTool` responses with per-backend TTL.
//! Cache keys are derived from the request type, name/URI, and arguments.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use moka::future::Cache;
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
}

/// Tower service that caches resource reads and tool call results.
#[derive(Clone)]
pub struct CacheService<S> {
    inner: S,
    caches: Arc<Vec<BackendCache>>,
}

impl<S> CacheService<S> {
    pub fn new(inner: S, configs: Vec<(String, &BackendCacheConfig)>) -> Self {
        let caches = configs
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
                }
            })
            .collect();
        Self {
            inner,
            caches: Arc::new(caches),
        }
    }
}

/// Extract cache key and find the matching backend cache.
fn resolve_cache<'a>(
    caches: &'a [BackendCache],
    req: &McpRequest,
) -> Option<(&'a Cache<String, RouterResponse>, String)> {
    match req {
        McpRequest::ReadResource(params) => {
            let key = format!("res:{}", params.uri);
            for bc in caches {
                if params.uri.starts_with(&bc.namespace) {
                    return bc.resource_cache.as_ref().map(|c| (c, key));
                }
            }
            None
        }
        McpRequest::CallTool(params) => {
            let args = serde_json::to_string(&params.arguments).unwrap_or_default();
            let key = format!("tool:{}:{}", params.name, args);
            for bc in caches {
                if params.name.starts_with(&bc.namespace) {
                    return bc.tool_cache.as_ref().map(|c| (c, key));
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

        if let Some((cache, key)) = resolve_cache(&caches, &req.inner) {
            let cache = cache.clone();
            let mut inner = self.inner.clone();

            return Box::pin(async move {
                // Cache hit -- return with current request ID
                if let Some(cached) = cache.get(&key).await {
                    return Ok(RouterResponse {
                        id: req.id,
                        inner: cached.inner,
                    });
                }

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
