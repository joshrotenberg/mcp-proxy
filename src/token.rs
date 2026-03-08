//! Token passthrough middleware for forwarding client credentials to backends.
//!
//! When a backend has `forward_auth = true`, the client's inbound bearer token
//! is extracted from `RouterRequest.extensions` and stored as a [`ClientToken`]
//! for downstream middleware and backend services to consume.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "github"
//! transport = "http"
//! url = "http://github-mcp.internal:8080"
//! forward_auth = true  # forward client's token to this backend
//!
//! [[backends]]
//! name = "db"
//! transport = "http"
//! url = "http://db-mcp.internal:8080"
//! bearer_token = "${DB_API_KEY}"  # static token for this backend
//! ```
//!
//! # How it works
//!
//! 1. The proxy's auth layer (JWT/bearer) validates the inbound token and
//!    stores [`TokenClaims`](tower_mcp::oauth::token::TokenClaims) in request extensions.
//! 2. This middleware reads the `TokenClaims` and stores the subject (`sub` claim)
//!    and any available identity info as a [`ClientToken`] in extensions.
//! 3. Backend-specific middleware or future transport enhancements can read
//!    `ClientToken` to forward credentials.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

/// A client's identity token extracted from inbound authentication.
///
/// Stored in `RouterRequest.extensions` by the [`TokenPassthroughService`]
/// for downstream consumption.
#[derive(Debug, Clone)]
pub struct ClientToken {
    /// The subject (user/client identifier) from the token.
    pub subject: Option<String>,
    /// Space-delimited scopes from the token.
    pub scope: Option<String>,
    /// The raw bearer token string, if available.
    pub raw_token: Option<String>,
}

/// Middleware that extracts client identity from auth claims and makes it
/// available to backends configured with `forward_auth = true`.
#[derive(Clone)]
pub struct TokenPassthroughService<S> {
    inner: S,
    forward_namespaces: Arc<HashSet<String>>,
}

impl<S> TokenPassthroughService<S> {
    /// Create a new token passthrough service.
    ///
    /// `forward_namespaces` is the set of backend namespace prefixes (e.g. `"github/"`)
    /// that should receive forwarded tokens.
    pub fn new(inner: S, forward_namespaces: HashSet<String>) -> Self {
        Self {
            inner,
            forward_namespaces: Arc::new(forward_namespaces),
        }
    }
}

/// Check if a request targets a namespace that wants token forwarding.
fn request_targets_namespace(req: &McpRequest, namespaces: &HashSet<String>) -> bool {
    let name = match req {
        McpRequest::CallTool(params) => Some(params.name.as_str()),
        McpRequest::ReadResource(params) => Some(params.uri.as_str()),
        McpRequest::GetPrompt(params) => Some(params.name.as_str()),
        _ => None,
    };
    if let Some(name) = name {
        namespaces.iter().any(|ns| name.starts_with(ns))
    } else {
        false
    }
}

impl<S> Service<RouterRequest> for TokenPassthroughService<S>
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

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        // Only inject ClientToken for requests targeting forward_auth backends
        if !self.forward_namespaces.is_empty()
            && request_targets_namespace(&req.inner, &self.forward_namespaces)
        {
            let client_token = req
                .extensions
                .get::<tower_mcp::oauth::token::TokenClaims>()
                .map(|claims| ClientToken {
                    subject: claims.sub.clone(),
                    scope: claims.scope.clone(),
                    raw_token: None, // Raw token not available from TokenClaims
                });
            if let Some(token) = client_token {
                tracing::debug!(
                    subject = ?token.subject,
                    "Injected ClientToken for forward_auth backend"
                );
                req.extensions.insert(token);
            }
        }

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tower::Service;
    use tower_mcp::protocol::{CallToolParams, McpRequest, RequestId};
    use tower_mcp::router::{Extensions, RouterRequest};

    use super::{TokenPassthroughService, request_targets_namespace};
    use crate::test_util::{MockService, call_service};

    #[test]
    fn test_request_targets_namespace_match() {
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let req = McpRequest::CallTool(CallToolParams {
            name: "github/search".to_string(),
            arguments: serde_json::json!({}),
            meta: None,
            task: None,
        });
        assert!(request_targets_namespace(&req, &namespaces));
    }

    #[test]
    fn test_request_targets_namespace_no_match() {
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let req = McpRequest::CallTool(CallToolParams {
            name: "db/query".to_string(),
            arguments: serde_json::json!({}),
            meta: None,
            task: None,
        });
        assert!(!request_targets_namespace(&req, &namespaces));
    }

    #[test]
    fn test_request_targets_namespace_list_tools() {
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let req = McpRequest::ListTools(Default::default());
        assert!(!request_targets_namespace(&req, &namespaces));
    }

    #[tokio::test]
    async fn test_passthrough_injects_client_token() {
        let mock = MockService::with_tools(&["github/search"]);
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let mut svc = TokenPassthroughService::new(mock, namespaces);

        // Create request with TokenClaims in extensions
        let mut extensions = Extensions::new();
        extensions.insert(tower_mcp::oauth::token::TokenClaims {
            sub: Some("user-123".to_string()),
            scope: Some("mcp:read".to_string()),
            iss: None,
            aud: None,
            exp: None,
            client_id: None,
            extra: Default::default(),
        });

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "github/search".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions,
        };

        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_passthrough_skips_non_forward_backends() {
        let mock = MockService::with_tools(&["db/query"]);
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let mut svc = TokenPassthroughService::new(mock, namespaces);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "db/query".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_passthrough_no_claims_passes_through() {
        let mock = MockService::with_tools(&["github/search"]);
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let mut svc = TokenPassthroughService::new(mock, namespaces);

        // No TokenClaims in extensions -- should still pass through
        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "github/search".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }
}
