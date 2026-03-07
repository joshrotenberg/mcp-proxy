//! MCP admin tools for gateway introspection.
//!
//! Registers tools under the `gateway/` namespace that allow any MCP client
//! to query gateway status. Uses `ChannelTransport` to add an in-process
//! backend to the proxy.

use std::sync::Arc;

use serde::Serialize;
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
}

#[derive(Serialize)]
struct BackendInfo {
    namespace: String,
    healthy: bool,
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
/// - `gateway/session_count` -- active session count
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

    McpRouter::new()
        .server_info("mcp-gateway-admin", "0.1.0")
        .tool(list_backends)
        .tool(session_count)
        .tool(config_tool)
}
