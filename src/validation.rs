//! Request validation middleware for the gateway.
//!
//! Validates tool call arguments against size limits before forwarding.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use tower_mcp::protocol::McpRequest;
use tower_mcp::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

/// Configuration for request validation.
#[derive(Clone)]
pub struct ValidationConfig {
    /// Maximum serialized size of tool call arguments in bytes.
    pub max_argument_size: Option<usize>,
}

/// Middleware that validates requests before forwarding.
#[derive(Clone)]
pub struct ValidationService<S> {
    inner: S,
    config: Arc<ValidationConfig>,
}

impl<S> ValidationService<S> {
    /// Create a new validation service wrapping `inner`.
    pub fn new(inner: S, config: ValidationConfig) -> Self {
        Self {
            inner,
            config: Arc::new(config),
        }
    }
}

impl<S> Service<RouterRequest> for ValidationService<S>
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
        let config = Arc::clone(&self.config);
        let request_id = req.id.clone();

        // Validate argument size for tool calls
        if let McpRequest::CallTool(ref params) = req.inner
            && let Some(max_size) = config.max_argument_size
        {
            let size = serde_json::to_string(&params.arguments)
                .map(|s| s.len())
                .unwrap_or(0);
            if size > max_size {
                return Box::pin(async move {
                    Ok(RouterResponse {
                        id: request_id,
                        inner: Err(JsonRpcError::invalid_params(format!(
                            "Tool arguments exceed maximum size: {} bytes (limit: {} bytes)",
                            size, max_size
                        ))),
                    })
                });
            }
        }

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::McpRequest;

    use super::{ValidationConfig, ValidationService};
    use crate::test_util::{MockService, call_service};

    #[tokio::test]
    async fn test_validation_passes_small_arguments() {
        let mock = MockService::with_tools(&["tool"]);
        let config = ValidationConfig {
            max_argument_size: Some(1024),
        };
        let mut svc = ValidationService::new(mock, config);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"key": "small"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok(), "small args should pass validation");
    }

    #[tokio::test]
    async fn test_validation_rejects_large_arguments() {
        let mock = MockService::with_tools(&["tool"]);
        let config = ValidationConfig {
            max_argument_size: Some(10), // 10 bytes
        };
        let mut svc = ValidationService::new(mock, config);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"key": "this string is definitely longer than 10 bytes"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        let err = resp.inner.unwrap_err();
        assert!(
            err.message.contains("exceed maximum size"),
            "should mention size exceeded: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_validation_passes_non_tool_requests() {
        let mock = MockService::with_tools(&["tool"]);
        let config = ValidationConfig {
            max_argument_size: Some(1),
        };
        let mut svc = ValidationService::new(mock, config);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok(), "non-tool requests should pass");
    }

    #[tokio::test]
    async fn test_validation_disabled_passes_everything() {
        let mock = MockService::with_tools(&["tool"]);
        let config = ValidationConfig {
            max_argument_size: None,
        };
        let mut svc = ValidationService::new(mock, config);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"key": "any size is fine"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }
}
