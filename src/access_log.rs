//! Structured access logging middleware.
//!
//! Logs each MCP request with structured fields including method, tool/resource
//! name, duration, and status. Uses the `mcp::access` tracing target so
//! operators can filter access logs independently.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower::Service;
use tower_mcp::protocol::McpRequest;
use tower_mcp::{RouterRequest, RouterResponse};

/// Tower service that emits structured access log entries.
#[derive(Clone)]
pub struct AccessLogService<S> {
    inner: S,
}

impl<S> AccessLogService<S> {
    /// Create a new access log service wrapping `inner`.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

/// Extract the tool, resource, or prompt name from an MCP request.
fn request_target(req: &McpRequest) -> Option<&str> {
    match req {
        McpRequest::CallTool(params) => Some(&params.name),
        McpRequest::ReadResource(params) => Some(&params.uri),
        McpRequest::GetPrompt(params) => Some(&params.name),
        _ => None,
    }
}

impl<S> Service<RouterRequest> for AccessLogService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let method = req.inner.method_name().to_string();
        let target = request_target(&req.inner).map(|s| s.to_string());
        let start = Instant::now();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let result = fut.await;
            let duration_ms = start.elapsed().as_millis() as u64;

            let status = match &result {
                Ok(resp) => {
                    if resp.inner.is_ok() {
                        "ok"
                    } else {
                        "error"
                    }
                }
                Err(_) => "error",
            };

            match target {
                Some(name) => {
                    tracing::info!(
                        target: "mcp::access",
                        method = %method,
                        target = %name,
                        duration_ms = duration_ms,
                        status = %status,
                    );
                }
                None => {
                    tracing::info!(
                        target: "mcp::access",
                        method = %method,
                        duration_ms = duration_ms,
                        status = %status,
                    );
                }
            }

            result
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::McpRequest;

    use super::AccessLogService;
    use crate::test_util::{ErrorMockService, MockService, call_service};

    #[tokio::test]
    async fn test_access_log_passes_through_list() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = AccessLogService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_access_log_passes_through_tool_call() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = AccessLogService::new(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_access_log_handles_errors() {
        let mock = ErrorMockService;
        let mut svc = AccessLogService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_err());
    }

    #[tokio::test]
    async fn test_access_log_handles_ping() {
        let mock = MockService::with_tools(&[]);
        let mut svc = AccessLogService::new(mock);

        let resp = call_service(&mut svc, McpRequest::Ping).await;
        assert!(resp.inner.is_ok());
    }
}
