//! Role-based access control middleware for the gateway.
//!
//! Reads JWT claims from `RouterRequest.extensions`, maps them to roles
//! via config, and applies per-role tool allow/deny lists.
//! Runs on top of static capability filtering (can only further restrict).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use tower_mcp::protocol::{McpRequest, McpResponse};
use tower_mcp::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use crate::config::{RoleConfig, RoleMappingConfig};

/// Resolved RBAC rules.
#[derive(Clone)]
pub struct RbacConfig {
    /// Claim name to read from TokenClaims (e.g. "scope", "role")
    claim: String,
    /// Map of claim value -> role name
    claim_to_role: HashMap<String, String>,
    /// Map of role name -> allowed tools (empty = all allowed)
    role_allow: HashMap<String, HashSet<String>>,
    /// Map of role name -> denied tools
    role_deny: HashMap<String, HashSet<String>>,
}

impl RbacConfig {
    pub fn new(roles: &[RoleConfig], mapping: &RoleMappingConfig) -> Self {
        let mut role_allow = HashMap::new();
        let mut role_deny = HashMap::new();

        for role in roles {
            if !role.allow_tools.is_empty() {
                role_allow.insert(
                    role.name.clone(),
                    role.allow_tools.iter().cloned().collect(),
                );
            }
            if !role.deny_tools.is_empty() {
                role_deny.insert(role.name.clone(), role.deny_tools.iter().cloned().collect());
            }
        }

        Self {
            claim: mapping.claim.clone(),
            claim_to_role: mapping.mapping.clone(),
            role_allow,
            role_deny,
        }
    }

    /// Resolve the role for the current request from TokenClaims.
    fn resolve_role(&self, extensions: &tower_mcp::router::Extensions) -> Option<String> {
        let claims = extensions.get::<tower_mcp::oauth::token::TokenClaims>()?;

        // Check standard scope field first
        if self.claim == "scope" {
            let scopes = claims.scopes();
            for scope in &scopes {
                if let Some(role) = self.claim_to_role.get(scope) {
                    return Some(role.clone());
                }
            }
            return None;
        }

        // Check extra claims
        if let Some(value) = claims.extra.get(&self.claim) {
            let claim_str = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Try direct mapping
            if let Some(role) = self.claim_to_role.get(&claim_str) {
                return Some(role.clone());
            }
            // Try space-delimited (like scope)
            for part in claim_str.split_whitespace() {
                if let Some(role) = self.claim_to_role.get(part) {
                    return Some(role.clone());
                }
            }
        }

        None
    }

    /// Check if a tool is allowed for the given role.
    fn is_tool_allowed(&self, role: &str, tool_name: &str) -> bool {
        // If role has an allowlist, tool must be in it
        if let Some(allowed) = self.role_allow.get(role)
            && !allowed.contains(tool_name)
        {
            return false;
        }
        // If role has a denylist, tool must not be in it
        if let Some(denied) = self.role_deny.get(role)
            && denied.contains(tool_name)
        {
            return false;
        }
        true
    }
}

/// Middleware that enforces RBAC on tool calls and list responses.
#[derive(Clone)]
pub struct RbacService<S> {
    inner: S,
    config: Arc<RbacConfig>,
}

impl<S> RbacService<S> {
    pub fn new(inner: S, config: RbacConfig) -> Self {
        Self {
            inner,
            config: Arc::new(config),
        }
    }
}

impl<S> Service<RouterRequest> for RbacService<S>
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

        // Resolve role from extensions
        let role = config.resolve_role(&req.extensions);

        // If no role resolved, pass through (no RBAC restriction applies)
        // This allows unauthenticated or bearer-auth requests to proceed
        // (they're already validated by the auth layer)
        let Some(role) = role else {
            let fut = self.inner.call(req);
            return Box::pin(fut);
        };

        let role_for_filter = role.clone();

        // Check tool calls against RBAC
        if let McpRequest::CallTool(ref params) = req.inner
            && !config.is_tool_allowed(&role, &params.name)
        {
            let tool_name = params.name.clone();
            return Box::pin(async move {
                Ok(RouterResponse {
                    id: request_id,
                    inner: Err(JsonRpcError::invalid_params(format!(
                        "Role '{}' is not authorized to call tool: {}",
                        role, tool_name
                    ))),
                })
            });
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut resp = fut.await?;

            // Filter list_tools response based on role
            if let Ok(McpResponse::ListTools(ref mut result)) = resp.inner {
                result
                    .tools
                    .retain(|tool| config.is_tool_allowed(&role_for_filter, &tool.name));
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
    use tower_mcp::protocol::{McpRequest, McpResponse, RequestId};
    use tower_mcp::router::Extensions;

    use super::{RbacConfig, RbacService};
    use crate::config::{RoleConfig, RoleMappingConfig};
    use crate::test_util::MockService;

    fn test_rbac_config() -> RbacConfig {
        let roles = vec![
            RoleConfig {
                name: "admin".into(),
                allow_tools: vec![],
                deny_tools: vec![],
            },
            RoleConfig {
                name: "reader".into(),
                allow_tools: vec!["fs/read".into()],
                deny_tools: vec![],
            },
        ];
        let mapping = RoleMappingConfig {
            claim: "scope".into(),
            mapping: HashMap::from([
                ("admin".into(), "admin".into()),
                ("read-only".into(), "reader".into()),
            ]),
        };
        RbacConfig::new(&roles, &mapping)
    }

    fn request_with_scope(scope: &str, inner: McpRequest) -> tower_mcp::RouterRequest {
        let mut extensions = Extensions::new();
        extensions.insert(TokenClaims {
            sub: None,
            iss: None,
            aud: None,
            exp: None,
            scope: Some(scope.to_string()),
            client_id: None,
            extra: HashMap::new(),
        });
        tower_mcp::RouterRequest {
            id: RequestId::Number(1),
            inner,
            extensions,
        }
    }

    #[tokio::test]
    async fn test_rbac_admin_can_call_any_tool() {
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        let req = request_with_scope(
            "admin",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok(), "admin should call any tool");
    }

    #[tokio::test]
    async fn test_rbac_reader_denied_write() {
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        let req = request_with_scope(
            "read-only",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        let err = resp.inner.unwrap_err();
        assert!(err.message.contains("not authorized"));
    }

    #[tokio::test]
    async fn test_rbac_reader_allowed_read() {
        let mock = MockService::with_tools(&["fs/read"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        let req = request_with_scope(
            "read-only",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/read".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok(), "reader should call allowed tools");
    }

    #[tokio::test]
    async fn test_rbac_filters_list_tools_for_role() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "fs/delete"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        let req = request_with_scope("read-only", McpRequest::ListTools(Default::default()));
        let resp = svc.call(req).await.unwrap();

        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"fs/read"));
                assert!(!names.contains(&"fs/write"));
                assert!(!names.contains(&"fs/delete"));
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_rbac_no_claims_passes_through() {
        let mock = MockService::with_tools(&["fs/write"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        // No TokenClaims in extensions
        let req = tower_mcp::RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = svc.call(req).await.unwrap();
        assert!(resp.inner.is_ok(), "no claims should pass through");
    }
}
