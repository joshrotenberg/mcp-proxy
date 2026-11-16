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
                    ttl_ms: None,
                    cache_scope: None,
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

/// A mock service whose `CallTool` result text is the JSON serialization of
/// the arguments it received, so tests can assert what middleware forwarded.
#[derive(Clone)]
pub struct ArgsEchoService;

impl Service<RouterRequest> for ArgsEchoService {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let id = req.id.clone();
        Box::pin(async move {
            let inner = match req.inner {
                McpRequest::CallTool(params) => Ok(McpResponse::CallTool(CallToolResult::text(
                    params.arguments.to_string(),
                ))),
                _ => Ok(McpResponse::Pong(Default::default())),
            };
            Ok(RouterResponse { id, inner })
        })
    }
}

/// Send a `CallTool` with the given arguments through `svc` (typically wrapping
/// [`ArgsEchoService`]) and return the arguments the inner service received.
pub async fn forwarded_args<S>(
    svc: &mut S,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send,
{
    let resp = call_service(
        svc,
        McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: name.to_string(),
            arguments,
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        }),
    )
    .await;
    match resp.inner {
        Ok(McpResponse::CallTool(result)) => {
            serde_json::from_str(&result.all_text()).expect("echoed arguments are JSON")
        }
        other => panic!("expected CallTool response, got {other:?}"),
    }
}

/// What [`ExtensionProbe`] saw for one `CallTool`: the tool name, the `String`
/// extension marker, and whether a `RequestContext` was present and cancelled.
pub type ProbeRecord = (String, Option<String>, Option<bool>);

/// A mock service that records request extensions for each `CallTool`, and
/// fails calls whose name starts with `fail_prefix`.
#[derive(Clone, Default)]
pub struct ExtensionProbe {
    pub seen: std::sync::Arc<std::sync::Mutex<Vec<ProbeRecord>>>,
    pub fail_prefix: Option<&'static str>,
}

impl Service<RouterRequest> for ExtensionProbe {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let id = req.id.clone();
        let inner = match &req.inner {
            McpRequest::CallTool(params) => {
                self.seen.lock().unwrap().push((
                    params.name.clone(),
                    req.extensions.get::<String>().cloned(),
                    req.extensions
                        .get::<tower_mcp::RequestContext>()
                        .map(|context| context.is_cancelled()),
                ));
                if self
                    .fail_prefix
                    .is_some_and(|prefix| params.name.starts_with(prefix))
                {
                    Err(tower_mcp_types::JsonRpcError {
                        code: -32603,
                        message: "probe failure".to_string(),
                        data: None,
                    })
                } else {
                    Ok(McpResponse::CallTool(CallToolResult::text("ok")))
                }
            }
            _ => Ok(McpResponse::Pong(Default::default())),
        };
        Box::pin(async move { Ok(RouterResponse { id, inner }) })
    }
}

/// A `CallTool` request carrying a `String("caller")` marker and a cancelled
/// `RequestContext`, for checking what routing layers forward.
pub fn marked_call(name: &str) -> RouterRequest {
    let mut extensions = Extensions::new();
    extensions.insert(String::from("caller"));
    let context = tower_mcp::RequestContext::new(RequestId::Number(1));
    context.cancellation_token().cancel();
    extensions.insert(context);
    RouterRequest {
        id: RequestId::Number(1),
        inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: name.to_string(),
            arguments: serde_json::json!({}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        }),
        extensions,
    }
}

/// A mock service that always returns a JSON-RPC error response.
#[derive(Clone)]
pub struct ErrorMockService;

impl Service<RouterRequest> for ErrorMockService {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let id = req.id.clone();
        Box::pin(async move {
            Ok(RouterResponse {
                id,
                inner: Err(tower_mcp_types::JsonRpcError {
                    code: -32603,
                    message: "internal error".to_string(),
                    data: None,
                }),
            })
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
