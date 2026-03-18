//! HTTP transport-level E2E tests.
//!
//! Unlike `e2e.rs` which tests the MCP pipeline in-process, these tests
//! exercise the full HTTP stack: they start a real TCP server on a random
//! port and make HTTP requests with `reqwest`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::TcpListener;
use tower::util::BoxCloneService;

use tower_mcp::client::ChannelTransport;
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::RouterRequest;
use tower_mcp::router::RouterResponse;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_proxy::admin::{self, BackendMeta};
use mcp_proxy::config::ProxyConfig;

// ---------------------------------------------------------------------------
// Test backend routers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn math_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    McpRouter::new()
        .server_info("math-server", "1.0.0")
        .tool(add)
}

fn text_router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a message")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.message)) })
        .build();

    McpRouter::new()
        .server_info("text-server", "1.0.0")
        .tool(echo)
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build an McpProxy with in-process channel backends, wrap it in the HTTP
/// transport and admin router, and bind to a random port. Returns the
/// server address and a handle to abort the server task.
async fn spawn_proxy_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let proxy = McpProxy::builder("test-proxy", "1.0.0")
        .separator("/")
        .backend("math", ChannelTransport::new(math_router()))
        .await
        .backend("text", ChannelTransport::new(text_router()))
        .await
        .build_strict()
        .await
        .expect("proxy should build");

    let proxy_for_admin = proxy.clone();
    let proxy_for_mgmt = proxy.clone();

    let service: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
        BoxCloneService::new(proxy);

    let (router, session_handle) =
        tower_mcp::transport::http::HttpTransport::from_service(service).into_router_with_handle();

    let backend_meta: HashMap<String, BackendMeta> = [
        (
            "math".to_string(),
            BackendMeta {
                transport: "channel".to_string(),
            },
        ),
        (
            "text".to_string(),
            BackendMeta {
                transport: "channel".to_string(),
            },
        ),
    ]
    .into();

    let admin_state = admin::spawn_health_checker(
        proxy_for_admin,
        "test-proxy".into(),
        "1.0.0".into(),
        2,
        backend_meta,
    );

    let test_config = ProxyConfig::parse(
        r#"
        [proxy]
        name = "test-proxy"
        version = "1.0.0"
        [proxy.listen]

        [[backends]]
        name = "math"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "text"
        transport = "stdio"
        command = "echo"
        "#,
    )
    .unwrap();

    let router = router.nest(
        "/admin",
        admin::admin_router(
            admin_state,
            None,
            session_handle,
            None,
            proxy_for_mgmt,
            &test_config,
            None,
        ),
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to random port");
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    (addr, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The `/admin/backends` endpoint returns proxy metadata and backend list.
#[tokio::test]
async fn test_admin_backends_endpoint() {
    let (addr, handle) = spawn_proxy_server().await;
    let url = format!("http://{}/admin/backends", addr);

    let resp = reqwest::get(&url).await.expect("GET /admin/backends");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["proxy"]["name"], "test-proxy");
    assert_eq!(body["proxy"]["version"], "1.0.0");
    assert_eq!(body["proxy"]["backend_count"], 2);
    assert!(body["backends"].is_array());

    handle.abort();
}

/// The `/admin/health` endpoint returns a health status object.
#[tokio::test]
async fn test_admin_health_endpoint() {
    let (addr, handle) = spawn_proxy_server().await;
    let url = format!("http://{}/admin/health", addr);

    let resp = reqwest::get(&url).await.expect("GET /admin/health");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    // The health checker may not have run yet, but the endpoint should
    // respond with a valid status field.
    assert!(
        body["status"] == "healthy" || body["status"] == "degraded",
        "unexpected status: {}",
        body["status"]
    );
    assert!(body["unhealthy_backends"].is_array());

    handle.abort();
}

/// The `/admin/sessions` endpoint returns the active session count.
#[tokio::test]
async fn test_admin_sessions_endpoint() {
    let (addr, handle) = spawn_proxy_server().await;
    let url = format!("http://{}/admin/sessions", addr);

    let resp = reqwest::get(&url).await.expect("GET /admin/sessions");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    // No MCP sessions established, so count should be 0.
    assert_eq!(body["active_sessions"], 0);

    handle.abort();
}

/// The `/admin/cache/stats` endpoint returns an empty array when no caches
/// are configured.
#[tokio::test]
async fn test_admin_cache_stats_no_cache() {
    let (addr, handle) = spawn_proxy_server().await;
    let url = format!("http://{}/admin/cache/stats", addr);

    let resp = reqwest::get(&url).await.expect("GET /admin/cache/stats");
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.as_array().unwrap().is_empty());

    handle.abort();
}

/// The `/admin/config` endpoint returns the proxy configuration.
#[tokio::test]
async fn test_admin_config_endpoint() {
    let (addr, handle) = spawn_proxy_server().await;
    let url = format!("http://{}/admin/config", addr);

    let resp = reqwest::get(&url).await.expect("GET /admin/config");
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    // The config endpoint returns TOML; it should contain the proxy name.
    assert!(
        body.contains("test-proxy"),
        "config should contain proxy name"
    );

    handle.abort();
}
