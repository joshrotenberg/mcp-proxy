//! Backend failover middleware.
//!
//! Routes requests to a primary backend, automatically falling over to
//! secondary backends when the primary returns an error. Multiple failover
//! backends can be configured per primary, ordered by [`priority`](crate::config::BackendConfig::priority)
//! (lower values tried first).
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "api"
//! transport = "http"
//! url = "http://primary:8080"
//!
//! [[backends]]
//! name = "api-backup"
//! transport = "http"
//! url = "http://secondary:8080"
//! failover_for = "api"
//! priority = 0            # tried first (default)
//!
//! [[backends]]
//! name = "api-backup-2"
//! transport = "http"
//! url = "http://tertiary:8080"
//! failover_for = "api"
//! priority = 10           # tried second
//! ```
//!
//! # How it works
//!
//! 1. Request arrives targeting `api/search`
//! 2. Request is forwarded to the `api` backend
//! 3. If `api` returns an error, the request is retried against `api-backup/search`
//! 4. If `api-backup` also fails, the request is retried against `api-backup-2/search`
//! 5. Failover backend tools are hidden from `ListTools` (like canary backends)

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{CallToolParams, GetPromptParams, McpRequest, ReadResourceParams};

/// Resolved failover mapping for a single primary backend.
#[derive(Debug, Clone)]
struct FailoverMapping {
    /// Primary namespace prefix (e.g. "api/").
    primary_prefix: String,
    /// Ordered list of failover namespace prefixes (e.g. ["api-backup/", "api-backup-2/"]).
    /// Tried in order until one succeeds.
    failover_prefixes: Vec<String>,
}

/// Tower layer that produces a [`FailoverService`].
#[derive(Clone)]
pub struct FailoverLayer {
    failovers: HashMap<String, Vec<String>>,
    separator: String,
}

impl FailoverLayer {
    /// Create a new failover layer.
    ///
    /// `failovers` maps primary backend names to an ordered list of failover
    /// backend names (sorted by priority, lowest first).
    pub fn new(failovers: HashMap<String, Vec<String>>, separator: impl Into<String>) -> Self {
        Self {
            failovers,
            separator: separator.into(),
        }
    }
}

impl<S> Layer<S> for FailoverLayer {
    type Service = FailoverService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FailoverService::new(inner, self.failovers.clone(), &self.separator)
    }
}

/// Tower service that fails over to secondary backends on primary error.
///
/// When a primary backend returns an error, failover backends are tried
/// in priority order until one succeeds or all have been exhausted.
#[derive(Clone)]
pub struct FailoverService<S> {
    inner: S,
    mappings: Arc<Vec<FailoverMapping>>,
}

impl<S> FailoverService<S> {
    /// Create a new failover service.
    ///
    /// `failovers` maps primary backend names to an ordered list of failover
    /// backend names (sorted by priority, lowest first).
    pub fn new(inner: S, failovers: HashMap<String, Vec<String>>, separator: &str) -> Self {
        let mappings = failovers
            .into_iter()
            .map(|(primary, failover_names)| FailoverMapping {
                primary_prefix: format!("{primary}{separator}"),
                failover_prefixes: failover_names
                    .into_iter()
                    .map(|name| format!("{name}{separator}"))
                    .collect(),
            })
            .collect();

        Self {
            inner,
            mappings: Arc::new(mappings),
        }
    }
}

/// Rewrite a request's namespace from primary to failover.
fn rewrite_request(req: &McpRequest, primary_prefix: &str, failover_prefix: &str) -> McpRequest {
    match req {
        McpRequest::CallTool(params) => {
            if let Some(local) = params.name.strip_prefix(primary_prefix) {
                McpRequest::CallTool(CallToolParams {
                    name: format!("{failover_prefix}{local}"),
                    arguments: params.arguments.clone(),
                    meta: params.meta.clone(),
                    task: params.task.clone(),
                })
            } else {
                req.clone()
            }
        }
        McpRequest::ReadResource(params) => {
            if let Some(local) = params.uri.strip_prefix(primary_prefix) {
                McpRequest::ReadResource(ReadResourceParams {
                    uri: format!("{failover_prefix}{local}"),
                    meta: params.meta.clone(),
                })
            } else {
                req.clone()
            }
        }
        McpRequest::GetPrompt(params) => {
            if let Some(local) = params.name.strip_prefix(primary_prefix) {
                McpRequest::GetPrompt(GetPromptParams {
                    name: format!("{failover_prefix}{local}"),
                    arguments: params.arguments.clone(),
                    meta: params.meta.clone(),
                })
            } else {
                req.clone()
            }
        }
        other => other.clone(),
    }
}

impl<S> Service<RouterRequest> for FailoverService<S>
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
        let mappings = Arc::clone(&self.mappings);
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // Find if this request targets a primary that has failovers
            let mapping = mappings.iter().find(|m| match &req.inner {
                McpRequest::CallTool(p) => p.name.starts_with(&m.primary_prefix),
                McpRequest::ReadResource(p) => p.uri.starts_with(&m.primary_prefix),
                McpRequest::GetPrompt(p) => p.name.starts_with(&m.primary_prefix),
                _ => false,
            });

            let mapping = match mapping {
                Some(m) => m.clone(),
                None => {
                    // No failover configured for this request, pass through
                    return inner.call(req).await;
                }
            };

            // Try primary
            let primary_resp = inner.call(req.clone()).await?;

            // If primary succeeded, return it
            if primary_resp.inner.is_ok() {
                return Ok(primary_resp);
            }

            // Primary failed -- attempt failovers in priority order
            // TODO: When outlier detection is integrated, check if the primary
            // backend is ejected and skip directly to failover without waiting
            // for an error response. This requires sharing ejection state
            // between the OutlierDetectionService and FailoverService layers.
            let mut last_resp = primary_resp;

            for failover_prefix in &mapping.failover_prefixes {
                let failover_name = failover_prefix.trim_end_matches('/');
                tracing::warn!(
                    primary = %mapping.primary_prefix.trim_end_matches('/'),
                    failover = %failover_name,
                    "Backend failed, attempting failover"
                );

                let failover_request =
                    rewrite_request(&req.inner, &mapping.primary_prefix, failover_prefix);

                let failover_req = RouterRequest {
                    id: req.id.clone(),
                    inner: failover_request,
                    extensions: Extensions::new(),
                };

                let resp = inner.call(failover_req).await?;

                if resp.inner.is_ok() {
                    return Ok(resp);
                }

                last_resp = resp;
            }

            // All failovers exhausted, return the last error
            Ok(last_resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::FailoverService;
    use crate::test_util::{MockService, call_service};

    fn make_failover_svc(mock: MockService) -> FailoverService<MockService> {
        let failovers = [("primary".to_string(), vec!["backup".to_string()])]
            .into_iter()
            .collect();
        FailoverService::new(mock, failovers, "/")
    }

    #[tokio::test]
    async fn test_failover_passes_through_when_no_mapping() {
        let mock = MockService::with_tools(&["other/tool"]);
        let mut svc = make_failover_svc(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_failover_passes_through_on_success() {
        let mock = MockService::with_tools(&["primary/tool", "backup/tool"]);
        let mut svc = make_failover_svc(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "primary/tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok(), "successful primary should pass through");
    }

    #[tokio::test]
    async fn test_failover_retries_on_primary_error() {
        // Create a mock that returns errors for "primary/" calls
        // but succeeds for "backup/" calls
        use std::convert::Infallible;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tower::Service;
        use tower_mcp::protocol::CallToolResult;
        use tower_mcp::router::{RouterRequest, RouterResponse};

        #[derive(Clone)]
        struct FailPrimaryMock;

        impl Service<RouterRequest> for FailPrimaryMock {
            type Response = RouterResponse;
            type Error = Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: RouterRequest) -> Self::Future {
                let id = req.id.clone();
                Box::pin(async move {
                    let inner = match &req.inner {
                        McpRequest::CallTool(params) if params.name.starts_with("primary/") => {
                            Err(tower_mcp_types::JsonRpcError {
                                code: -32603,
                                message: "primary down".to_string(),
                                data: None,
                            })
                        }
                        McpRequest::CallTool(params) if params.name.starts_with("backup/") => {
                            Ok(McpResponse::CallTool(CallToolResult::text("from backup")))
                        }
                        _ => Ok(McpResponse::Pong(Default::default())),
                    };
                    Ok(RouterResponse { id, inner })
                })
            }
        }

        let failovers = [("primary".to_string(), vec!["backup".to_string()])]
            .into_iter()
            .collect();
        let mut svc = FailoverService::new(FailPrimaryMock, failovers, "/");

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "primary/tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "from backup");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_failover_chain_tries_in_order() {
        // Mock that fails for primary and backup-1, succeeds for backup-2
        use std::convert::Infallible;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tower::Service;
        use tower_mcp::protocol::CallToolResult;
        use tower_mcp::router::{RouterRequest, RouterResponse};

        #[derive(Clone)]
        struct ChainMock;

        impl Service<RouterRequest> for ChainMock {
            type Response = RouterResponse;
            type Error = Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: RouterRequest) -> Self::Future {
                let id = req.id.clone();
                Box::pin(async move {
                    let inner = match &req.inner {
                        McpRequest::CallTool(params) if params.name.starts_with("primary/") => {
                            Err(tower_mcp_types::JsonRpcError {
                                code: -32603,
                                message: "primary down".to_string(),
                                data: None,
                            })
                        }
                        McpRequest::CallTool(params) if params.name.starts_with("backup-1/") => {
                            Err(tower_mcp_types::JsonRpcError {
                                code: -32603,
                                message: "backup-1 down".to_string(),
                                data: None,
                            })
                        }
                        McpRequest::CallTool(params) if params.name.starts_with("backup-2/") => {
                            Ok(McpResponse::CallTool(CallToolResult::text("from backup-2")))
                        }
                        _ => Ok(McpResponse::Pong(Default::default())),
                    };
                    Ok(RouterResponse { id, inner })
                })
            }
        }

        let failovers = [(
            "primary".to_string(),
            vec!["backup-1".to_string(), "backup-2".to_string()],
        )]
        .into_iter()
        .collect();
        let mut svc = FailoverService::new(ChainMock, failovers, "/");

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "primary/tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "from backup-2");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_failover_chain_all_fail_returns_last_error() {
        use std::convert::Infallible;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tower::Service;
        use tower_mcp::router::{RouterRequest, RouterResponse};

        #[derive(Clone)]
        struct AllFailMock;

        impl Service<RouterRequest> for AllFailMock {
            type Response = RouterResponse;
            type Error = Infallible;
            type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: RouterRequest) -> Self::Future {
                let id = req.id.clone();
                Box::pin(async move {
                    let inner = match &req.inner {
                        McpRequest::CallTool(params) => Err(tower_mcp_types::JsonRpcError {
                            code: -32603,
                            message: format!("{} down", params.name),
                            data: None,
                        }),
                        _ => Ok(McpResponse::Pong(Default::default())),
                    };
                    Ok(RouterResponse { id, inner })
                })
            }
        }

        let failovers = [(
            "primary".to_string(),
            vec!["backup-1".to_string(), "backup-2".to_string()],
        )]
        .into_iter()
        .collect();
        let mut svc = FailoverService::new(AllFailMock, failovers, "/");

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "primary/tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        // Should get the last failover's error
        let err = resp.inner.unwrap_err();
        assert!(
            err.message.contains("backup-2"),
            "expected last failover error, got: {}",
            err.message
        );
    }
}
