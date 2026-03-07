//! MCP admin tools for gateway introspection.
//!
//! Registers tools under the `gateway/` namespace that allow any MCP client
//! to query gateway status. Uses `ChannelTransport` to add an in-process
//! backend to the proxy.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tower_mcp::client::ChannelTransport;
use tower_mcp::proxy::{AddBackendError, McpProxy};
use tower_mcp::{CallToolResult, McpRouter, NoParams, SessionHandle, ToolBuilder};

use crate::admin::AdminState;
use crate::config::GatewayConfig;

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
    gateway_name: String,
    gateway_version: String,
    backend_count: usize,
    backends: Vec<BackendInfo>,
}

#[derive(Serialize)]
struct SessionResult {
    active_sessions: usize,
}

/// Register admin tools as an in-process backend on the proxy.
///
/// Tools are added under the `gateway/` namespace:
/// - `gateway/list_backends` -- list backends with health status
/// - `gateway/health_check` -- cached health check results
/// - `gateway/session_count` -- active session count
/// - `gateway/add_backend` -- dynamically add an HTTP backend
/// - `gateway/config` -- dump current config (TOML)
pub async fn register_admin_tools(
    proxy: &McpProxy,
    admin_state: AdminState,
    session_handle: SessionHandle,
    config: &GatewayConfig,
) -> Result<(), AddBackendError> {
    let config_toml =
        toml::to_string_pretty(config).unwrap_or_else(|e| format!("error serializing: {e}"));

    let state = AdminToolState {
        admin_state,
        session_handle,
        config_snapshot: Arc::new(config_toml),
        proxy: proxy.clone(),
    };

    let router = build_admin_router(state);
    let transport = ChannelTransport::new(router);

    proxy.add_backend("gateway", transport).await
}

fn build_admin_router(state: AdminToolState) -> McpRouter {
    let state_for_backends = state.clone();
    let list_backends = ToolBuilder::new("list_backends")
        .description("List all gateway backends with health status")
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
                    gateway_name: s.admin_state.gateway_name().to_string(),
                    gateway_version: s.admin_state.gateway_version().to_string(),
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
        .description("Dump the current gateway configuration")
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
        .description("Dynamically add an HTTP backend to the gateway")
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

    McpRouter::new()
        .server_info("mcp-gateway-admin", "0.1.0")
        .tool(list_backends)
        .tool(health_check)
        .tool(session_count)
        .tool(add_backend)
        .tool(config_tool)
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
