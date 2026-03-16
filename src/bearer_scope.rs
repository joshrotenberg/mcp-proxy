//! Per-token tool scoping for bearer token authentication.
//!
//! When scoped bearer tokens are configured, this module provides:
//! - An Axum middleware that identifies which scoped token was used and
//!   injects scope info via [`TokenClaims`] into request extensions.
//! - An MCP middleware that reads scope info from extensions and enforces
//!   tool allow/deny lists per token.
//!
//! # Architecture
//!
//! tower-mcp's HTTP transport only bridges [`TokenClaims`] from Axum
//! extensions to MCP extensions. To pass bearer scope info across this
//! boundary, the Axum middleware inserts synthetic `TokenClaims` with
//! scope details in the `extra` map (key: `__bearer_scope`).
//!
//! The MCP-level [`BearerScopingService`] reads this marker and applies
//! the matching token's allow/deny rules.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::oauth::token::TokenClaims;
use tower_mcp::protocol::{McpRequest, McpResponse};
use tower_mcp::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use crate::config::BearerTokenConfig;

/// Key used in `TokenClaims.extra` to store bearer scope info.
const BEARER_SCOPE_KEY: &str = "__bearer_scope";

// ---------------------------------------------------------------------------
// Axum middleware: inject TokenClaims with bearer scope info
// ---------------------------------------------------------------------------

/// Axum middleware layer that validates bearer tokens and injects scope info.
///
/// For scoped tokens, inserts synthetic [`TokenClaims`] into request
/// extensions so tower-mcp's HTTP transport propagates them to MCP
/// extensions. Unscoped tokens pass through without `TokenClaims`.
#[derive(Clone)]
pub struct ScopedBearerAuthLayer {
    inner: Arc<ScopedBearerAuthState>,
}

struct ScopedBearerAuthState {
    /// All valid tokens (for validation)
    valid_tokens: HashSet<String>,
    /// Token -> scope JSON (only for scoped tokens)
    scopes: HashMap<String, serde_json::Value>,
}

impl ScopedBearerAuthLayer {
    /// Build from combined simple + scoped token lists.
    pub fn new(simple_tokens: &[String], scoped_tokens: &[BearerTokenConfig]) -> Self {
        let mut valid_tokens = HashSet::new();
        let mut scopes = HashMap::new();

        for t in simple_tokens {
            valid_tokens.insert(t.clone());
        }

        for st in scoped_tokens {
            valid_tokens.insert(st.token.clone());
            // Build scope JSON for this token
            let scope = serde_json::json!({
                "allow": st.allow_tools,
                "deny": st.deny_tools,
            });
            scopes.insert(st.token.clone(), scope);
        }

        Self {
            inner: Arc::new(ScopedBearerAuthState {
                valid_tokens,
                scopes,
            }),
        }
    }
}

impl<S> tower::Layer<S> for ScopedBearerAuthLayer {
    type Service = ScopedBearerAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ScopedBearerAuthService {
            inner,
            state: Arc::clone(&self.inner),
        }
    }
}

/// Axum service that validates bearer tokens and injects scope info.
#[derive(Clone)]
pub struct ScopedBearerAuthService<S> {
    inner: S,
    state: Arc<ScopedBearerAuthState>,
}

impl<S> Service<axum::http::Request<axum::body::Body>> for ScopedBearerAuthService<S>
where
    S: Service<axum::http::Request<axum::body::Body>, Response = axum::response::Response>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
    S::Error: Into<tower_mcp::BoxError> + Send,
{
    type Response = axum::response::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        let token = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|s| s.trim().to_owned());

        let state = Arc::clone(&self.state);
        let inner = self.inner.clone();

        Box::pin(async move {
            let Some(token) = token else {
                return Ok(unauthorized_response("Missing bearer token"));
            };

            if !state.valid_tokens.contains(&token) {
                return Ok(unauthorized_response("Invalid bearer token"));
            }

            let mut req = req;

            // If this is a scoped token, inject TokenClaims with scope info
            if let Some(scope) = state.scopes.get(&token) {
                let mut extra = HashMap::new();
                extra.insert(BEARER_SCOPE_KEY.to_string(), scope.clone());
                let claims = TokenClaims {
                    sub: None,
                    iss: None,
                    aud: None,
                    exp: None,
                    scope: None,
                    client_id: None,
                    extra,
                };
                req.extensions_mut().insert(claims);
            }

            tower::ServiceExt::oneshot(inner, req).await
        })
    }
}

/// Construct an HTTP 401 Unauthorized response.
fn unauthorized_response(message: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": -32001,
            "message": message
        },
        "id": null
    });

    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// MCP middleware: enforce per-token tool scoping
// ---------------------------------------------------------------------------

/// Resolved bearer token scope (allow/deny tool sets).
#[derive(Debug, Clone)]
struct ResolvedScope {
    allow: HashSet<String>,
    deny: HashSet<String>,
}

impl ResolvedScope {
    /// Parse scope from the `TokenClaims.extra` map.
    fn from_claims(claims: &TokenClaims) -> Option<Self> {
        let scope_val = claims.extra.get(BEARER_SCOPE_KEY)?;

        let allow: HashSet<String> = scope_val
            .get("allow")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let deny: HashSet<String> = scope_val
            .get("deny")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // If both are empty, this is an unscoped token
        if allow.is_empty() && deny.is_empty() {
            return None;
        }

        Some(Self { allow, deny })
    }

    /// Check if a tool is allowed under this scope.
    fn is_tool_allowed(&self, tool_name: &str) -> bool {
        if !self.allow.is_empty() && !self.allow.contains(tool_name) {
            return false;
        }
        if self.deny.contains(tool_name) {
            return false;
        }
        true
    }
}

/// MCP middleware that enforces per-bearer-token tool access control.
///
/// Reads scope info from `TokenClaims.extra` (injected by [`ScopedBearerAuthLayer`])
/// and applies allow/deny lists to tool calls and list responses.
#[derive(Clone)]
pub struct BearerScopingService<S> {
    inner: S,
}

impl<S> BearerScopingService<S> {
    /// Wrap an inner MCP service with bearer scoping enforcement.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<RouterRequest> for BearerScopingService<S>
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
        let request_id = req.id.clone();

        // Try to extract bearer scope from extensions
        let scope = req
            .extensions
            .get::<TokenClaims>()
            .and_then(ResolvedScope::from_claims);

        // No scope = unscoped token or no auth; pass through
        let Some(scope) = scope else {
            let fut = self.inner.call(req);
            return Box::pin(fut);
        };

        // Check tool calls against scope
        if let McpRequest::CallTool(ref params) = req.inner
            && !scope.is_tool_allowed(&params.name)
        {
            let tool_name = params.name.clone();
            return Box::pin(async move {
                Ok(RouterResponse {
                    id: request_id,
                    inner: Err(JsonRpcError::invalid_params(format!(
                        "Token is not authorized to call tool: {tool_name}"
                    ))),
                })
            });
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut resp = fut.await?;

            // Filter list_tools response
            if let Ok(McpResponse::ListTools(ref mut result)) = resp.inner {
                result
                    .tools
                    .retain(|tool| scope.is_tool_allowed(&tool.name));
            }

            Ok(resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tower::Service;
    use tower_mcp::oauth::token::TokenClaims;
    use tower_mcp::protocol::{
        CallToolParams, ListToolsParams, McpRequest, McpResponse, RequestId,
    };
    use tower_mcp::router::Extensions;

    use super::{BEARER_SCOPE_KEY, BearerScopingService};
    use crate::test_util::{MockService, call_service};

    fn request_with_bearer_scope(
        allow: &[&str],
        deny: &[&str],
        inner: McpRequest,
    ) -> tower_mcp::RouterRequest {
        let mut extra = HashMap::new();
        extra.insert(
            BEARER_SCOPE_KEY.to_string(),
            serde_json::json!({
                "allow": allow,
                "deny": deny,
            }),
        );
        let mut extensions = Extensions::new();
        extensions.insert(TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: None,
            client_id: None,
            extra,
        });
        tower_mcp::RouterRequest {
            id: RequestId::Number(1),
            inner,
            extensions,
        }
    }

    #[tokio::test]
    async fn no_scope_passes_through() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "db/query"]);
        let mut svc = BearerScopingService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(ListToolsParams::default())).await;
        let tools = match resp.inner.unwrap() {
            McpResponse::ListTools(r) => r.tools,
            other => panic!("expected ListTools, got: {other:?}"),
        };
        assert_eq!(tools.len(), 3);
    }

    #[tokio::test]
    async fn allow_list_filters_tools() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "db/query"]);
        let mut svc = BearerScopingService::new(mock);

        let req = request_with_bearer_scope(
            &["fs/read"],
            &[],
            McpRequest::ListTools(ListToolsParams::default()),
        );
        let resp = svc.call(req).await.unwrap();
        let tools = match resp.inner.unwrap() {
            McpResponse::ListTools(r) => r.tools,
            other => panic!("expected ListTools, got: {other:?}"),
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "fs/read");
    }

    #[tokio::test]
    async fn deny_list_filters_tools() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "db/query"]);
        let mut svc = BearerScopingService::new(mock);

        let req = request_with_bearer_scope(
            &[],
            &["fs/write"],
            McpRequest::ListTools(ListToolsParams::default()),
        );
        let resp = svc.call(req).await.unwrap();
        let tools = match resp.inner.unwrap() {
            McpResponse::ListTools(r) => r.tools,
            other => panic!("expected ListTools, got: {other:?}"),
        };
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t.name != "fs/write"));
    }

    #[tokio::test]
    async fn allow_list_blocks_call() {
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = BearerScopingService::new(mock);

        let req = request_with_bearer_scope(
            &["fs/read"],
            &[],
            McpRequest::CallTool(CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_err(), "should block disallowed tool call");
        let err = resp.inner.unwrap_err();
        assert!(err.message.contains("fs/write"));
    }

    #[tokio::test]
    async fn allow_list_permits_call() {
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = BearerScopingService::new(mock);

        let req = request_with_bearer_scope(
            &["fs/read"],
            &[],
            McpRequest::CallTool(CallToolParams {
                name: "fs/read".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok(), "should allow permitted tool call");
    }

    #[tokio::test]
    async fn deny_list_blocks_call() {
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = BearerScopingService::new(mock);

        let req = request_with_bearer_scope(
            &[],
            &["fs/write"],
            McpRequest::CallTool(CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_err(), "should block denied tool call");
    }
}
