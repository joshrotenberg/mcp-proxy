//! Legacy identity-metadata helper; this module does not forward credentials.
//!
//! [`TokenPassthroughService`] copies subject/scope from existing OAuth
//! [`TokenClaims`](tower_mcp::oauth::token::TokenClaims) into [`ClientToken`] for
//! selected namespaces. It never obtains a raw bearer token; `raw_token` is
//! always None for metadata it creates. Neither HTTP nor WebSocket transports
//! consume this custom extension. Static bearer authentication does not create
//! these OAuth claims.
//!
//! This low-level API remains for source compatibility with custom service
//! stacks. It is not installed by the configured proxy. The configuration field
//! `forward_auth = true` is rejected in every build; see [backend authentication
//! and migration](crate::config#backend-authentication-and-migration). It does not
//! define a trust/audience policy or delegated identity isolation for routing,
//! backend sessions, caches or coalescing. Custom callers must not treat its
//! metadata as credential delegation or as independent proof of authentication.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

/// Legacy subject/scope metadata copied from existing OAuth claims.
///
/// Stored in `RouterRequest.extensions` by the [`TokenPassthroughService`]
/// for custom services; configured transports do not consume it.
#[derive(Debug, Clone)]
pub struct ClientToken {
    /// The subject (user/client identifier) from the token.
    pub subject: Option<String>,
    /// Space-delimited scopes from the token.
    pub scope: Option<String>,
    /// Reserved for source compatibility; this helper always sets it to None.
    pub raw_token: Option<String>,
}

/// Legacy middleware that copies identity metadata for selected namespaces.
///
/// Does not forward tokens and is not installed by the configured proxy.
#[derive(Clone)]
pub struct TokenPassthroughService<S> {
    inner: S,
    forward_namespaces: Arc<HashSet<String>>,
}

impl<S> TokenPassthroughService<S> {
    /// Create an identity-metadata helper for a custom service stack.
    ///
    /// `forward_namespaces` is the set of backend namespace prefixes (e.g. `"github/"`)
    /// for which subject/scope metadata should be copied.
    pub fn new(inner: S, forward_namespaces: HashSet<String>) -> Self {
        Self {
            inner,
            forward_namespaces: Arc::new(forward_namespaces),
        }
    }
}

/// Check if a request targets a namespace selected for identity metadata.
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
        // Copy metadata only for explicitly selected namespaces.
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
                    "Copied legacy client identity metadata"
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

    use std::sync::{Arc, Mutex};

    use tower::{Service, ServiceExt};
    use tower_mcp::protocol::{CallToolParams, McpRequest, RequestId};
    use tower_mcp::router::{Extensions, RouterRequest};

    use super::{ClientToken, TokenPassthroughService, request_targets_namespace};
    use crate::test_util::{MockService, call_service};

    #[test]
    fn test_request_targets_namespace_match() {
        let namespaces: HashSet<String> = ["github/".to_string()].into();
        let req = McpRequest::CallTool(CallToolParams {
            name: "github/search".to_string(),
            arguments: serde_json::json!({}),
            input_responses: None,
            request_state: None,
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
            input_responses: None,
            request_state: None,
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
    async fn legacy_helper_copies_only_subject_scope_and_never_a_raw_token() {
        let captured = Arc::new(Mutex::new(None));
        let received = captured.clone();
        let mock = tower::service_fn(move |request: RouterRequest| {
            *received.lock().unwrap() = request.extensions.get::<ClientToken>().cloned();
            MockService::with_tools(&["github/search"]).oneshot(request)
        });
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
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions,
        };

        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok());
        let metadata = captured.lock().unwrap().take().expect("identity metadata");
        assert_eq!(metadata.subject.as_deref(), Some("user-123"));
        assert_eq!(metadata.scope.as_deref(), Some("mcp:read"));
        assert!(metadata.raw_token.is_none());
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }
}
