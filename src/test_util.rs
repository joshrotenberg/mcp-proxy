//! Shared test utilities for middleware testing.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::protocol::{
    CallToolResult, ListToolsResult, McpRequest, McpResponse, RequestId, ToolDefinition,
};
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};

/// A simple service that echoes back known tool names for list requests
/// and returns a text result for call requests.
#[derive(Clone)]
pub struct MockService {
    pub tools: Vec<ToolDefinition>,
}

impl MockService {
    pub fn with_tools(names: &[&str]) -> Self {
        let tools = names
            .iter()
            .map(|name| ToolDefinition {
                name: name.to_string(),
                title: None,
                description: Some(format!("{name} tool")),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icons: None,
                annotations: None,
                execution: None,
                meta: None,
            })
            .collect();
        Self { tools }
    }
}

impl Service<RouterRequest> for MockService {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let id = req.id.clone();
        let tools = self.tools.clone();

        Box::pin(async move {
            let inner = match req.inner {
                McpRequest::ListTools(_) => Ok(McpResponse::ListTools(ListToolsResult {
                    tools,
                    next_cursor: None,
                    meta: None,
                })),
                McpRequest::CallTool(params) => Ok(McpResponse::CallTool(CallToolResult::text(
                    format!("called: {}", params.name),
                ))),
                _ => Ok(McpResponse::Pong(Default::default())),
            };
            Ok(RouterResponse { id, inner })
        })
    }
}

/// Helper to send an MCP request through any service that implements
/// `Service<RouterRequest, Response=RouterResponse, Error=Infallible>`.
pub async fn call_service<S>(svc: &mut S, request: McpRequest) -> RouterResponse
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
