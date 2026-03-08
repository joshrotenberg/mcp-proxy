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

use mcp_proxy::alias::{AliasMap, AliasService};
use mcp_proxy::cache::CacheService;
use mcp_proxy::canary::CanaryService;
use mcp_proxy::coalesce::CoalesceService;
use mcp_proxy::config::BackendCacheConfig;
use mcp_proxy::config::{BackendFilter, InjectArgsConfig, NameFilter};
use mcp_proxy::filter::CapabilityFilterService;
use mcp_proxy::inject::{InjectArgsService, InjectionRules};
use mcp_proxy::mirror::MirrorService;
use mcp_proxy::validation::{ValidationConfig, ValidationService};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

/// A tool that echoes back all arguments as JSON, for testing argument injection.
#[derive(Debug, Deserialize, JsonSchema)]
struct ArgsInput {
    #[serde(flatten)]
    args: serde_json::Map<String, serde_json::Value>,
}

fn math_router() -> McpRouter {
    let add = ToolBuilder::new("add")
        .description("Add two numbers")
        .handler(|input: AddInput| async move {
            Ok(CallToolResult::text(format!("{}", input.a + input.b)))
        })
        .build();

    let echo_args = ToolBuilder::new("echo_args")
        .description("Echo back all arguments as JSON")
        .handler(|input: ArgsInput| async move {
            Ok(CallToolResult::text(
                serde_json::to_string(&input.args).unwrap(),
            ))
        })
        .build();

    McpRouter::new()
        .server_info("math-server", "1.0.0")
        .tool(add)
        .tool(echo_args)
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
            assert!(names.contains(&"math/echo_args"), "tools: {:?}", names);
            assert!(names.contains(&"text/echo"), "tools: {:?}", names);
            assert!(names.contains(&"text/upper"), "tools: {:?}", names);
            assert_eq!(names.len(), 4);
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

    // Verify we start with 4 tools (math/add, math/echo_args, text/echo, text/upper)
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    let initial_count = match resp.inner.unwrap() {
        McpResponse::ListTools(result) => result.tools.len(),
        _ => panic!("expected ListTools"),
    };
    assert_eq!(initial_count, 4);

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

    // Should now have 5 tools
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names.len(), 5, "should have 5 tools: {:?}", names);
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
                3,
                "should have math/add + math/echo_args + text/echo: {:?}",
                names
            );
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}

// --- Proxy + inject args ---

#[tokio::test]
async fn test_proxy_with_inject_default_args() {
    let proxy = build_proxy().await;
    let mut defaults = serde_json::Map::new();
    defaults.insert("timeout".to_string(), serde_json::json!(30));

    let rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let mut svc = InjectArgsService::new(proxy, rules);

    // Call echo_args with just "query" -- should get "timeout" injected
    let resp = call(
        &mut svc,
        tool_call("math/echo_args", serde_json::json!({"query": "SELECT 1"})),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            let args: serde_json::Value = serde_json::from_str(&result.all_text()).unwrap();
            assert_eq!(args["query"], "SELECT 1");
            assert_eq!(args["timeout"], 30, "default arg should be injected");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_proxy_with_inject_does_not_overwrite() {
    let proxy = build_proxy().await;
    let mut defaults = serde_json::Map::new();
    defaults.insert("timeout".to_string(), serde_json::json!(30));

    let rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let mut svc = InjectArgsService::new(proxy, rules);

    // Call with timeout already set -- should NOT be overwritten
    let resp = call(
        &mut svc,
        tool_call(
            "math/echo_args",
            serde_json::json!({"query": "SELECT 1", "timeout": 60}),
        ),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            let args: serde_json::Value = serde_json::from_str(&result.all_text()).unwrap();
            assert_eq!(args["timeout"], 60, "existing value should be preserved");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_proxy_with_inject_per_tool_overwrite() {
    let proxy = build_proxy().await;
    let tool_rules = vec![InjectArgsConfig {
        tool: "echo_args".to_string(),
        args: {
            let mut m = serde_json::Map::new();
            m.insert("forced".to_string(), serde_json::json!(true));
            m
        },
        overwrite: true,
    }];

    let rules = vec![InjectionRules::new(
        "math/".to_string(),
        serde_json::Map::new(),
        tool_rules,
    )];
    let mut svc = InjectArgsService::new(proxy, rules);

    // Call with forced=false -- should be overwritten to true
    let resp = call(
        &mut svc,
        tool_call(
            "math/echo_args",
            serde_json::json!({"data": "hi", "forced": false}),
        ),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            let args: serde_json::Value = serde_json::from_str(&result.all_text()).unwrap();
            assert_eq!(args["forced"], true, "should be overwritten");
            assert_eq!(args["data"], "hi", "other args preserved");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_proxy_with_inject_skips_other_namespace() {
    let proxy = build_proxy().await;
    let mut defaults = serde_json::Map::new();
    defaults.insert("injected".to_string(), serde_json::json!(true));

    // Only inject into math/ namespace
    let rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let mut svc = InjectArgsService::new(proxy, rules);

    // Call text/echo -- should NOT get injected args
    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hello"})),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "hello");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// --- Proxy + mirror ---

#[tokio::test]
async fn test_proxy_with_mirror_primary_response_returned() {
    // Mirror sends a fire-and-forget copy; primary response is returned to caller.
    let math_transport = ChannelTransport::new(math_router());
    let text_transport = ChannelTransport::new(text_router());
    let text_v2_transport = ChannelTransport::new(text_router());

    let proxy = McpProxy::builder("mirror-proxy", "1.0.0")
        .separator("/")
        .backend("math", math_transport)
        .await
        .backend("text", text_transport)
        .await
        .backend("text-v2", text_v2_transport)
        .await
        .build_strict()
        .await
        .expect("proxy should build");

    let mirror_mappings = [("text".to_string(), ("text-v2".to_string(), 100u32))]
        .into_iter()
        .collect();

    let mut svc = MirrorService::new(proxy, mirror_mappings, "/");

    // Call primary -- should get normal response
    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hello"})),
    )
    .await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "hello");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }

    // List tools should show all backends
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"text/echo"));
            assert!(names.contains(&"text-v2/echo"));
            assert!(names.contains(&"math/add"));
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}

// --- Proxy + coalesce ---

#[tokio::test]
async fn test_proxy_with_coalesce() {
    let proxy = build_proxy().await;
    let mut svc = CoalesceService::new(proxy);

    // Sequential calls with same args should both succeed
    let resp1 = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 7, "b": 8})),
    )
    .await;
    let resp2 = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 7, "b": 8})),
    )
    .await;

    match (resp1.inner.unwrap(), resp2.inner.unwrap()) {
        (McpResponse::CallTool(r1), McpResponse::CallTool(r2)) => {
            assert_eq!(r1.all_text(), "15");
            assert_eq!(r2.all_text(), "15");
        }
        _ => panic!("expected CallTool responses"),
    }
}

// --- Proxy + canary ---

#[tokio::test]
async fn test_canary_routes_all_traffic_when_primary_weight_zero() {
    // Two backends with the same tools: primary "api" and canary "api-canary"
    let api_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("primary")) })
            .build();
        McpRouter::new().server_info("api", "1.0.0").tool(tool)
    };

    let canary_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("canary")) })
            .build();
        McpRouter::new()
            .server_info("api-canary", "1.0.0")
            .tool(tool)
    };

    let proxy = McpProxy::builder("canary-proxy", "1.0.0")
        .separator("/")
        .backend("api", ChannelTransport::new(api_router))
        .await
        .backend("api-canary", ChannelTransport::new(canary_router))
        .await
        .build_strict()
        .await
        .expect("proxy should build");

    // 0 primary weight = 100% canary
    let canaries = [("api".to_string(), ("api-canary".to_string(), 0u32, 100u32))]
        .into_iter()
        .collect();
    let mut svc = CanaryService::new(proxy, canaries, "/");

    // All requests to api/search should be rewritten to api-canary/search
    let resp = call(&mut svc, tool_call("api/search", serde_json::json!({}))).await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "canary", "should route to canary");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_canary_passes_through_when_primary_weight_full() {
    let api_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("primary")) })
            .build();
        McpRouter::new().server_info("api", "1.0.0").tool(tool)
    };

    let canary_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("canary")) })
            .build();
        McpRouter::new()
            .server_info("api-canary", "1.0.0")
            .tool(tool)
    };

    let proxy = McpProxy::builder("canary-proxy", "1.0.0")
        .separator("/")
        .backend("api", ChannelTransport::new(api_router))
        .await
        .backend("api-canary", ChannelTransport::new(canary_router))
        .await
        .build_strict()
        .await
        .expect("proxy should build");

    // 100 primary / 0 canary -- but total_weight would be 100, so all go to primary
    let canaries = [("api".to_string(), ("api-canary".to_string(), 100u32, 0u32))]
        .into_iter()
        .collect();
    let mut svc = CanaryService::new(proxy, canaries, "/");

    let resp = call(&mut svc, tool_call("api/search", serde_json::json!({}))).await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "primary", "should route to primary");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_canary_with_filter_hides_canary_tools() {
    let api_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("primary")) })
            .build();
        McpRouter::new().server_info("api", "1.0.0").tool(tool)
    };

    let canary_router = {
        let tool = ToolBuilder::new("search")
            .description("Search")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("canary")) })
            .build();
        McpRouter::new()
            .server_info("api-canary", "1.0.0")
            .tool(tool)
    };

    let proxy = McpProxy::builder("canary-proxy", "1.0.0")
        .separator("/")
        .backend("api", ChannelTransport::new(api_router))
        .await
        .backend("api-canary", ChannelTransport::new(canary_router))
        .await
        .build_strict()
        .await
        .expect("proxy should build");

    // Canary middleware (0 primary = all canary)
    let canaries = [("api".to_string(), ("api-canary".to_string(), 0u32, 100u32))]
        .into_iter()
        .collect();
    let canary_svc = CanaryService::new(proxy, canaries, "/");

    // Filter to hide canary tools (empty AllowList = hide everything)
    let filters = vec![BackendFilter {
        namespace: "api-canary/".to_string(),
        tool_filter: NameFilter::AllowList(std::collections::HashSet::new()),
        resource_filter: NameFilter::AllowList(std::collections::HashSet::new()),
        prompt_filter: NameFilter::AllowList(std::collections::HashSet::new()),
    }];
    let mut svc = CapabilityFilterService::new(canary_svc, filters);

    // ListTools should only show api/search, not api-canary/search
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(
                names.contains(&"api/search"),
                "primary visible: {:?}",
                names
            );
            assert!(
                !names.contains(&"api-canary/search"),
                "canary hidden: {:?}",
                names
            );
            assert_eq!(names.len(), 1, "only primary tool: {:?}", names);
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }

    // But CallTool to api/search should still route to canary and succeed
    let resp = call(&mut svc, tool_call("api/search", serde_json::json!({}))).await;

    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert_eq!(result.all_text(), "canary", "should route to canary");
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// --- Full middleware stack composition ---

#[tokio::test]
async fn test_full_middleware_stack() {
    let proxy = build_proxy().await;

    // Build a realistic middleware stack:
    // validation -> alias -> filter -> inject -> proxy
    let mut defaults = serde_json::Map::new();
    defaults.insert("timeout".to_string(), serde_json::json!(30));
    let inject_rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];

    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
    }];

    let aliases = AliasMap::new(vec![(
        "math/".to_string(),
        "add".to_string(),
        "sum".to_string(),
    )])
    .unwrap();

    let validation = ValidationConfig {
        max_argument_size: Some(4096),
    };

    // Stack: outermost -> innermost
    let injected = InjectArgsService::new(proxy, inject_rules);
    let filtered = CapabilityFilterService::new(injected, filters);
    let aliased = AliasService::new(filtered, aliases);
    let mut svc = ValidationService::new(aliased, validation);

    // math/add is aliased to math/sum
    let resp = call(
        &mut svc,
        tool_call("math/sum", serde_json::json!({"a": 100, "b": 200})),
    )
    .await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => assert_eq!(result.all_text(), "300"),
        other => panic!("expected CallTool, got: {:?}", other),
    }

    // text/upper is filtered out
    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hi"})),
    )
    .await;
    assert!(resp.inner.is_err());

    // text/echo works normally
    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "world"})),
    )
    .await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => assert_eq!(result.all_text(), "world"),
        other => panic!("expected CallTool, got: {:?}", other),
    }

    // List tools shows aliased + filtered view
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    match resp.inner.unwrap() {
        McpResponse::ListTools(result) => {
            let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"math/sum"), "aliased name: {:?}", names);
            assert!(!names.contains(&"math/add"), "original hidden: {:?}", names);
            assert!(names.contains(&"text/echo"), "echo visible: {:?}", names);
            assert!(
                !names.contains(&"text/upper"),
                "upper filtered: {:?}",
                names
            );
        }
        other => panic!("expected ListTools, got: {:?}", other),
    }
}
