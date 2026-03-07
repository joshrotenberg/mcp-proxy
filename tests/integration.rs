//! Integration tests for the gateway middleware stack.
//!
//! Uses in-process MCP backends via `ChannelTransport` to test the full
//! middleware composition without external processes.

use std::convert::Infallible;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::Service;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_gateway::alias::{AliasMap, AliasService};
use mcp_gateway::cache::CacheService;
use mcp_gateway::config::BackendCacheConfig;
use mcp_gateway::config::{BackendFilter, NameFilter};
use mcp_gateway::filter::CapabilityFilterService;
use mcp_gateway::validation::{ValidationConfig, ValidationService};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
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

    let upper = ToolBuilder::new("upper")
        .description("Uppercase a message")
        .handler(|input: EchoInput| async move {
            Ok(CallToolResult::text(input.message.to_uppercase()))
        })
        .build();

    McpRouter::new()
        .server_info("text-server", "1.0.0")
        .tool(echo)
        .tool(upper)
}

async fn build_proxy() -> McpProxy {
    let math_transport = ChannelTransport::new(math_router());
    let text_transport = ChannelTransport::new(text_router());

    McpProxy::builder("test-proxy", "1.0.0")
        .separator("/")
        .backend("math", math_transport)
        .await
        .backend("text", text_transport)
        .await
        .build_strict()
        .await
        .expect("proxy should build")
}

async fn call<S>(svc: &mut S, request: McpRequest) -> RouterResponse
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
{
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: request,
        extensions: Extensions::new(),
    };
    svc.call(req).await.expect("infallible")
}

fn tool_call(name: &str, args: serde_json::Value) -> McpRequest {
    McpRequest::CallTool(CallToolParams {
        name: name.to_string(),
        arguments: args,
        meta: None,
        task: None,
    })
}

// --- End-to-end proxy tests ---

#[tokio::test]
async fn test_proxy_list_tools_namespaced() {
    let mut proxy = build_proxy().await;

    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"math/add"), "tools: {:?}", names);
            assert!(names.contains(&"text/echo"), "tools: {:?}", names);
            assert!(names.contains(&"text/upper"), "tools: {:?}", names);
            assert_eq!(names.len(), 3);
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_proxy_call_tool_through_namespace() {
    let mut proxy = build_proxy().await;

    let resp = call(
        &mut proxy,
        tool_call("math/add", serde_json::json!({"a": 3, "b": 4})),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "7");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// --- Proxy + filter ---

#[tokio::test]
async fn test_proxy_with_filter_hides_tools() {
    let proxy = build_proxy().await;
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
    }];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"math/add"));
            assert!(names.contains(&"text/echo"));
            assert!(!names.contains(&"text/upper"), "upper should be hidden");
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_proxy_with_filter_denies_call() {
    let proxy = build_proxy().await;
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::AllowList(["echo".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
    }];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hi"})),
    )
    .await;

    assert!(resp.inner.is_err(), "calling hidden tool should fail");
}

// --- Proxy + alias ---

#[tokio::test]
async fn test_proxy_with_alias_renames_tools() {
    let proxy = build_proxy().await;
    let aliases = AliasMap::new(vec![(
        "math/".to_string(),
        "add".to_string(),
        "sum".to_string(),
    )])
    .unwrap();
    let mut svc = AliasService::new(proxy, aliases);

    // List should show aliased name
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(
                names.contains(&"math/sum"),
                "should be aliased: {:?}",
                names
            );
            assert!(
                !names.contains(&"math/add"),
                "original should be gone: {:?}",
                names
            );
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }

    // Call aliased name should work
    let resp = call(
        &mut svc,
        tool_call("math/sum", serde_json::json!({"a": 10, "b": 20})),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => assert_eq!(result.all_text(), "30"),
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// --- Proxy + validation ---

#[tokio::test]
async fn test_proxy_with_validation_rejects_oversized() {
    let proxy = build_proxy().await;
    let config = ValidationConfig {
        max_argument_size: Some(10),
    };
    let mut svc = ValidationService::new(proxy, config);

    let resp = call(
        &mut svc,
        tool_call(
            "text/echo",
            serde_json::json!({"message": "this is a very long message that exceeds the size limit"}),
        ),
    )
    .await;

    let err = resp.inner.unwrap_err();
    assert!(err.message.contains("exceed maximum size"));
}

// --- Proxy + cache ---

#[tokio::test]
async fn test_proxy_with_cache_returns_cached_result() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, _handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    let req = tool_call("math/add", serde_json::json!({"a": 5, "b": 5}));

    let resp1 = call(&mut svc, req.clone()).await;
    let resp2 = call(&mut svc, req).await;

    match (resp1.inner.unwrap(), resp2.inner.unwrap()) {
        (McpResponse::CallTool(r1), McpResponse::CallTool(r2)) => {
            assert_eq!(r1.all_text(), "10");
            assert_eq!(r2.all_text(), "10");
        }
        _ => panic!("expected CallTool responses"),
    }
}

// --- Admin tools via proxy ---

#[tokio::test]
async fn test_admin_tools_list_backends() {
    let mut proxy = build_proxy().await;

    // Register admin tools as a gateway/ backend
    let admin_router = tower_mcp::McpRouter::new()
        .server_info("admin", "1.0.0")
        .tool(
            ToolBuilder::new("list_backends")
                .description("List backends")
                .handler(
                    |_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("math,text")) },
                )
                .build(),
        );
    let admin_transport = ChannelTransport::new(admin_router);
    proxy
        .add_backend("gateway", admin_transport)
        .await
        .expect("add gateway backend");

    // List tools should include gateway/list_backends
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(
                names.contains(&"gateway/list_backends"),
                "should have admin tool: {:?}",
                names
            );
            assert!(names.contains(&"math/add"), "original tools present");
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }

    // Call admin tool
    let resp = call(
        &mut proxy,
        tool_call("gateway/list_backends", serde_json::json!({})),
    )
    .await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "math,text");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_dynamic_add_backend() {
    let mut proxy = build_proxy().await;

    // Verify we start with 3 tools (math/add, text/echo, text/upper)
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    let initial_count = match resp.inner.unwrap() {
        McpResponse::ListTools(result) => result.tools.len(),
        _ => panic!("expected ListTools"),
    };
    assert_eq!(initial_count, 3);

    // Dynamically add another backend
    let extra_router = McpRouter::new().server_info("extra", "1.0.0").tool(
        ToolBuilder::new("ping")
            .description("Ping")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("pong")) })
            .build(),
    );
    let extra_transport = ChannelTransport::new(extra_router);
    proxy
        .add_backend("extra", extra_transport)
        .await
        .expect("add extra backend");

    // Should now have 4 tools
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names.len(), 4, "should have 4 tools: {:?}", names);
            assert!(names.contains(&"extra/ping"));
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }

    // Call the new tool
    let resp = call(&mut proxy, tool_call("extra/ping", serde_json::json!({}))).await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => assert_eq!(result.all_text(), "pong"),
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// --- Cache stats ---

#[tokio::test]
async fn test_cache_stats_through_proxy() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    // Miss
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    // Hit
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    // Miss (different args)
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 3, "b": 4})),
    )
    .await;

    let stats = handle.stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].namespace, "math/");
    assert_eq!(stats[0].hits, 1);
    assert_eq!(stats[0].misses, 2);

    // Clear and verify counters reset
    handle.clear();
    let stats = handle.stats();
    assert_eq!(stats[0].hits, 0);
    assert_eq!(stats[0].misses, 0);
}

// --- Stacked middleware ---

#[tokio::test]
async fn test_proxy_with_stacked_middleware() {
    let proxy = build_proxy().await;

    // Validation -> Filter -> Proxy
    let validation = ValidationConfig {
        max_argument_size: Some(1024),
    };
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
    }];

    let filtered = CapabilityFilterService::new(proxy, filters);
    let mut svc = ValidationService::new(filtered, validation);

    // Normal call works
    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hello"})),
    )
    .await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => assert_eq!(result.all_text(), "hello"),
        other => panic!("expected CallTool, got: {:?}", other),
    }

    // Filtered tool is denied
    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hello"})),
    )
    .await;
    assert!(resp.inner.is_err(), "filtered tool should be denied");

    // List shows filtered view
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(
                names.len(),
                2,
                "should have math/add + text/echo: {:?}",
                names
            );
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}
