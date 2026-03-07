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
