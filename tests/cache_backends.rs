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
        input_responses: None,
        request_state: None,
        meta: None,
        task: None,
    })
}

async fn call_svc<S>(svc: &mut S, request: McpRequest) -> RouterResponse
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send,
{
    static DIAGNOSTICS: std::sync::Once = std::sync::Once::new();
    DIAGNOSTICS.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("mcp_proxy::cache=debug")
            .with_test_writer()
            .try_init();
    });
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
            generation: Some("test-generation".into()),
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
            generation: Some("test-generation".into()),
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
        let guard = ContainerGuard::new(RedisTemplate::new(&name).port(port))
            .wait_for_ready(true)
            .start()
            .await
            .expect("Redis integration tests require a working Docker daemon and redis:7-alpine");

        let conn_str = guard.connection_string();
        let backend_cfg = CacheBackendConfig {
            backend: "redis".to_string(),
            url: Some(conn_str),
            prefix: "mcp-test:".to_string(),
            generation: Some("test-generation".into()),
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
        let guard = ContainerGuard::new(RedisTemplate::new(&name).port(port))
            .wait_for_ready(true)
            .start()
            .await
            .expect("Redis integration tests require a working Docker daemon and redis:7-alpine");

        let conn_str = guard.connection_string();
        let backend_cfg = CacheBackendConfig {
            backend: "redis".to_string(),
            url: Some(conn_str),
            prefix: "mcp-clear:".to_string(),
            generation: Some("test-generation".into()),
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

#[derive(Clone)]
struct Counting {
    label: &'static str,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Service<RouterRequest> for Counting {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = std::future::Ready<Result<RouterResponse, Infallible>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Infallible>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, req: RouterRequest) -> Self::Future {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let response = if matches!(req.inner, McpRequest::ReadResource(_)) {
            McpResponse::ReadResource(
                serde_json::from_value(
                    serde_json::json!({"contents":[{"uri":"api/asset","text":self.label}]}),
                )
                .unwrap(),
            )
        } else {
            McpResponse::CallTool(tower_mcp::CallToolResult::text(self.label))
        };
        std::future::ready(Ok(RouterResponse {
            id: req.id,
            inner: Ok(response),
        }))
    }
}
fn counted(
    config: &CacheBackendConfig,
    namespace: &str,
    label: &'static str,
    capacity: u64,
    ttl: u64,
) -> (
    CacheService<Counting>,
    mcp_proxy::cache::CacheHandle,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let per_backend = BackendCacheConfig {
        resource_ttl_seconds: ttl,
        tool_ttl_seconds: ttl,
        max_entries: capacity,
    };
    let (service, handle) = CacheService::new(
        Counting {
            label,
            calls: calls.clone(),
        },
        vec![(namespace.into(), &per_backend)],
        config,
    );
    (service, handle, calls)
}
fn executions(calls: &std::sync::atomic::AtomicUsize) -> usize {
    calls.load(std::sync::atomic::Ordering::SeqCst)
}
fn text(response: RouterResponse) -> String {
    let McpResponse::CallTool(result) = response.inner.unwrap() else {
        panic!("tool result required")
    };
    let value = serde_json::to_value(result).unwrap();
    value["content"][0]["text"].as_str().unwrap().into()
}

async fn isolation_restart_and_generation(mut config: CacheBackendConfig) {
    // Deliberately share a generation: prefix isolation must not pass only due to random generations.
    config.generation = Some("shared-v1".into());
    config.prefix = "tenant*:[a]:".into();
    let (mut first, first_handle, first_calls) = counted(&config, "api/", "first", 10, 60);
    assert_eq!(
        text(call_svc(&mut first, tool_call("api/query")).await),
        "first"
    );
    let (mut restarted, restart_handle, restart_calls) =
        counted(&config, "api/", "restart", 10, 60);
    assert_eq!(
        text(call_svc(&mut restarted, tool_call("api/query")).await),
        "first"
    );
    assert_eq!(
        executions(&restart_calls),
        0,
        "explicit generation must preserve restart reuse"
    );
    let mut other = config.clone();
    other.prefix = "tenant*:[a]:nested".into();
    let (mut second, second_handle, second_calls) = counted(&other, "api/", "second", 10, 60);
    assert_eq!(
        text(call_svc(&mut second, tool_call("api/query")).await),
        "second"
    );
    assert_eq!(
        executions(&second_calls),
        1,
        "other prefix reused the first result"
    );
    let (mut different_backend, backend_handle, _) =
        counted(&config, "api/nested/", "nested", 10, 60);
    call_svc(&mut different_backend, tool_call("api/nested/query")).await;
    assert_eq!(first_handle.stats().await[0].entry_count, 1);
    assert_eq!(backend_handle.stats().await[0].entry_count, 1);
    first_handle.clear().await;
    assert_eq!(restart_handle.stats().await[0].entry_count, 0);
    assert_eq!(
        second_handle.stats().await[0].entry_count,
        1,
        "clear crossed prefix scope"
    );
    assert_eq!(
        backend_handle.stats().await[0].entry_count,
        1,
        "clear crossed backend scope"
    );
    call_svc(&mut first, tool_call("api/query")).await;
    assert_eq!(executions(&first_calls), 2);
    config.generation = Some("shared-v2".into());
    let (mut changed, _, calls) = counted(&config, "api/", "changed", 10, 60);
    assert_eq!(
        text(call_svc(&mut changed, tool_call("api/query")).await),
        "changed"
    );
    assert_eq!(executions(&calls), 1);
    // Independent writers must not race beyond the shared persistent budget.
    config.generation = Some("concurrent-budget".into());
    let (mut warm, handle, _) = counted(&config, "api/", "warm", 4, 60);
    call_svc(&mut warm, tool_call("api/warm")).await;
    let mut jobs = Vec::new();
    for index in 0..16 {
        let config = config.clone();
        jobs.push(tokio::spawn(async move {
            let (mut service, _, _) = counted(&config, "api/", "writer", 4, 60);
            call_svc(&mut service, tool_call(&format!("api/{index}"))).await;
        }));
    }
    for job in jobs {
        job.await.unwrap();
    }
    let count = handle.stats().await[0].entry_count;
    assert!(
        (1..=4).contains(&count),
        "concurrent writers exceeded capacity: {count}"
    );
    config.generation = None;
    let (mut fresh, _, calls) = counted(&config, "api/", "fresh", 10, 60);
    let (mut another, _, other_calls) = counted(&config, "api/", "another", 10, 60);
    assert_eq!(
        text(call_svc(&mut fresh, tool_call("api/query")).await),
        "fresh"
    );
    assert_eq!(
        text(call_svc(&mut another, tool_call("api/query")).await),
        "another"
    );
    assert_eq!((executions(&calls), executions(&other_calls)), (1, 1));
}
async fn capacity_and_expiry(mut config: CacheBackendConfig) {
    config.generation = Some("capacity-v1".into());
    let (mut service, handle, calls) = counted(&config, "api/", "value", 1, 1);
    call_svc(&mut service, tool_call("api/one")).await;
    call_svc(&mut service, tool_call("api/two")).await;
    assert_eq!(handle.stats().await[0].entry_count, 1);
    // Tool and resource entries must share the same budget.
    call_svc(
        &mut service,
        McpRequest::ReadResource(
            serde_json::from_value(serde_json::json!({"uri":"api/asset"})).unwrap(),
        ),
    )
    .await;
    assert_eq!(handle.stats().await[0].entry_count, 1);
    call_svc(&mut service, tool_call("api/two")).await;
    assert_eq!(
        executions(&calls),
        4,
        "resource insertion failed to evict the tool entry"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(handle.stats().await[0].entry_count, 0);
    call_svc(&mut service, tool_call("api/two")).await;
    assert_eq!(executions(&calls), 5);
}

#[cfg(feature = "sqlite-cache")]
#[tokio::test]
async fn sqlite_scopes_clear_restart_generation_capacity_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let config = CacheBackendConfig {
        backend: "sqlite".into(),
        url: Some(dir.path().join("cache.db").to_str().unwrap().into()),
        prefix: "test".into(),
        generation: None,
    };
    isolation_restart_and_generation(config.clone()).await;
    capacity_and_expiry(config.clone()).await;
    let conn = rusqlite::Connection::open(config.url.as_ref().unwrap()).unwrap();
    let expired: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mcp_cache_v2 WHERE expires_at <= ?1",
            [std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(expired, 0, "expired rows were not physically pruned");
}
#[cfg(feature = "sqlite-cache")]
#[tokio::test]
async fn sqlite_busy_and_invalid_paths_do_not_stall_the_executor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.db");
    let config = CacheBackendConfig {
        backend: "sqlite".into(),
        url: Some(path.to_str().unwrap().into()),
        prefix: "busy".into(),
        generation: Some("v1".into()),
    };
    let (mut service, _, calls) = counted(&config, "api/", "value", 10, 60);
    call_svc(&mut service, tool_call("api/one")).await;
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let started = std::time::Instant::now();
    let (response, ()) = tokio::join!(call_svc(&mut service, tool_call("api/one")), async {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "SQLite blocked Tokio's worker"
        );
    });
    assert_eq!(text(response), "value");
    assert_eq!(executions(&calls), 2, "unavailable cache must fail open");
    lock.execute_batch("ROLLBACK").unwrap();
    let invalid = CacheBackendConfig {
        url: Some(dir.path().to_str().unwrap().into()),
        ..config
    };
    let (mut service, _, calls) = counted(&invalid, "api/", "uncached", 10, 60);
    call_svc(&mut service, tool_call("api/one")).await;
    call_svc(&mut service, tool_call("api/one")).await;
    assert_eq!(
        executions(&calls),
        2,
        "bad storage must not panic or claim a hit"
    );
}
#[cfg(feature = "redis-cache")]
#[tokio::test]
async fn redis_scopes_clear_restart_generation_capacity_and_expiry() {
    use docker_wrapper::{RedisTemplate, testing::ContainerGuard};
    let guard = ContainerGuard::new(
        RedisTemplate::new(format!("mcp-cache-contract-{}", uuid::Uuid::new_v4())).port(18300),
    )
    .wait_for_ready(true)
    .start()
    .await
    .expect("real Redis required");
    let config = CacheBackendConfig {
        backend: "redis".into(),
        url: Some(guard.connection_string()),
        prefix: "test".into(),
        generation: None,
    };
    isolation_restart_and_generation(config.clone()).await;
    capacity_and_expiry(config).await;
}
#[cfg(feature = "redis-cache")]
#[tokio::test]
async fn silent_redis_has_deadlines_and_does_not_block_backend_work() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = CacheBackendConfig {
        backend: "redis".into(),
        url: Some(format!("redis://{}", listener.local_addr().unwrap())),
        prefix: "test".into(),
        generation: None,
    };
    let peer = tokio::spawn(async move {
        let mut sockets = Vec::new();
        loop {
            sockets.push(listener.accept().await.unwrap().0);
        }
    });
    let (mut service, handle, calls) = counted(&config, "api/", "uncached", 1, 60);
    let started = std::time::Instant::now();
    for _ in 0..2 {
        assert_eq!(
            text(call_svc(&mut service, tool_call("api/one")).await),
            "uncached"
        );
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(executions(&calls), 2);
    assert_eq!(handle.stats().await[0].entry_count, 0);
    peer.abort();
    let _ = peer.await;
}
