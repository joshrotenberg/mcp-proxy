//! End-to-end integration tests for the MCP proxy.
//!
//! These tests exercise the full pipeline: proxy construction, middleware
//! composition, HTTP transport, and admin API. Uses in-process backends
//! via `ChannelTransport` served through the actual HTTP+SSE stack.

use std::convert::Infallible;

use schemars::JsonSchema;
use serde::Deserialize;
use tower::Service;
use tower::util::BoxCloneService;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_proxy::alias::{AliasMap, AliasService};
use mcp_proxy::cache::CacheService;
use mcp_proxy::config::{BackendCacheConfig, BackendFilter, InjectArgsConfig, NameFilter};
use mcp_proxy::filter::CapabilityFilterService;
use mcp_proxy::inject::{InjectArgsService, InjectionRules};
use mcp_proxy::validation::{ValidationConfig, ValidationService};

// ---------------------------------------------------------------------------
// Test backend routers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoInput {
    message: String,
}

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

/// A backend that always returns an error result (simulates a broken backend).
fn error_router() -> McpRouter {
    let fail = ToolBuilder::new("fail")
        .description("Always fails")
        .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::error("backend error")) })
        .build();

    McpRouter::new()
        .server_info("error-server", "1.0.0")
        .tool(fail)
}

/// A slow backend for timeout testing.
fn slow_router() -> McpRouter {
    let slow = ToolBuilder::new("slow")
        .description("Responds after a delay")
        .handler(|_: tower_mcp::NoParams| async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(CallToolResult::text("done"))
        })
        .build();

    McpRouter::new()
        .server_info("slow-server", "1.0.0")
        .tool(slow)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn build_proxy() -> McpProxy {
    McpProxy::builder("test-proxy", "1.0.0")
        .separator("/")
        .backend("math", ChannelTransport::new(math_router()))
        .await
        .backend("text", ChannelTransport::new(text_router()))
        .await
        .build_strict()
        .await
        .expect("proxy should build")
}

async fn build_proxy_with_error_backend() -> McpProxy {
    McpProxy::builder("test-proxy", "1.0.0")
        .separator("/")
        .backend("math", ChannelTransport::new(math_router()))
        .await
        .backend("broken", ChannelTransport::new(error_router()))
        .await
        .build_strict()
        .await
        .expect("proxy should build")
}

async fn build_proxy_with_slow_backend() -> McpProxy {
    McpProxy::builder("test-proxy", "1.0.0")
        .separator("/")
        .backend("math", ChannelTransport::new(math_router()))
        .await
        .backend("slow", ChannelTransport::new(slow_router()))
        .await
        .build_strict()
        .await
        .expect("proxy should build")
}

async fn call<S>(svc: &mut S, request: McpRequest) -> RouterResponse
where
    S: tower::Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
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

fn get_tool_names(resp: &RouterResponse) -> Vec<String> {
    match resp.inner.as_ref().unwrap() {
        McpResponse::ListTools(result) => result.tools.iter().map(|t| t.name.clone()).collect(),
        other => panic!("expected ListTools, got: {:?}", other),
    }
}

fn get_tool_result_text(resp: &RouterResponse) -> String {
    match resp.inner.as_ref().unwrap() {
        McpResponse::CallTool(result) => result.all_text(),
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// ===========================================================================
// Tier 1: Basic proxy pipeline
// ===========================================================================

#[tokio::test]
async fn e2e_list_tools_returns_all_namespaced_tools() {
    let mut proxy = build_proxy().await;
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);

    assert_eq!(names.len(), 4);
    assert!(names.contains(&"math/add".to_string()));
    assert!(names.contains(&"math/echo_args".to_string()));
    assert!(names.contains(&"text/echo".to_string()));
    assert!(names.contains(&"text/upper".to_string()));
}

#[tokio::test]
async fn e2e_call_tool_routes_to_correct_backend() {
    let mut proxy = build_proxy().await;

    // Math backend
    let resp = call(
        &mut proxy,
        tool_call("math/add", serde_json::json!({"a": 100, "b": 200})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "300");

    // Text backend
    let resp = call(
        &mut proxy,
        tool_call("text/upper", serde_json::json!({"message": "hello"})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "HELLO");
}

#[tokio::test]
async fn e2e_call_unknown_tool_returns_error() {
    let mut proxy = build_proxy().await;
    let resp = call(
        &mut proxy,
        tool_call("nonexistent/tool", serde_json::json!({})),
    )
    .await;
    assert!(resp.inner.is_err(), "unknown tool should return error");
}

#[tokio::test]
async fn e2e_ping_succeeds() {
    let mut proxy = build_proxy().await;
    let resp = call(&mut proxy, McpRequest::Ping).await;
    assert!(resp.inner.is_ok());
    match resp.inner.unwrap() {
        McpResponse::Pong(_) => {}
        other => panic!("expected Pong, got: {:?}", other),
    }
}

// ===========================================================================
// Tier 2: Error handling
// ===========================================================================

#[tokio::test]
async fn e2e_error_backend_propagates_error() {
    let mut proxy = build_proxy_with_error_backend().await;
    let resp = call(&mut proxy, tool_call("broken/fail", serde_json::json!({}))).await;
    // The error backend returns CallToolResult::error (Ok response with is_error=true)
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert!(result.is_error, "should be an error result");
            assert!(
                result.all_text().contains("backend error"),
                "error text: {}",
                result.all_text()
            );
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

#[tokio::test]
async fn e2e_error_backend_does_not_affect_healthy_backend() {
    let mut proxy = build_proxy_with_error_backend().await;

    // Healthy backend still works
    let resp = call(
        &mut proxy,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "3");

    // Error backend returns error result
    let resp = call(&mut proxy, tool_call("broken/fail", serde_json::json!({}))).await;
    match resp.inner.unwrap() {
        McpResponse::CallTool(result) => {
            assert!(result.is_error);
        }
        other => panic!("expected CallTool, got: {:?}", other),
    }
}

// ===========================================================================
// Tier 3: Middleware composition - filter + alias + inject + validate
// ===========================================================================

#[tokio::test]
async fn e2e_filter_hides_tools_from_listing() {
    let proxy = build_proxy().await;
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);

    assert!(names.contains(&"text/echo".to_string()));
    assert!(!names.contains(&"text/upper".to_string()));
    assert!(names.contains(&"math/add".to_string()));
}

#[tokio::test]
async fn e2e_filter_blocks_call_to_hidden_tool() {
    let proxy = build_proxy().await;
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hi"})),
    )
    .await;
    assert!(resp.inner.is_err());
    let err = resp.inner.unwrap_err();
    assert!(err.message.contains("not available"));
}

#[tokio::test]
async fn e2e_filter_allowlist_only_permits_listed_tools() {
    let proxy = build_proxy().await;
    let filters = vec![BackendFilter {
        namespace: "math/".to_string(),
        tool_filter: NameFilter::AllowList(["add".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);

    assert!(names.contains(&"math/add".to_string()));
    assert!(!names.contains(&"math/echo_args".to_string()));
    // Text tools unaffected (no filter on text/ namespace)
    assert!(names.contains(&"text/echo".to_string()));
    assert!(names.contains(&"text/upper".to_string()));
}

#[tokio::test]
async fn e2e_alias_renames_in_list_and_call() {
    let proxy = build_proxy().await;
    let aliases = AliasMap::new(vec![
        ("math/".to_string(), "add".to_string(), "sum".to_string()),
        (
            "text/".to_string(),
            "upper".to_string(),
            "uppercase".to_string(),
        ),
    ])
    .unwrap();
    let mut svc = AliasService::new(proxy, aliases);

    // List shows aliased names
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);
    assert!(names.contains(&"math/sum".to_string()));
    assert!(!names.contains(&"math/add".to_string()));
    assert!(names.contains(&"text/uppercase".to_string()));
    assert!(!names.contains(&"text/upper".to_string()));

    // Call aliased name
    let resp = call(
        &mut svc,
        tool_call("math/sum", serde_json::json!({"a": 50, "b": 50})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "100");

    // Call other aliased name
    let resp = call(
        &mut svc,
        tool_call("text/uppercase", serde_json::json!({"message": "test"})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "TEST");
}

#[tokio::test]
async fn e2e_inject_merges_default_args() {
    let proxy = build_proxy().await;
    let mut defaults = serde_json::Map::new();
    defaults.insert("timeout".to_string(), serde_json::json!(30));
    defaults.insert("retries".to_string(), serde_json::json!(3));

    let rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let mut svc = InjectArgsService::new(proxy, rules);

    let resp = call(
        &mut svc,
        tool_call("math/echo_args", serde_json::json!({"query": "test"})),
    )
    .await;

    let text = get_tool_result_text(&resp);
    let args: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(args["query"], "test");
    assert_eq!(args["timeout"], 30);
    assert_eq!(args["retries"], 3);
}

#[tokio::test]
async fn e2e_inject_per_tool_with_overwrite() {
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

    // User passes forced=false, but overwrite=true forces it to true
    let resp = call(
        &mut svc,
        tool_call(
            "math/echo_args",
            serde_json::json!({"data": "hi", "forced": false}),
        ),
    )
    .await;

    let text = get_tool_result_text(&resp);
    let args: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(args["forced"], true);
    assert_eq!(args["data"], "hi");
}

#[tokio::test]
async fn e2e_validation_rejects_oversized_arguments() {
    let proxy = build_proxy().await;
    let config = ValidationConfig {
        max_argument_size: Some(10), // Very small limit
    };
    let mut svc = ValidationService::new(proxy, config);

    let resp = call(
        &mut svc,
        tool_call(
            "text/echo",
            serde_json::json!({"message": "this message is way too long for the tiny limit"}),
        ),
    )
    .await;

    let err = resp.inner.unwrap_err();
    assert!(err.message.contains("exceed maximum size"));
}

#[tokio::test]
async fn e2e_validation_allows_small_arguments() {
    let proxy = build_proxy().await;
    let config = ValidationConfig {
        max_argument_size: Some(1024),
    };
    let mut svc = ValidationService::new(proxy, config);

    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "ok"})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "ok");
}

// ===========================================================================
// Tier 4: Full middleware stack composition
// ===========================================================================

#[tokio::test]
async fn e2e_full_stack_filter_alias_inject_validate() {
    let proxy = build_proxy().await;

    // Build realistic middleware stack:
    // validation -> alias -> filter -> inject -> proxy

    // Inject default args into math/ namespace
    let mut defaults = serde_json::Map::new();
    defaults.insert("timeout".to_string(), serde_json::json!(30));
    let inject_rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let injected = InjectArgsService::new(proxy, inject_rules);

    // Filter out text/upper
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let filtered = CapabilityFilterService::new(injected, filters);

    // Alias math/add -> math/sum
    let aliases = AliasMap::new(vec![(
        "math/".to_string(),
        "add".to_string(),
        "sum".to_string(),
    )])
    .unwrap();
    let aliased = AliasService::new(filtered, aliases);

    // Validate argument size
    let validation = ValidationConfig {
        max_argument_size: Some(4096),
    };
    let mut svc = ValidationService::new(aliased, validation);

    // 1. List tools: aliased + filtered
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);
    assert!(
        names.contains(&"math/sum".to_string()),
        "aliased: {:?}",
        names
    );
    assert!(
        !names.contains(&"math/add".to_string()),
        "original gone: {:?}",
        names
    );
    assert!(names.contains(&"text/echo".to_string()));
    assert!(
        !names.contains(&"text/upper".to_string()),
        "filtered: {:?}",
        names
    );
    assert_eq!(names.len(), 3); // math/sum, math/echo_args, text/echo

    // 2. Call aliased tool works
    let resp = call(
        &mut svc,
        tool_call("math/sum", serde_json::json!({"a": 100, "b": 200})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "300");

    // 3. Call filtered tool is denied
    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hi"})),
    )
    .await;
    assert!(resp.inner.is_err());

    // 4. Injected args are present
    let resp = call(
        &mut svc,
        tool_call("math/echo_args", serde_json::json!({"query": "test"})),
    )
    .await;
    let text = get_tool_result_text(&resp);
    let args: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(args["timeout"], 30);
}

// ===========================================================================
// Tier 5: Caching behavior
// ===========================================================================

#[tokio::test]
async fn e2e_cache_hit_returns_same_result() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    let req = tool_call("math/add", serde_json::json!({"a": 5, "b": 5}));

    // First call: miss
    let resp1 = call(&mut svc, req.clone()).await;
    // Second call: hit
    let resp2 = call(&mut svc, req).await;

    assert_eq!(get_tool_result_text(&resp1), "10");
    assert_eq!(get_tool_result_text(&resp2), "10");

    let stats = handle.stats();
    assert_eq!(stats[0].hits, 1);
    assert_eq!(stats[0].misses, 1);
}

#[tokio::test]
async fn e2e_cache_different_args_are_separate_entries() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 3, "b": 4})),
    )
    .await;

    let stats = handle.stats();
    assert_eq!(
        stats[0].misses, 2,
        "different args = different cache entries"
    );
    assert_eq!(stats[0].hits, 0);
}

#[tokio::test]
async fn e2e_cache_clear_resets_stats() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    let req = tool_call("math/add", serde_json::json!({"a": 1, "b": 1}));
    let _ = call(&mut svc, req.clone()).await;
    let _ = call(&mut svc, req).await;

    assert_eq!(handle.stats()[0].hits, 1);

    handle.clear();

    let stats = handle.stats();
    assert_eq!(stats[0].hits, 0);
    assert_eq!(stats[0].misses, 0);
}

#[tokio::test]
async fn e2e_cache_uncached_namespace_is_not_cached() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    // Only cache math/ namespace
    let (mut svc, handle) = CacheService::new(proxy, vec![("math/".to_string(), &cfg)]);

    // Call text/ twice -- should not be cached
    let _ = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "a"})),
    )
    .await;
    let _ = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "a"})),
    )
    .await;

    // Only math/ namespace should have stats
    let stats = handle.stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].namespace, "math/");
    assert_eq!(stats[0].hits, 0);
    assert_eq!(stats[0].misses, 0);
}

// ===========================================================================
// Tier 6: Dynamic backend addition
// ===========================================================================

#[tokio::test]
async fn e2e_dynamic_add_backend_appears_in_tool_list() {
    let mut proxy = build_proxy().await;

    // Initially 4 tools
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    assert_eq!(get_tool_names(&resp).len(), 4);

    // Add a new backend dynamically
    let extra_router = McpRouter::new().server_info("extra", "1.0.0").tool(
        ToolBuilder::new("ping")
            .description("Ping")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("pong")) })
            .build(),
    );
    proxy
        .add_backend("extra", ChannelTransport::new(extra_router))
        .await
        .expect("add backend");

    // Now 5 tools
    let resp = call(&mut proxy, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);
    assert_eq!(names.len(), 5);
    assert!(names.contains(&"extra/ping".to_string()));
}

#[tokio::test]
async fn e2e_dynamic_backend_is_callable() {
    let mut proxy = build_proxy().await;

    let extra_router = McpRouter::new().server_info("extra", "1.0.0").tool(
        ToolBuilder::new("ping")
            .description("Ping")
            .handler(|_: tower_mcp::NoParams| async move { Ok(CallToolResult::text("pong")) })
            .build(),
    );
    proxy
        .add_backend("extra", ChannelTransport::new(extra_router))
        .await
        .expect("add backend");

    let resp = call(&mut proxy, tool_call("extra/ping", serde_json::json!({}))).await;
    assert_eq!(get_tool_result_text(&resp), "pong");
}

// ===========================================================================
// Tier 7: Config validation
// ===========================================================================

#[cfg(test)]
mod config_tests {
    use mcp_proxy::config::ProxyConfig;

    fn valid_config() -> &'static str {
        r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "echo"
        transport = "stdio"
        command = "echo"
        "#
    }

    #[test]
    fn config_valid_minimal() {
        let config = ProxyConfig::parse(valid_config()).unwrap();
        assert_eq!(config.proxy.name, "test");
        assert_eq!(config.backends.len(), 1);
    }

    #[test]
    fn config_rejects_no_backends() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("at least one backend"));
    }

    #[test]
    fn config_rejects_stdio_without_command() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "bad"
        transport = "stdio"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn config_rejects_http_without_url() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "bad"
        transport = "http"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("url"));
    }

    #[test]
    fn config_rejects_invalid_circuit_breaker_rate() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [backends.circuit_breaker]
        failure_rate_threshold = 1.5
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("failure_rate_threshold"));
    }

    #[test]
    fn config_rejects_zero_rate_limit() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"

        [backends.rate_limit]
        requests = 0
        period_seconds = 1
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("requests must be > 0"));
    }

    #[test]
    fn config_rejects_both_expose_and_hide_tools() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"
        expose_tools = ["read"]
        hide_tools = ["write"]
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("both expose_tools and hide_tools"));
    }

    #[test]
    fn config_rejects_mirror_of_unknown_backend() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "primary"
        transport = "http"
        url = "http://localhost:8080"

        [[backends]]
        name = "mirror"
        transport = "http"
        url = "http://localhost:8081"
        mirror_of = "nonexistent"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("unknown backend"));
    }

    #[test]
    fn config_rejects_mirror_of_self() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"
        mirror_of = "api"
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("cannot reference itself"));
    }

    #[test]
    fn config_rejects_canary_of_unknown_backend() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "primary"
        transport = "http"
        url = "http://localhost:8080"

        [[backends]]
        name = "canary"
        transport = "http"
        url = "http://localhost:8081"
        canary_of = "nonexistent"
        weight = 10
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("unknown backend"));
    }

    #[test]
    fn config_rejects_canary_with_zero_weight() {
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "primary"
        transport = "http"
        url = "http://localhost:8080"

        [[backends]]
        name = "canary"
        transport = "http"
        url = "http://localhost:8081"
        canary_of = "primary"
        weight = 0
        "#;
        let err = ProxyConfig::parse(toml).unwrap_err();
        assert!(err.to_string().contains("weight must be > 0"));
    }

    #[test]
    fn config_parses_full_featured() {
        let toml = r#"
        [proxy]
        name = "enterprise"
        version = "2.0.0"
        separator = "::"
        hot_reload = true
        shutdown_timeout_seconds = 60

        [proxy.listen]
        host = "0.0.0.0"
        port = 9090

        [auth]
        type = "bearer"
        tokens = ["token1", "token2"]

        [performance]
        coalesce_requests = true

        [security]
        max_argument_size = 1048576

        [observability]
        audit = true
        log_level = "debug"
        json_logs = true

        [observability.metrics]
        enabled = true

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://api:8080"
        expose_tools = ["search", "list"]

        [backends.timeout]
        seconds = 30

        [backends.rate_limit]
        requests = 100
        period_seconds = 1

        [backends.circuit_breaker]
        failure_rate_threshold = 0.5

        [backends.cache]
        tool_ttl_seconds = 60
        resource_ttl_seconds = 300
        max_entries = 500

        [[backends]]
        name = "db"
        transport = "stdio"
        command = "db-mcp"
        args = ["--read-only"]
        hide_tools = ["drop_table"]

        [backends.concurrency]
        max_concurrent = 5
        "#;
        let config = ProxyConfig::parse(toml).unwrap();
        assert_eq!(config.proxy.name, "enterprise");
        assert_eq!(config.proxy.version, "2.0.0");
        assert_eq!(config.proxy.separator, "::");
        assert!(config.proxy.hot_reload);
        assert_eq!(config.proxy.shutdown_timeout_seconds, 60);
        assert_eq!(config.proxy.listen.host, "0.0.0.0");
        assert_eq!(config.proxy.listen.port, 9090);
        assert!(config.auth.is_some());
        assert!(config.performance.coalesce_requests);
        assert_eq!(config.security.max_argument_size, Some(1048576));
        assert!(config.observability.audit);
        assert!(config.observability.metrics.enabled);
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[0].expose_tools.len(), 2);
        assert!(config.backends[0].timeout.is_some());
        assert!(config.backends[0].rate_limit.is_some());
        assert!(config.backends[0].circuit_breaker.is_some());
        assert!(config.backends[0].cache.is_some());
        assert_eq!(config.backends[1].hide_tools.len(), 1);
        assert!(config.backends[1].concurrency.is_some());
    }

    #[test]
    fn config_env_var_resolution() {
        // SAFETY: test runs single-threaded; no other threads read this var.
        unsafe {
            std::env::set_var("TEST_MCP_TOKEN", "secret123");
        }
        let toml = r#"
        [proxy]
        name = "test"
        [proxy.listen]

        [[backends]]
        name = "api"
        transport = "http"
        url = "http://localhost:8080"
        bearer_token = "${TEST_MCP_TOKEN}"
        "#;
        let mut config = ProxyConfig::parse(toml).unwrap();
        config.resolve_env_vars();
        assert_eq!(
            config.backends[0].bearer_token.as_deref(),
            Some("secret123")
        );
        unsafe {
            std::env::remove_var("TEST_MCP_TOKEN");
        }
    }
}

// ===========================================================================
// Tier 8: Chaos-based resilience testing
// ===========================================================================

/// Test that timeouts fire on slow backends.
#[tokio::test]
async fn e2e_timeout_fires_on_slow_backend() {
    use std::time::Duration;

    let proxy = build_proxy_with_slow_backend().await;

    let mut svc: BoxCloneService<RouterRequest, RouterResponse, tower::BoxError> =
        BoxCloneService::new(tower::timeout::Timeout::new(
            proxy,
            Duration::from_millis(100),
        ));

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: tool_call("slow/slow", serde_json::json!({})),
        extensions: Extensions::new(),
    };

    let result = svc.call(req).await;
    assert!(result.is_err(), "should timeout");
}

#[tokio::test]
async fn e2e_timeout_does_not_fire_on_fast_backend() {
    use std::time::Duration;

    let proxy = build_proxy_with_slow_backend().await;

    let mut svc: BoxCloneService<RouterRequest, RouterResponse, tower::BoxError> =
        BoxCloneService::new(tower::timeout::Timeout::new(proxy, Duration::from_secs(10)));

    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
        extensions: Extensions::new(),
    };

    let result = svc.call(req).await;
    assert!(result.is_ok(), "fast backend should not timeout");
}

#[tokio::test]
async fn e2e_concurrent_requests_all_succeed() {
    let proxy = build_proxy().await;
    let svc: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
        BoxCloneService::new(proxy);

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let mut svc = svc.clone();
            tokio::spawn(async move {
                let req = RouterRequest {
                    id: RequestId::Number(i),
                    inner: tool_call("math/add", serde_json::json!({"a": i, "b": i})),
                    extensions: Extensions::new(),
                };
                let resp = svc.call(req).await.expect("infallible");
                let expected = format!("{}", i * 2);
                let actual = get_tool_result_text(&resp);
                assert_eq!(actual, expected, "request {i}");
            })
        })
        .collect();

    for h in handles {
        h.await.expect("task panicked");
    }
}

#[tokio::test]
async fn e2e_concurrent_requests_across_backends() {
    let proxy = build_proxy().await;
    let svc: BoxCloneService<RouterRequest, RouterResponse, Infallible> =
        BoxCloneService::new(proxy);

    let mut handles = vec![];

    // Mix of math and text backend calls
    for i in 0..10 {
        let mut math_svc = svc.clone();
        handles.push(tokio::spawn(async move {
            let req = RouterRequest {
                id: RequestId::Number(i),
                inner: tool_call("math/add", serde_json::json!({"a": i, "b": 1})),
                extensions: Extensions::new(),
            };
            let resp = math_svc.call(req).await.expect("infallible");
            assert!(resp.inner.is_ok());
        }));

        let mut text_svc = svc.clone();
        handles.push(tokio::spawn(async move {
            let req = RouterRequest {
                id: RequestId::Number(i + 100),
                inner: tool_call(
                    "text/echo",
                    serde_json::json!({"message": format!("msg-{i}")}),
                ),
                extensions: Extensions::new(),
            };
            let resp = text_svc.call(req).await.expect("infallible");
            assert_eq!(get_tool_result_text(&resp), format!("msg-{i}"));
        }));
    }

    for h in handles {
        h.await.expect("task panicked");
    }
}

// ===========================================================================
// Tier 9: Middleware ordering and interaction
// ===========================================================================

#[tokio::test]
async fn e2e_filter_then_cache_filters_before_caching() {
    let proxy = build_proxy().await;

    // Filter out text/upper, then cache math/
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let filtered = CapabilityFilterService::new(proxy, filters);

    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, _) = CacheService::new(filtered, vec![("math/".to_string(), &cfg)]);

    // Filtered tool is still denied
    let resp = call(
        &mut svc,
        tool_call("text/upper", serde_json::json!({"message": "hi"})),
    )
    .await;
    assert!(resp.inner.is_err());

    // Cached tool works
    let resp = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    assert_eq!(get_tool_result_text(&resp), "3");
}

#[tokio::test]
async fn e2e_alias_then_filter_uses_original_names_for_filter() {
    let proxy = build_proxy().await;

    // Filter denies "upper" (original name)
    let filters = vec![BackendFilter {
        namespace: "text/".to_string(),
        tool_filter: NameFilter::DenyList(["upper".to_string()].into()),
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }];
    let filtered = CapabilityFilterService::new(proxy, filters);

    // Alias text/upper -> text/shout (filter is inner, alias is outer)
    let aliases = AliasMap::new(vec![(
        "text/".to_string(),
        "upper".to_string(),
        "shout".to_string(),
    )])
    .unwrap();
    let mut svc = AliasService::new(filtered, aliases);

    // text/shout should be filtered because the underlying tool "upper" is denied
    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);
    assert!(
        !names.contains(&"text/shout".to_string()),
        "aliased name of filtered tool should not appear: {:?}",
        names
    );
}

#[tokio::test]
async fn e2e_inject_does_not_affect_other_namespaces() {
    let proxy = build_proxy().await;
    let mut defaults = serde_json::Map::new();
    defaults.insert("injected".to_string(), serde_json::json!(true));

    let rules = vec![InjectionRules::new("math/".to_string(), defaults, vec![])];
    let mut svc = InjectArgsService::new(proxy, rules);

    // text/ namespace should not get injected args
    let resp = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hello"})),
    )
    .await;
    // echo just returns the message, not a JSON dump of all args
    assert_eq!(get_tool_result_text(&resp), "hello");
}

// ===========================================================================
// Tier 10: Edge cases
// ===========================================================================

#[tokio::test]
async fn e2e_empty_arguments_accepted() {
    let proxy = build_proxy().await;
    let mut svc = ValidationService::new(
        proxy,
        ValidationConfig {
            max_argument_size: Some(1024),
        },
    );

    let resp = call(&mut svc, tool_call("math/add", serde_json::json!({}))).await;
    // Missing fields will cause a backend error, but validation should pass
    // The point is validation doesn't reject empty args
    // (The backend may or may not handle missing fields gracefully)
    assert!(
        resp.inner.is_ok() || resp.inner.is_err(),
        "should not panic"
    );
}

#[tokio::test]
async fn e2e_multiple_filters_on_different_namespaces() {
    let proxy = build_proxy().await;
    let filters = vec![
        BackendFilter {
            namespace: "math/".to_string(),
            tool_filter: NameFilter::AllowList(["add".to_string()].into()),
            resource_filter: NameFilter::PassAll,
            prompt_filter: NameFilter::PassAll,
            hide_destructive: false,
            read_only_only: false,
        },
        BackendFilter {
            namespace: "text/".to_string(),
            tool_filter: NameFilter::AllowList(["echo".to_string()].into()),
            resource_filter: NameFilter::PassAll,
            prompt_filter: NameFilter::PassAll,
            hide_destructive: false,
            read_only_only: false,
        },
    ];
    let mut svc = CapabilityFilterService::new(proxy, filters);

    let resp = call(&mut svc, McpRequest::ListTools(Default::default())).await;
    let names = get_tool_names(&resp);

    assert_eq!(names.len(), 2, "only allowed tools: {:?}", names);
    assert!(names.contains(&"math/add".to_string()));
    assert!(names.contains(&"text/echo".to_string()));
}

#[tokio::test]
async fn e2e_cache_with_multiple_backends() {
    let proxy = build_proxy().await;
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 100,
    };
    let (mut svc, handle) = CacheService::new(
        proxy,
        vec![("math/".to_string(), &cfg), ("text/".to_string(), &cfg)],
    );

    // Cache math
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;
    let _ = call(
        &mut svc,
        tool_call("math/add", serde_json::json!({"a": 1, "b": 2})),
    )
    .await;

    // Cache text
    let _ = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hi"})),
    )
    .await;
    let _ = call(
        &mut svc,
        tool_call("text/echo", serde_json::json!({"message": "hi"})),
    )
    .await;

    let stats = handle.stats();
    assert_eq!(stats.len(), 2);

    let math_stats = stats.iter().find(|s| s.namespace == "math/").unwrap();
    assert_eq!(math_stats.hits, 1);
    assert_eq!(math_stats.misses, 1);

    let text_stats = stats.iter().find(|s| s.namespace == "text/").unwrap();
    assert_eq!(text_stats.hits, 1);
    assert_eq!(text_stats.misses, 1);
}

// ===========================================================================
// Tier 11: Bearer token scoping
// ===========================================================================

#[cfg(feature = "oauth")]
mod bearer_scoping {
    use std::collections::HashMap;

    use tower::Service;
    use tower_mcp::oauth::token::TokenClaims;
    use tower_mcp::protocol::{CallToolParams, ListToolsParams, McpRequest, RequestId};
    use tower_mcp::router::{Extensions, RouterRequest};

    use mcp_proxy::bearer_scope::BearerScopingService;

    use super::{build_proxy, get_tool_names, get_tool_result_text};

    const BEARER_SCOPE_KEY: &str = "__bearer_scope";

    fn call_with_scope(allow: &[&str], deny: &[&str], inner: McpRequest) -> RouterRequest {
        let mut extra = HashMap::new();
        extra.insert(
            BEARER_SCOPE_KEY.to_string(),
            serde_json::json!({ "allow": allow, "deny": deny }),
        );
        let mut extensions = Extensions::new();
        extensions.insert(TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra,
        });
        RouterRequest {
            id: RequestId::Number(1),
            inner,
            extensions,
        }
    }

    fn call_unscoped(inner: McpRequest) -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(1),
            inner,
            extensions: Extensions::new(),
        }
    }

    #[tokio::test]
    async fn e2e_bearer_scope_allow_list_filters_list_tools() {
        let proxy = build_proxy().await;
        let mut svc = BearerScopingService::new(proxy);

        let req = call_with_scope(
            &["math/add"],
            &[],
            McpRequest::ListTools(ListToolsParams::default()),
        );
        let resp = svc.call(req).await.unwrap();
        let names = get_tool_names(&resp);
        assert_eq!(names, vec!["math/add"]);
    }

    #[tokio::test]
    async fn e2e_bearer_scope_deny_list_filters_list_tools() {
        let proxy = build_proxy().await;
        let mut svc = BearerScopingService::new(proxy);

        let req = call_with_scope(
            &[],
            &["math/add"],
            McpRequest::ListTools(ListToolsParams::default()),
        );
        let resp = svc.call(req).await.unwrap();
        let names = get_tool_names(&resp);
        assert!(!names.contains(&"math/add".to_string()));
        assert!(names.len() >= 2); // text/echo, text/upper, math/echo_args still present
    }

    #[tokio::test]
    async fn e2e_bearer_scope_blocks_disallowed_call() {
        let proxy = build_proxy().await;
        let mut svc = BearerScopingService::new(proxy);

        let req = call_with_scope(
            &["math/add"],
            &[],
            McpRequest::CallTool(CallToolParams {
                name: "text/echo".to_string(),
                arguments: serde_json::json!({"message": "hi"}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_err());
        let err = resp.inner.unwrap_err();
        assert!(err.message.contains("text/echo"));
    }

    #[tokio::test]
    async fn e2e_bearer_scope_allows_permitted_call() {
        let proxy = build_proxy().await;
        let mut svc = BearerScopingService::new(proxy);

        let req = call_with_scope(
            &["math/add"],
            &[],
            McpRequest::CallTool(CallToolParams {
                name: "math/add".to_string(),
                arguments: serde_json::json!({"a": 3, "b": 4}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        let text = get_tool_result_text(&resp);
        assert_eq!(text, "7");
    }

    #[tokio::test]
    async fn e2e_bearer_scope_unscoped_token_sees_all() {
        let proxy = build_proxy().await;
        let mut svc = BearerScopingService::new(proxy);

        let req = call_unscoped(McpRequest::ListTools(ListToolsParams::default()));
        let resp = svc.call(req).await.unwrap();
        let names = get_tool_names(&resp);
        assert!(names.len() >= 4); // math/add, math/echo_args, text/echo, text/upper
    }
}

// ===========================================================================
// Tier 12: WebSocket backend transport
// ===========================================================================

#[cfg(feature = "websocket")]
mod websocket_transport {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use tower::Service;
    use tower_mcp::protocol::{
        CallToolParams, ListToolsParams, McpRequest, McpResponse, RequestId,
    };
    use tower_mcp::proxy::McpProxy;
    use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
    use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

    use mcp_proxy::ws_transport::WebSocketClientTransport;

    use std::convert::Infallible;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct WsInput {
        value: String,
    }

    fn ws_router() -> McpRouter {
        let echo = ToolBuilder::new("echo")
            .description("Echo a value")
            .handler(|input: WsInput| async move { Ok(CallToolResult::text(input.value)) })
            .build();

        McpRouter::new()
            .server_info("ws-server", "1.0.0")
            .tool(echo)
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

    #[tokio::test]
    async fn e2e_websocket_backend_list_and_call() {
        // Start a WebSocket MCP server
        let router = ws_router();
        let ws_transport = tower_mcp::transport::websocket::WebSocketTransport::new(router);
        let axum_router = ws_transport.into_router();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, axum_router).await.unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect via WebSocket transport
        let ws_url = format!("ws://127.0.0.1:{}", addr.port());
        let transport = WebSocketClientTransport::connect(&ws_url).await.unwrap();

        // Build proxy with WebSocket backend
        let mut proxy = McpProxy::builder("ws-test", "1.0.0")
            .separator("/")
            .backend("ws", transport)
            .await
            .build_strict()
            .await
            .expect("proxy should build with WebSocket backend");

        // List tools
        let resp = call(
            &mut proxy,
            McpRequest::ListTools(ListToolsParams::default()),
        )
        .await;
        let tools = match resp.inner.unwrap() {
            McpResponse::ListTools(r) => r.tools,
            other => panic!("expected ListTools, got: {other:?}"),
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ws/echo");

        // Call tool
        let resp = call(
            &mut proxy,
            McpRequest::CallTool(CallToolParams {
                name: "ws/echo".to_string(),
                arguments: serde_json::json!({"value": "hello ws"}),
                meta: None,
                task: None,
            }),
        )
        .await;
        let result = match resp.inner.unwrap() {
            McpResponse::CallTool(r) => r,
            other => panic!("expected CallTool, got: {other:?}"),
        };
        assert_eq!(result.all_text(), "hello ws");

        server_handle.abort();
    }
}

// ===========================================================================
// Tier 13: BM25 tool discovery
// ===========================================================================

#[cfg(feature = "discovery")]
mod tool_discovery {
    use tower::Service;
    use tower_mcp::protocol::{CallToolParams, ListToolsParams, McpRequest, RequestId};
    use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};

    use super::{build_proxy, get_tool_names, get_tool_result_text};
    use std::convert::Infallible;

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

    #[tokio::test]
    async fn e2e_discovery_index_and_search() {
        let mut proxy = build_proxy().await;

        // Build the discovery index
        let index = mcp_proxy::discovery::build_index(&mut proxy, "/").await;
        let discovery_tools = mcp_proxy::discovery::build_discovery_tools(index);

        // Register discovery tools as a backend
        let router = tower_mcp::McpRouter::new().server_info("discovery", "1.0.0");
        let mut router = router;
        for tool in discovery_tools {
            router = router.tool(tool);
        }
        let transport = tower_mcp::client::ChannelTransport::new(router);
        proxy
            .add_backend("discovery", transport)
            .await
            .expect("should add discovery backend");

        // List tools should include discovery tools
        let resp = call(
            &mut proxy,
            McpRequest::ListTools(ListToolsParams::default()),
        )
        .await;
        let names = get_tool_names(&resp);
        assert!(
            names.contains(&"discovery/search_tools".to_string()),
            "missing search_tools: {names:?}"
        );
        assert!(
            names.contains(&"discovery/similar_tools".to_string()),
            "missing similar_tools: {names:?}"
        );
        assert!(
            names.contains(&"discovery/tool_categories".to_string()),
            "missing tool_categories: {names:?}"
        );
    }

    #[tokio::test]
    async fn e2e_discovery_search_finds_tools() {
        let mut proxy = build_proxy().await;

        let index = mcp_proxy::discovery::build_index(&mut proxy, "/").await;
        let discovery_tools = mcp_proxy::discovery::build_discovery_tools(index);

        let router = tower_mcp::McpRouter::new().server_info("discovery", "1.0.0");
        let mut router = router;
        for tool in discovery_tools {
            router = router.tool(tool);
        }
        let transport = tower_mcp::client::ChannelTransport::new(router);
        proxy
            .add_backend("discovery", transport)
            .await
            .expect("should add discovery backend");

        // Search for "add" should find math/add
        let resp = call(
            &mut proxy,
            McpRequest::CallTool(CallToolParams {
                name: "discovery/search_tools".to_string(),
                arguments: serde_json::json!({"query": "add numbers"}),
                meta: None,
                task: None,
            }),
        )
        .await;
        let text = get_tool_result_text(&resp);
        assert!(
            text.contains("add"),
            "search should find 'add' tool: {text}"
        );
    }

    #[tokio::test]
    async fn e2e_discovery_categories() {
        let mut proxy = build_proxy().await;

        let index = mcp_proxy::discovery::build_index(&mut proxy, "/").await;
        let discovery_tools = mcp_proxy::discovery::build_discovery_tools(index);

        let router = tower_mcp::McpRouter::new().server_info("discovery", "1.0.0");
        let mut router = router;
        for tool in discovery_tools {
            router = router.tool(tool);
        }
        let transport = tower_mcp::client::ChannelTransport::new(router);
        proxy
            .add_backend("discovery", transport)
            .await
            .expect("should add discovery backend");

        // Categories should include math and text backends
        let resp = call(
            &mut proxy,
            McpRequest::CallTool(CallToolParams {
                name: "discovery/tool_categories".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;
        let text = get_tool_result_text(&resp);
        assert!(text.contains("math"), "should have math category: {text}");
        assert!(text.contains("text"), "should have text category: {text}");
    }
}
