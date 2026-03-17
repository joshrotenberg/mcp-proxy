//! Structured access logging middleware.
//!
//! Logs each MCP request with structured fields including method, tool/resource
//! name, backend name, duration, and status. Uses the `mcp::access` tracing
//! target so operators can filter access logs independently.
//!
//! The backend name is derived from the namespace prefix of the tool name. For
//! example, with separator `/`, a tool named `math/add` belongs to backend
//! `math`.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower::{Layer, Service};
use tower_mcp::protocol::McpRequest;
use tower_mcp::{RouterRequest, RouterResponse};

/// Tower layer that produces an [`AccessLogService`].
#[derive(Clone)]
pub struct AccessLogLayer {
    separator: String,
}

impl Default for AccessLogLayer {
    fn default() -> Self {
        Self {
            separator: "/".to_string(),
        }
    }
}

impl AccessLogLayer {
    /// Create a new access log layer with the given namespace separator.
    pub fn new(separator: impl Into<String>) -> Self {
        Self {
            separator: separator.into(),
        }
    }
}

impl<S> Layer<S> for AccessLogLayer {
    type Service = AccessLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessLogService::new(inner, self.separator.clone())
    }
}

/// Tower service that emits structured access log entries.
///
/// Includes the backend name derived from the namespace prefix of tool names.
#[derive(Clone)]
pub struct AccessLogService<S> {
    inner: S,
    separator: String,
}

impl<S> AccessLogService<S> {
    /// Create a new access log service wrapping `inner`.
    ///
    /// The `separator` is used to split namespaced tool names into backend and
    /// tool components (e.g. with separator `/`, `math/add` yields backend
    /// `math`).
    pub fn new(inner: S, separator: impl Into<String>) -> Self {
        Self {
            inner,
            separator: separator.into(),
        }
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

/// Extract the backend name from a namespaced target string.
///
/// Given a target like `"math/add"` and separator `"/"`, returns `Some("math")`.
/// Returns `None` if the target does not contain the separator.
fn extract_backend<'a>(target: &'a str, separator: &str) -> Option<&'a str> {
    target.find(separator).map(|idx| &target[..idx])
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
        let backend = target
            .as_deref()
            .and_then(|t| extract_backend(t, &self.separator))
            .map(|s| s.to_string());
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

            match (target, backend) {
                (Some(name), Some(be)) => {
                    tracing::info!(
                        target: "mcp::access",
                        method = %method,
                        target = %name,
                        backend = %be,
                        duration_ms = duration_ms,
                        status = %status,
                    );
                }
                (Some(name), None) => {
                    tracing::info!(
                        target: "mcp::access",
                        method = %method,
                        target = %name,
                        duration_ms = duration_ms,
                        status = %status,
                    );
                }
                _ => {
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

    use super::{AccessLogService, extract_backend};
    use crate::test_util::{ErrorMockService, MockService, call_service};

    #[test]
    fn test_extract_backend_with_separator() {
        assert_eq!(extract_backend("math/add", "/"), Some("math"));
        assert_eq!(extract_backend("db/query", "/"), Some("db"));
        assert_eq!(extract_backend("math::add", "::"), Some("math"));
    }

    #[test]
    fn test_extract_backend_no_separator() {
        assert_eq!(extract_backend("add", "/"), None);
        assert_eq!(extract_backend("tool", "::"), None);
    }

    #[test]
    fn test_extract_backend_multiple_separators() {
        // Should return the first segment before the first separator.
        assert_eq!(extract_backend("a/b/c", "/"), Some("a"));
    }

    #[tokio::test]
    async fn test_access_log_passes_through_list() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = AccessLogService::new(mock, "/");

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_access_log_passes_through_tool_call() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = AccessLogService::new(mock, "/");

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
    async fn test_access_log_passes_through_namespaced_tool_call() {
        let mock = MockService::with_tools(&["math/add"]);
        let mut svc = AccessLogService::new(mock, "/");

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "math/add".to_string(),
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
        let mut svc = AccessLogService::new(mock, "/");

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_err());
    }

    #[tokio::test]
    async fn test_access_log_handles_ping() {
        let mock = MockService::with_tools(&[]);
        let mut svc = AccessLogService::new(mock, "/");

        let resp = call_service(&mut svc, McpRequest::Ping).await;
        assert!(resp.inner.is_ok());
    }
}
