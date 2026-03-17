//! Integration tests for external cache backends (Redis, SQLite).
//!
//! Redis tests require Docker. SQLite tests use a temp file.
//! Both require tower-mcp 0.8.8+ for RouterResponse serialization.
//!
//! These tests only compile when the corresponding feature is enabled:
//! `cargo test --test cache_backends --features sqlite-cache,redis-cache`

#![cfg(any(feature = "redis-cache", feature = "sqlite-cache"))]

use std::convert::Infallible;

use tower::Service;
use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};

use mcp_proxy::cache::CacheService;
use mcp_proxy::config::{BackendCacheConfig, CacheBackendConfig};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct MockService;

impl Service<RouterRequest> for MockService {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RouterResponse, Infallible>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let id = req.id.clone();
        Box::pin(async move {
            let inner = match req.inner {
                McpRequest::CallTool(params) => Ok(McpResponse::CallTool(
                    tower_mcp::CallToolResult::text(format!("called: {}", params.name)),
                )),
                _ => Ok(McpResponse::Pong(Default::default())),
            };
            Ok(RouterResponse { id, inner })
        })
    }
}

fn tool_call(name: &str) -> McpRequest {
    McpRequest::CallTool(CallToolParams {
        name: name.to_string(),
        arguments: serde_json::json!({"key": "value"}),
        meta: None,
        task: None,
    })
}

async fn call_svc<S>(svc: &mut S, request: McpRequest) -> RouterResponse
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send,
{
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: request,
        extensions: Extensions::new(),
    };
    svc.call(req).await.expect("infallible")
}

// ---------------------------------------------------------------------------
// SQLite cache tests
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlite-cache")]
mod sqlite {
    use super::*;

    #[tokio::test]
    async fn test_sqlite_cache_hit_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test-cache.db");

        let backend_cfg = CacheBackendConfig {
            backend: "sqlite".to_string(),
            url: Some(db_path.to_str().unwrap().to_string()),
            prefix: "test:".to_string(),
        };

        let per_backend = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };

        let (mut svc, handle) = CacheService::new(
            MockService,
            vec![("api/".to_string(), &per_backend)],
            &backend_cfg,
        );

        // First call = miss
        let resp = call_svc(&mut svc, tool_call("api/query")).await;
        assert!(resp.inner.is_ok());
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 0);

        // Second call = hit
        let resp = call_svc(&mut svc, tool_call("api/query")).await;
        assert!(resp.inner.is_ok());
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 1);
    }

    #[tokio::test]
    async fn test_sqlite_cache_clear() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test-cache-clear.db");

        let backend_cfg = CacheBackendConfig {
            backend: "sqlite".to_string(),
            url: Some(db_path.to_str().unwrap().to_string()),
            prefix: "test:".to_string(),
        };

        let per_backend = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };

        let (mut svc, handle) = CacheService::new(
            MockService,
            vec![("api/".to_string(), &per_backend)],
            &backend_cfg,
        );

        let _ = call_svc(&mut svc, tool_call("api/query")).await;
        let _ = call_svc(&mut svc, tool_call("api/query")).await;

        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 1);

        handle.clear().await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 0);
        assert_eq!(stats[0].misses, 0);

        // After clear, next call should miss
        let _ = call_svc(&mut svc, tool_call("api/query")).await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 0);
    }
}

// ---------------------------------------------------------------------------
// Redis cache tests (require Docker)
// ---------------------------------------------------------------------------

#[cfg(feature = "redis-cache")]
mod redis_tests {
    use super::*;
    use docker_wrapper::RedisTemplate;
    use docker_wrapper::testing::ContainerGuard;
    use std::sync::atomic::{AtomicU16, Ordering};

    static PORT_COUNTER: AtomicU16 = AtomicU16::new(18200);

    fn next_port() -> u16 {
        PORT_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    fn unique_name(prefix: &str) -> String {
        format!("{}-{}", prefix, uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn test_redis_cache_hit_miss() {
        let port = next_port();
        let name = unique_name("mcp-cache-test");
        let guard = match ContainerGuard::new(RedisTemplate::new(&name).port(port))
            .start()
            .await
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping Redis test (Docker not available): {e}");
                return;
            }
        };

        // Wait for Redis to be ready
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let conn_str = guard.connection_string();
        let backend_cfg = CacheBackendConfig {
            backend: "redis".to_string(),
            url: Some(conn_str),
            prefix: "mcp-test:".to_string(),
        };

        let per_backend = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };

        let (mut svc, handle) = CacheService::new(
            MockService,
            vec![("api/".to_string(), &per_backend)],
            &backend_cfg,
        );

        // First call = miss
        let resp = call_svc(&mut svc, tool_call("api/query")).await;
        assert!(resp.inner.is_ok());
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 0);

        // Second call = hit
        let resp = call_svc(&mut svc, tool_call("api/query")).await;
        assert!(resp.inner.is_ok());
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);
        assert_eq!(stats[0].hits, 1);

        drop(guard);
    }

    #[tokio::test]
    async fn test_redis_cache_clear() {
        let port = next_port();
        let name = unique_name("mcp-cache-clear");
        let guard = match ContainerGuard::new(RedisTemplate::new(&name).port(port))
            .start()
            .await
        {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping Redis test (Docker not available): {e}");
                return;
            }
        };

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let conn_str = guard.connection_string();
        let backend_cfg = CacheBackendConfig {
            backend: "redis".to_string(),
            url: Some(conn_str),
            prefix: "mcp-clear:".to_string(),
        };

        let per_backend = BackendCacheConfig {
            resource_ttl_seconds: 60,
            tool_ttl_seconds: 60,
            max_entries: 100,
        };

        let (mut svc, handle) = CacheService::new(
            MockService,
            vec![("api/".to_string(), &per_backend)],
            &backend_cfg,
        );

        let _ = call_svc(&mut svc, tool_call("api/query")).await;
        let _ = call_svc(&mut svc, tool_call("api/query")).await;

        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 1);

        handle.clear().await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].hits, 0);

        // After clear, next call should miss
        let _ = call_svc(&mut svc, tool_call("api/query")).await;
        let stats = handle.stats().await;
        assert_eq!(stats[0].misses, 1);

        drop(guard);
    }
}
