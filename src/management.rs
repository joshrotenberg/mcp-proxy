//! Explicit management authority, independent of data-plane authentication.
//!
//! Only server-verified HTTP extensions cross into MCP. Administrative requests
//! use a separate service, outside data-plane caches, coalescing, and policies.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use tower::{Service, ServiceExt};
use tower_mcp::protocol::{CompletionReference, McpRequest, McpResponse};
use tower_mcp::{Extensions, RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use crate::config::{AuthConfig, ProxyConfig};

/// Verified credential identity; contains no credential material.
#[derive(Clone)]
pub(crate) enum Identity {
    Management,
    Bearer(usize),
    Scoped(usize),
}

/// The management credential never falls back to a data-plane token. Reject
/// collisions even for callers constructing configuration without validation.
pub(crate) fn admin_token(config: &ProxyConfig) -> Option<&str> {
    let token = config.security.admin_token.as_deref()?;
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    if let Some(AuthConfig::Bearer {
        tokens,
        scoped_tokens,
    }) = &config.auth
        && (tokens.iter().any(|t| t == token) || scoped_tokens.iter().any(|t| t.token == token))
    {
        return None;
    }
    Some(token)
}

pub(crate) fn validate(config: &ProxyConfig) -> anyhow::Result<()> {
    if config.security.admin_token.is_some() && admin_token(config).is_none() {
        anyhow::bail!(
            "security.admin_token must be a nonempty ASCII token without whitespace and differ from all ordinary and scoped bearer tokens"
        );
    }
    Ok(())
}

/// Preserve OAuth ownership while separating static credentials and authority.
pub(crate) fn session_principal(extensions: &Extensions) -> Option<String> {
    if let Some(identity) = extensions.get::<Identity>() {
        return Some(match identity {
            Identity::Management => "mcp-proxy:management".to_string(),
            Identity::Bearer(index) => format!("mcp-proxy:bearer:{index}"),
            Identity::Scoped(index) => format!("mcp-proxy:scoped:{index}"),
        });
    }
    #[cfg(feature = "oauth")]
    if let Some(claims) = extensions.get::<tower_mcp::oauth::TokenClaims>() {
        let (kind, owner) = if let Some(subject) = &claims.sub {
            ("oauth", Some(subject))
        } else {
            ("oauth-client", claims.client_id.as_ref())
        };
        return Some(match owner.filter(|owner| !owner.trim().is_empty()) {
            Some(owner) => serde_json::json!([kind, claims.iss, owner]).to_string(),
            // The transport rejects an empty principal. Returning None would
            // incorrectly collapse an authenticated caller to anonymous.
            None => String::new(),
        });
    }
    None
}

/// Authenticate the separate admin credential before data-plane auth. The
/// unauthed transport clone shares sessions and receives only verified admins.
pub(crate) fn authenticate(
    config: &ProxyConfig,
    transport: Router,
    authenticated: Router,
) -> Router {
    // Middleware clones share one lookup table, not copies of every secret.
    let mut identities = std::collections::HashMap::new();
    if let Some(AuthConfig::Bearer {
        tokens,
        scoped_tokens,
    }) = &config.auth
    {
        for (index, token) in tokens.iter().enumerate() {
            identities.insert(token.clone(), Identity::Bearer(index));
        }
        for (index, token) in scoped_tokens.iter().enumerate() {
            identities.insert(token.token.clone(), Identity::Scoped(index));
        }
    }
    if let Some(token) = admin_token(config) {
        identities.insert(token.to_owned(), Identity::Management);
    }
    let identities = std::sync::Arc::new(identities);
    authenticated.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let token = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(tower_mcp::auth::extract_api_key);
            let identity = token.and_then(|token| identities.get(token).cloned());
            let is_admin = matches!(identity, Some(Identity::Management));
            // Never trust a previously installed value on an ordinary request.
            request.extensions_mut().remove::<Identity>();
            if let Some(identity) = identity {
                request.extensions_mut().insert(identity);
            }
            let transport = transport.clone();
            async move {
                if is_admin {
                    transport
                        .oneshot(request)
                        .await
                        .unwrap_or_else(|never| match never {})
                } else {
                    next.run(request).await
                }
            }
        },
    ))
}

fn management_tool(prefix: &str, name: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|local| !matches!(local, "search_tools" | "get_tool" | "call_tool"))
}

fn management_request(prefix: &str, request: &McpRequest) -> bool {
    match request {
        McpRequest::CallTool(p) => management_tool(prefix, &p.name),
        McpRequest::GetPrompt(p) => p.name.starts_with(prefix),
        McpRequest::ReadResource(p) => p.uri.starts_with(prefix),
        McpRequest::SubscribeResource(p) => p.uri.starts_with(prefix),
        McpRequest::UnsubscribeResource(p) => p.uri.starts_with(prefix),
        McpRequest::Complete(p) => match &p.reference {
            CompletionReference::Prompt { name } => name.starts_with(prefix),
            CompletionReference::Resource { uri } => uri.starts_with(prefix),
            _ => false,
        },
        _ => false,
    }
}

/// Guard both the external service and the innermost backend dispatch. The
/// inner guard stops composite/alias indirection; the outer guard precedes
/// caches and coalescing. `only_management` is used by the admin-only service.
#[derive(Clone)]
pub(crate) struct Guard<S> {
    inner: S,
    prefix: String,
    only_management: bool,
}

impl<S> Guard<S> {
    pub(crate) fn new(inner: S, separator: &str, only_management: bool) -> Self {
        Self {
            inner,
            prefix: format!("proxy{separator}"),
            only_management,
        }
    }
}

impl<S> Service<RouterRequest> for Guard<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RouterRequest) -> Self::Future {
        let management = management_request(&self.prefix, &request.inner);
        let allowed = if self.only_management {
            management
                || matches!(
                    &request.inner,
                    McpRequest::Initialize(_)
                        | McpRequest::Ping
                        | McpRequest::ListTools(_)
                        | McpRequest::ListPrompts(_)
                        | McpRequest::ListResources(_)
                        | McpRequest::ListResourceTemplates(_)
                )
        } else {
            !management
        };
        if !allowed {
            let message = if self.only_management {
                "Use an ordinary credential for backend capabilities"
            } else {
                "Explicit management authority is required"
            };
            return Box::pin(async move {
                Ok(RouterResponse {
                    id: request.id,
                    inner: Err(JsonRpcError::invalid_params(message)),
                })
            });
        }
        let future = self.inner.call(request);
        let prefix = self.prefix.clone();
        let only_management = self.only_management;
        Box::pin(async move {
            let mut response = future.await?;
            if let Ok(result) = &mut response.inner {
                match result {
                    McpResponse::ListTools(r) => r
                        .tools
                        .retain(|t| management_tool(&prefix, &t.name) == only_management),
                    McpResponse::ListPrompts(r) => r
                        .prompts
                        .retain(|p| p.name.starts_with(&prefix) == only_management),
                    McpResponse::ListResources(r) => r
                        .resources
                        .retain(|r| r.uri.starts_with(&prefix) == only_management),
                    McpResponse::ListResourceTemplates(r) => r
                        .resource_templates
                        .retain(|r| r.uri_template.starts_with(&prefix) == only_management),
                    _ => {}
                }
            }
            Ok(response)
        })
    }
}

/// Select a service using server-verified, per-request authority.
#[derive(Clone)]
pub(crate) struct Dispatch<D, A> {
    data: D,
    admin: A,
}

impl<D, A> Dispatch<D, A> {
    pub(crate) fn new(data: D, admin: A) -> Self {
        Self { data, admin }
    }
}

impl<D, A> Service<RouterRequest> for Dispatch<D, A>
where
    D: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    A: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    D::Future: Send + 'static,
    A::Future: Send + 'static,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: RouterRequest) -> Self::Future {
        if matches!(
            request.extensions.get::<Identity>(),
            Some(Identity::Management)
        ) {
            let service = self.admin.clone();
            Box::pin(service.oneshot(request))
        } else {
            let service = self.data.clone();
            Box::pin(service.oneshot(request))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower_mcp::protocol::RequestId;

    fn config() -> ProxyConfig {
        ProxyConfig::parse("[proxy]\nname='test'\n[proxy.listen]\n[[backends]]\nname='api'\ntransport='stdio'\ncommand='fixture'\n").unwrap()
    }

    #[test]
    fn management_credentials_must_be_distinct_even_after_programmatic_resolution() {
        let mut config = config();
        config.security.admin_token = Some("sentinel-shared-credential".into());
        config.auth = Some(AuthConfig::Bearer {
            tokens: vec!["sentinel-shared-credential".into()],
            scoped_tokens: vec![],
        });
        assert!(admin_token(&config).is_none());
        let error = validate(&config).unwrap_err();
        assert!(!error.to_string().contains("sentinel-shared-credential"));
        config.auth = Some(AuthConfig::Bearer {
            tokens: vec![],
            scoped_tokens: vec![crate::config::BearerTokenConfig {
                token: "sentinel-shared-credential".into(),
                allow_tools: vec!["api/ping".into()],
                deny_tools: vec![],
            }],
        });
        assert!(validate(&config).is_err());
        for invalid in [
            "",
            " ",
            "bad credential",
            "bad\ncredential",
            "non-ASCII-é",
            "bad\u{7f}",
        ] {
            config.security.admin_token = Some(invalid.into());
            assert!(validate(&config).is_err());
        }
        config.security.admin_token = Some("distinct-management-credential".into());
        assert!(validate(&config).is_ok());
        config.security.admin_token = None;
        assert!(admin_token(&config).is_none());
        assert!(validate(&config).is_ok());
    }

    #[tokio::test]
    async fn resource_templates_are_partitioned_and_completions_cannot_bypass_the_guard() {
        for only_management in [false, true] {
            let inner = tower::service_fn(|request: RouterRequest| async move {
                Ok::<_, Infallible>(RouterResponse {
                    id: request.id,
                    inner: Ok(McpResponse::ListResourceTemplates(
                        serde_json::from_value(json!({"resourceTemplates":[
                            {"uriTemplate":"proxy/file:///{path}","name":"private"},
                            {"uriTemplate":"api/file:///{path}","name":"ordinary"}
                        ]}))
                        .unwrap(),
                    )),
                })
            });
            let response = Guard::new(inner, "/", only_management)
                .oneshot(RouterRequest {
                    id: RequestId::Number(1),
                    inner: McpRequest::ListResourceTemplates(Default::default()),
                    extensions: Extensions::new(),
                })
                .await
                .unwrap();
            let Ok(McpResponse::ListResourceTemplates(list)) = response.inner else {
                panic!("expected templates")
            };
            assert_eq!(list.resource_templates.len(), 1);
            assert_eq!(
                list.resource_templates[0].name,
                if only_management {
                    "private"
                } else {
                    "ordinary"
                }
            );
        }
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let inner = tower::service_fn(move |request: RouterRequest| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok::<_, Infallible>(RouterResponse {
                    id: request.id,
                    inner: Ok(McpResponse::Pong(Default::default())),
                })
            }
        });
        for reference in [
            json!({"type":"ref/prompt","name":"proxy/explain_config"}),
            json!({"type":"ref/resource","uri":"proxy/state://private"}),
        ] {
            let response = Guard::new(inner.clone(), "/", false)
                .oneshot(RouterRequest {
                    id: RequestId::Number(2),
                    inner: McpRequest::Complete(
                        serde_json::from_value(
                            json!({"ref":reference,"argument":{"name":"key","value":""}}),
                        )
                        .unwrap(),
                    ),
                    extensions: Extensions::new(),
                })
                .await
                .unwrap();
            assert!(response.inner.is_err());
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[cfg(feature = "oauth")]
    #[test]
    fn oauth_client_credentials_have_distinct_owners_and_missing_identity_fails_closed() {
        fn principal(value: serde_json::Value) -> Option<String> {
            let claims: tower_mcp::oauth::TokenClaims = serde_json::from_value(value).unwrap();
            let mut extensions = Extensions::new();
            extensions.insert(claims);
            session_principal(&extensions)
        }
        let client_a = principal(json!({"iss":"issuer","client_id":"client-a"}));
        let client_b = principal(json!({"iss":"issuer","client_id":"client-b"}));
        assert!(client_a.as_ref().is_some_and(|owner| !owner.is_empty()));
        assert_ne!(client_a, client_b);
        assert_ne!(
            client_a,
            principal(json!({"iss":"other-issuer","client_id":"client-a"}))
        );
        assert_ne!(
            client_a,
            principal(json!({"iss":"issuer","sub":"client-a"}))
        );
        for claims in [json!({}), json!({"sub":" "}), json!({"client_id":""})] {
            assert_eq!(principal(claims).as_deref(), Some(""));
        }
        assert!(session_principal(&Extensions::new()).is_none());
    }

    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn oauth_claims_cannot_forge_typed_management_authority() {
        let mut extensions = Extensions::new();
        let claims: tower_mcp::oauth::TokenClaims = serde_json::from_value(json!({
            "sub":"mcp-proxy:management", "role":"admin", "__management":true,
        }))
        .unwrap();
        extensions.insert(claims);
        assert_ne!(
            session_principal(&extensions).as_deref(),
            Some("mcp-proxy:management")
        );
        let data = Guard::new(
            tower::service_fn(|request: RouterRequest| async {
                Ok::<_, Infallible>(RouterResponse {
                    id: request.id,
                    inner: Ok(McpResponse::Pong(Default::default())),
                })
            }),
            "/",
            false,
        );
        let admin = tower::service_fn(|_: RouterRequest| async {
            panic!("claims must never select administrative dispatch")
        });
        let response = Dispatch::new(data, admin)
            .oneshot(RouterRequest {
                id: RequestId::Number(1),
                inner: McpRequest::CallTool(
                    serde_json::from_value(json!({"name":"proxy/config","arguments":{}})).unwrap(),
                ),
                extensions,
            })
            .await
            .unwrap();
        assert!(response.inner.is_err());
    }
}
