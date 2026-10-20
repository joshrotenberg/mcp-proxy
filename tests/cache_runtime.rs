//! Persistent cache configuration is exercised through initialized application sessions.
#![cfg(any(feature = "sqlite-cache", feature = "redis-cache"))]
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn configured_contract(kind: &str, store: &str) {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let backend = tower_mcp::McpRouter::new().tool(
        tower_mcp::ToolBuilder::new("work")
            .handler(move |_: serde_json::Value| {
                let number = captured.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Ok(tower_mcp::CallToolResult::text(format!("result-{number}"))) }
            })
            .build(),
    );
    let mut backend =
        http::Server::start(tower_mcp::HttpTransport::new(backend).into_router()).await;
    for (generation, expected) in [
        ("", 1),
        ("", 2),
        ("generation='release-a'", 3),
        ("generation='release-a'", 3),
        ("generation='release-b'", 4),
    ] {
        let config = format!(
            r#"
[proxy]
name='cache-runtime'
[proxy.listen]
[auth]
type='bearer'
tokens=['data']
[cache]
backend='{kind}'
url={store:?}
prefix='runtime[*]'
{generation}
[[backends]]
name='api'
transport='http'
url='{}'
[backends.cache]
tool_ttl_seconds=60
max_entries=1
"#,
            backend.url
        );
        let mut app = app::App::embedded(&config).await;
        let session = http::Session::start(&app.url, "data").await;
        let listed = session.rpc("tools/list", json!({})).await;
        assert_eq!(http::success(&listed)["tools"][0]["name"], "api/work");
        for _ in 0..2 {
            let reply = session.call("api/work", json!({})).await;
            assert_eq!(
                http::success(&reply)["content"][0]["text"],
                format!("result-{expected}")
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        session.terminate().await;
        app.shutdown().await;
    }
    backend.stop().await;
}
#[cfg(feature = "sqlite-cache")]
#[tokio::test]
async fn configured_sqlite_generation_and_restart_contract() {
    let dir = tempfile::tempdir().unwrap();
    configured_contract("sqlite", dir.path().join("cache.db").to_str().unwrap()).await;
}
#[cfg(feature = "redis-cache")]
#[tokio::test]
async fn configured_redis_generation_and_restart_contract() {
    use docker_wrapper::{RedisTemplate, testing::ContainerGuard};
    let guard = ContainerGuard::new(
        RedisTemplate::new(format!("mcp-cache-runtime-{}", uuid::Uuid::new_v4())).port(18301),
    )
    .wait_for_ready(true)
    .start()
    .await
    .expect("real Redis required");
    configured_contract("redis", &guard.connection_string()).await;
}
