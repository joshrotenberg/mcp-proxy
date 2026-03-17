//! MCP admin tools for proxy introspection.
//!
//! Registers tools under the `proxy/` namespace that allow any MCP client
//! to query proxy status. Uses `ChannelTransport` to add an in-process
//! backend to the proxy.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_mcp::client::ChannelTransport;
use tower_mcp::proxy::{AddBackendError, McpProxy};
use tower_mcp::{CallToolResult, McpRouter, NoParams, SessionHandle, ToolBuilder};

use crate::admin::AdminState;
use crate::config::ProxyConfig;

/// Shared state accessible to admin tool handlers.
#[derive(Clone)]
struct AdminToolState {
    admin_state: AdminState,
    session_handle: SessionHandle,
    config_snapshot: Arc<String>,
    proxy: McpProxy,
}

#[derive(Serialize)]
struct BackendInfo {
    namespace: String,
    healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_checked_at: Option<String>,
    consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
}

#[derive(Serialize)]
struct BackendsResult {
    proxy_name: String,
    proxy_version: String,
    backend_count: usize,
    backends: Vec<BackendInfo>,
}

#[derive(Serialize)]
struct SessionResult {
    active_sessions: usize,
}

/// Register admin tools as an in-process backend on the proxy.
///
/// Tools are added under the `proxy/` namespace:
/// - `proxy/list_backends` -- list backends with health status
/// - `proxy/health_check` -- cached health check results
/// - `proxy/session_count` -- active session count
/// - `proxy/add_backend` -- dynamically add an HTTP backend
/// - `proxy/config` -- dump current config (TOML)
/// - `proxy/call_tool` -- (search mode only) invoke any backend tool by name
pub async fn register_admin_tools(
    proxy: &McpProxy,
    admin_state: AdminState,
    session_handle: SessionHandle,
    config: &ProxyConfig,
    discovery_tools: Option<Vec<tower_mcp::Tool>>,
) -> Result<(), AddBackendError> {
    let config_toml =
        toml::to_string_pretty(config).unwrap_or_else(|e| format!("error serializing: {e}"));

    let search_mode = config.proxy.tool_exposure == crate::config::ToolExposure::Search;

    let state = AdminToolState {
        admin_state,
        session_handle,
        config_snapshot: Arc::new(config_toml),
        proxy: proxy.clone(),
    };

    let router = build_admin_router(state, discovery_tools, search_mode);
    let transport = ChannelTransport::new(router);

    proxy.add_backend("proxy", transport).await
}

fn build_admin_router(
    state: AdminToolState,
    discovery_tools: Option<Vec<tower_mcp::Tool>>,
    search_mode: bool,
) -> McpRouter {
    let state_for_backends = state.clone();
    let list_backends = ToolBuilder::new("list_backends")
        .description("List all proxy backends with health status")
        .handler(move |_: NoParams| {
            let s = state_for_backends.clone();
            async move {
                let health = s.admin_state.health().await;
                let backends: Vec<BackendInfo> = health
                    .iter()
                    .map(|b| BackendInfo {
                        namespace: b.namespace.clone(),
                        healthy: b.healthy,
                        last_checked_at: b.last_checked_at.map(|t| t.to_rfc3339()),
                        consecutive_failures: b.consecutive_failures,
                        error: b.error.clone(),
                        transport: b.transport.clone(),
                    })
                    .collect();

                let result = BackendsResult {
                    proxy_name: s.admin_state.proxy_name().to_string(),
                    proxy_version: s.admin_state.proxy_version().to_string(),
                    backend_count: s.admin_state.backend_count(),
                    backends,
                };

                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&result).unwrap(),
                ))
            }
        })
        .build();

    let state_for_sessions = state.clone();
    let session_count = ToolBuilder::new("session_count")
        .description("Get the number of active MCP sessions")
        .handler(move |_: NoParams| {
            let s = state_for_sessions.clone();
            async move {
                let count = s.session_handle.session_count().await;
                let result = SessionResult {
                    active_sessions: count,
                };
                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&result).unwrap(),
                ))
            }
        })
        .build();

    let config_snapshot = Arc::clone(&state.config_snapshot);
    let config_tool = ToolBuilder::new("config")
        .description("Dump the current proxy configuration")
        .handler(move |_: NoParams| {
            let config = Arc::clone(&config_snapshot);
            async move { Ok(CallToolResult::text((*config).clone())) }
        })
        .build();

    let state_for_health = state.clone();
    let health_check = ToolBuilder::new("health_check")
        .description("Get cached health check results for all backends")
        .handler(move |_: NoParams| {
            let s = state_for_health.clone();
            async move {
                let health = s.admin_state.health().await;
                let backends: Vec<BackendInfo> = health
                    .iter()
                    .map(|b| BackendInfo {
                        namespace: b.namespace.clone(),
                        healthy: b.healthy,
                        last_checked_at: b.last_checked_at.map(|t| t.to_rfc3339()),
                        consecutive_failures: b.consecutive_failures,
                        error: b.error.clone(),
                        transport: b.transport.clone(),
                    })
                    .collect();
                let healthy_count = backends.iter().filter(|b| b.healthy).count();
                let total = backends.len();
                let result = HealthCheckResult {
                    status: if healthy_count == total {
                        "healthy"
                    } else {
                        "degraded"
                    }
                    .to_string(),
                    healthy_count,
                    total_count: total,
                    backends,
                };
                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&result).unwrap(),
                ))
            }
        })
        .build();

    let state_for_add = state.clone();
    let add_backend = ToolBuilder::new("add_backend")
        .description("Dynamically add an HTTP backend to the proxy")
        .handler(move |input: AddBackendInput| {
            let s = state_for_add.clone();
            async move {
                let transport = tower_mcp::client::HttpClientTransport::new(&input.url);
                match s.proxy.add_backend(&input.name, transport).await {
                    Ok(()) => Ok(CallToolResult::text(format!(
                        "Backend '{}' added successfully at {}",
                        input.name, input.url
                    ))),
                    Err(e) => Ok(CallToolResult::text(format!(
                        "Failed to add backend '{}': {e}",
                        input.name
                    ))),
                }
            }
        })
        .build();

    let mut router = McpRouter::new()
        .server_info("mcp-proxy-admin", "0.1.0")
        .tool(list_backends)
        .tool(health_check)
        .tool(session_count)
        .tool(add_backend)
        .tool(config_tool);

    if search_mode {
        let state_for_call = state.clone();
        let call_tool = ToolBuilder::new("call_tool")
            .description(
                "Invoke any backend tool by its fully-qualified name. Use proxy/search_tools \
                 to discover available tools, then call them through this tool.",
            )
            .handler(move |input: CallToolInput| {
                let s = state_for_call.clone();
                async move {
                    use tower::Service;
                    use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
                    use tower_mcp::router::{Extensions, RouterRequest};

                    let req = RouterRequest {
                        id: RequestId::Number(0),
                        inner: McpRequest::CallTool(CallToolParams {
                            name: input.name.clone(),
                            arguments: input.arguments.unwrap_or_default().into(),
                            meta: None,
                            task: None,
                        }),
                        extensions: Extensions::new(),
                    };

                    let mut proxy = s.proxy.clone();
                    match proxy.call(req).await {
                        Ok(resp) => match resp.inner {
                            Ok(McpResponse::CallTool(result)) => Ok(result),
                            Ok(_) => Ok(CallToolResult::text(format!(
                                "Unexpected response type for tool '{}'",
                                input.name
                            ))),
                            Err(e) => Ok(CallToolResult::text(format!(
                                "Error calling '{}': {}",
                                input.name, e.message
                            ))),
                        },
                        Err(_) => Ok(CallToolResult::text(format!(
                            "Internal error calling '{}'",
                            input.name
                        ))),
                    }
                }
            })
            .build();
        router = router.tool(call_tool);
    }

    if let Some(tools) = discovery_tools {
        for tool in tools {
            router = router.tool(tool);
        }
    }

    router
}

#[derive(Serialize)]
struct HealthCheckResult {
    status: String,
    healthy_count: usize,
    total_count: usize,
    backends: Vec<BackendInfo>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddBackendInput {
    /// Name/namespace for the new backend
    name: String,
    /// URL of the HTTP MCP server
    url: String,
}

/// Input for the `proxy/call_tool` meta-tool (search mode only).
#[derive(Debug, Deserialize, JsonSchema)]
struct CallToolInput {
    /// Fully-qualified tool name (e.g. "math/add", "files/read_file")
    name: String,
    /// Arguments to pass to the tool
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}
