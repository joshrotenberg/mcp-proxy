//! Request validation middleware for the proxy.
//!
//! Validates incoming requests against configurable constraints before they
//! reach backend services. Currently supports argument size limits for tool
//! calls; additional validation rules can be added to [`ValidationConfig`]
//! as needed.
//!
//! # Argument size validation
//!
//! When `max_argument_size` is set, the [`ValidationService`] serializes
//! `CallTool` arguments to JSON and checks the byte length against the
//! limit. Requests that exceed the limit are rejected immediately with an
//! `invalid_params` JSON-RPC error containing the actual and maximum sizes.
//! This prevents oversized payloads from reaching backends that may have
//! their own (less informative) size limits.
//!
//! Non-`CallTool` requests (e.g., `ListTools`, `ReadResource`, `Ping`)
//! pass through without validation. When `max_argument_size` is `None`,
//! all requests pass through.
//!
//! # Configuration
//!
//! Argument size limits are configured in the `[security]` section of the
//! TOML config:
//!
//! ```toml
//! [security]
//! max_argument_size = 1048576  # 1 MiB
//! ```
//!
//! Omit `max_argument_size` (or set it to `null` in YAML) to disable
//! argument size validation entirely.
//!
//! # Middleware stack position
//!
//! Validation runs early in the middleware stack -- after request coalescing
//! but before capability filtering. This means oversized requests are
//! rejected before any filtering or routing logic runs. The ordering in
//! `proxy.rs`:
//!
//! 1. Request coalescing
//! 2. **Request validation** (this module)
//! 3. Capability filtering ([`crate::filter`])
//! 4. Search-mode filtering ([`crate::filter`])
//! 5. Tool aliasing ([`crate::alias`])
//! 6. Composite tools ([`crate::composite`])

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use tower_mcp::protocol::McpRequest;

/// Tower layer that produces a [`ValidationService`].
#[derive(Clone)]
pub struct ValidationLayer {
    config: ValidationConfig,
}

impl ValidationLayer {
    /// Create a new validation layer.
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for ValidationLayer {
    type Service = ValidationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ValidationService::new(inner, self.config.clone())
    }
}
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
