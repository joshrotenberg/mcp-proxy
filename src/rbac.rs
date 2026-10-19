//! Role-based access control (RBAC) middleware for the proxy.
//!
//! This module enforces per-role tool access policies on authenticated requests.
//! It reads JWT claims from [`RouterRequest`] extensions, maps claim values to
//! named roles via a configurable mapping, and applies per-role allow/deny lists
//! to both `tools/call` and `tools/list` requests.
//!
//! # How role resolution works
//!
//! 1. The auth layer (JWT or introspection) validates the token and inserts
//!    [`TokenClaims`](tower_mcp::oauth::token::TokenClaims) into the request
//!    extensions.
//! 2. [`RbacConfig`] reads a configured claim (e.g. `"scope"`, `"role"`,
//!    `"groups"`) from those claims.
//! 3. All matching values resolve to a sorted, deduplicated set of roles.
//!    Scopes are space-delimited; custom claims accept strings or string arrays.
//!    A custom string uses an exact mapping when one exists, otherwise it is
//!    split on whitespace. Array elements use exact mappings.
//! 4. Grants combine across all resolved roles. An explicit deny in any role
//!    overrides every grant, including an unrestricted admin role.
//!
//! An installed RBAC layer rejects missing verified claims, malformed custom
//! claims (including mixed-type arrays), and undefined roles. Valid claims with
//! no mapped values follow `default_deny`: true rejects them, false preserves
//! legacy unrestricted access. Missing custom claims count as unmapped.
//! Configurations must define both unique named roles and a nonempty mapping
//! referencing those roles; invalid or incomplete policies fail construction.
//! The policy applies equally to JWT and opaque-token introspection claims.
//!
//! # Allow/deny list semantics
//!
//! Each role can define an allow list, a deny list, or both:
//!
//! - **Allow list only**: only the listed tools are accessible. All others are
//!   denied.
//! - **Deny list only**: all tools are accessible except those listed.
//! - **Both**: a tool must appear in the allow list AND not appear in the deny
//!   list.
//! - **Neither** (empty lists): the role has unrestricted access (e.g. an admin
//!   role).
//!
//! # Interaction with capability filtering
//!
//! RBAC runs **on top of** the static capability filter configured per backend.
//! The final set of visible tools is the **intersection** of what the backend
//! exposes and what the role permits -- RBAC can only further restrict, never
//! widen, the tools a client can see or call. The same combined policy filters
//! listings and authorizes calls, including search wrappers and composite targets.
//!
//! # Configuration example
//!
//! ```toml
//! [auth]
//! type = "jwt"
//! issuer = "https://auth.example.com"
//! audience = "https://mcp.example.com/mcp"
//! jwks_uri = "https://auth.example.com/.well-known/jwks.json"
//!
//! [[auth.roles]]
//! name = "admin"
//! # Empty allow/deny = unrestricted access
//!
//! [[auth.roles]]
//! name = "reader"
//! allow_tools = ["files/read_file", "files/list_dir"]
//!
//! [[auth.roles]]
//! name = "developer"
//! deny_tools = ["admin/restart", "admin/shutdown"]
//!
//! [auth.role_mapping]
//! claim = "scope"
//! default_deny = true
//!
//! [auth.role_mapping.mapping]
//! admin = "admin"
//! read-only = "reader"
//! dev = "developer"
//! ```
//!
//! # Enforcement
//!
//! [`RbacService`] is a Tower middleware that wraps the proxy's inner service.
//! On `tools/call` requests, it checks the tool name against the resolved role
//! before forwarding. On `tools/list` responses, it filters out tools the role
//! cannot access. Denied calls receive a JSON-RPC `InvalidParams` error with
//! a message identifying the denied tool. Library callers must handle the
//! `Result` returned by [`RbacConfig::new`].

use std::collections::{BTreeSet, HashMap, HashSet};
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

/// Outcome of resolving every mapped role from verified request claims.
enum RoleResolution {
    /// Missing or malformed verified claims must never bypass an installed policy.
    Invalid,
    /// All mapped role names, sorted and deduplicated.
    Roles(BTreeSet<String>),
    /// Valid claims with no recognized values, governed by `default_deny`.
    Unmapped,
}

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
    /// Deny authenticated requests whose claim value is not in `claim_to_role`.
    default_deny: bool,
}

impl RbacConfig {
    /// Build RBAC config from role definitions and claim-to-role mapping.
    ///
    /// Returns an error for incomplete configuration, duplicate/empty role names,
    /// empty claim/mapping values, or references to undefined roles.
    pub fn new(roles: &[RoleConfig], mapping: &RoleMappingConfig) -> anyhow::Result<Self> {
        crate::config::validate_rbac(roles, Some(mapping))?;
        let mut role_allow = HashMap::new();
        let mut role_deny = HashMap::new();

        for role in roles {
            role_allow.insert(
                role.name.clone(),
                role.allow_tools.iter().cloned().collect(),
            );
            if !role.deny_tools.is_empty() {
                role_deny.insert(role.name.clone(), role.deny_tools.iter().cloned().collect());
            }
        }

        Ok(Self {
            claim: mapping.claim.clone(),
            claim_to_role: mapping.mapping.clone(),
            role_allow,
            role_deny,
            default_deny: mapping.default_deny,
        })
    }

    /// Resolve all roles without depending on hash iteration or claim order.
    fn resolve_role(&self, extensions: &tower_mcp::router::Extensions) -> RoleResolution {
        let Some(claims) = extensions.get::<tower_mcp::oauth::token::TokenClaims>() else {
            return RoleResolution::Invalid;
        };
        let mut values = Vec::new();
        if self.claim == "scope" {
            if let Some(scope) = &claims.scope {
                values.extend(scope.split_whitespace());
            }
        } else if let Some(value) = claims.extra.get(&self.claim) {
            match value {
                serde_json::Value::String(value) => {
                    // Preserve exact custom mappings (e.g. a group containing
                    // spaces); otherwise support space-delimited role strings.
                    if self.claim_to_role.contains_key(value) {
                        values.push(value.as_str());
                    } else {
                        values.extend(value.split_whitespace());
                    }
                }
                serde_json::Value::Array(array) => {
                    for item in array {
                        let Some(value) = item.as_str() else {
                            return RoleResolution::Invalid;
                        };
                        values.push(value);
                    }
                }
                _ => return RoleResolution::Invalid,
            }
        }
        let mut roles = BTreeSet::new();
        for value in values {
            if let Some(role) = self.claim_to_role.get(value) {
                // Defense in depth: never treat an unknown role as unrestricted.
                if !self.role_allow.contains_key(role) {
                    return RoleResolution::Invalid;
                }
                roles.insert(role.clone());
            }
        }
        if roles.is_empty() {
            RoleResolution::Unmapped
        } else {
            RoleResolution::Roles(roles)
        }
    }

    /// Grants combine across roles; any explicit deny overrides every grant.
    fn is_tool_allowed(&self, roles: &BTreeSet<String>, tool_name: &str) -> bool {
        let mut granted = false;
        for role in roles {
            let Some(allowed) = self.role_allow.get(role) else {
                return false;
            };
            if self
                .role_deny
                .get(role)
                .is_some_and(|denied| denied.contains(tool_name))
            {
                return false;
            }
            granted |= allowed.is_empty() || allowed.contains(tool_name);
        }
        granted
    }
}

/// Middleware that enforces RBAC on tool calls and list responses.
#[derive(Clone)]
pub struct RbacService<S> {
    inner: S,
    config: Arc<RbacConfig>,
}

impl<S> RbacService<S> {
    /// Create a new RBAC enforcement service wrapping `inner`.
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

        // Missing verified claims indicate a broken authentication boundary.
        let roles = match config.resolve_role(&req.extensions) {
            RoleResolution::Invalid => {
                return Box::pin(async move {
                    Ok(RouterResponse {
                        id: request_id,
                        inner: Err(JsonRpcError::invalid_params(
                            "Missing or invalid verified RBAC claims; access denied".to_string(),
                        )),
                    })
                });
            }
            // Valid claims whose scope is not in the mapping. Under default-deny
            // this is rejected; otherwise it preserves the legacy pass-through.
            RoleResolution::Unmapped => {
                if config.default_deny {
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id: request_id,
                            inner: Err(JsonRpcError::invalid_params(
                                "Authenticated principal carries no recognized role; \
                                 access denied (rbac default_deny)"
                                    .to_string(),
                            )),
                        })
                    });
                }
                let fut = self.inner.call(req);
                return Box::pin(fut);
            }
            RoleResolution::Roles(roles) => roles,
        };

        let roles_for_filter = roles.clone();

        // Check tool calls against RBAC
        if let McpRequest::CallTool(ref params) = req.inner
            && !config.is_tool_allowed(&roles, &params.name)
        {
            let tool_name = params.name.clone();
            return Box::pin(async move {
                Ok(RouterResponse {
                    id: request_id,
                    inner: Err(JsonRpcError::invalid_params(format!(
                        "RBAC principal is not authorized to call tool: {}",
                        tool_name
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
                    .retain(|tool| config.is_tool_allowed(&roles_for_filter, &tool.name));
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
        rbac_config_with_default_deny(false)
    }

    fn rbac_config_with_default_deny(default_deny: bool) -> RbacConfig {
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
            default_deny,
        };
        RbacConfig::new(&roles, &mapping).unwrap()
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
                input_responses: None,
                request_state: None,
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
    async fn test_rbac_no_claims_is_denied() {
        let mock = MockService::with_tools(&["fs/write"]);
        let mut svc = RbacService::new(mock, test_rbac_config());

        // No TokenClaims in extensions
        let req = tower_mcp::RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = svc.call(req).await.unwrap();
        assert!(
            resp.inner.is_err(),
            "missing verified claims must fail closed"
        );
    }

    #[tokio::test]
    async fn test_rbac_unmapped_scope_passes_through_by_default() {
        // Valid claims, but the scope is not in the mapping. With default_deny
        // = false (the default), this preserves legacy pass-through behavior.
        let mock = MockService::with_tools(&["fs/write"]);
        let mut svc = RbacService::new(mock, rbac_config_with_default_deny(false));

        let req = request_with_scope(
            "unknown-scope",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        assert!(
            resp.inner.is_ok(),
            "unmapped scope should pass through when default_deny is false"
        );
    }

    #[tokio::test]
    async fn test_rbac_unmapped_scope_denied_with_default_deny() {
        // Valid claims, scope not in the mapping, default_deny = true: denied.
        let mock = MockService::with_tools(&["fs/write"]);
        let mut svc = RbacService::new(mock, rbac_config_with_default_deny(true));

        let req = request_with_scope(
            "unknown-scope",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(req).await.unwrap();
        let err = resp.inner.unwrap_err();
        assert!(
            err.message.contains("default_deny"),
            "unmapped scope should be denied when default_deny is true, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_rbac_mapped_scope_resolves_with_default_deny_enabled() {
        // A recognized scope must still resolve its role regardless of the
        // default_deny setting: reader can read, cannot write.
        let mock = MockService::with_tools(&["fs/read", "fs/write"]);
        let mut svc = RbacService::new(mock, rbac_config_with_default_deny(true));

        let read_req = request_with_scope(
            "read-only",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/read".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(read_req).await.unwrap();
        assert!(
            resp.inner.is_ok(),
            "mapped role should still resolve with default_deny enabled"
        );

        let write_req = request_with_scope(
            "read-only",
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        );
        let resp = svc.call(write_req).await.unwrap();
        let err = resp.inner.unwrap_err();
        assert!(
            err.message.contains("not authorized"),
            "reader should be denied write via role policy, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_rbac_no_claims_is_denied_with_default_deny() {
        // Missing verified claims fail closed with either default_deny setting.
        let mock = MockService::with_tools(&["fs/write"]);
        let mut svc = RbacService::new(mock, rbac_config_with_default_deny(true));

        let req = tower_mcp::RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/write".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };
        let resp = svc.call(req).await.unwrap();
        assert!(
            resp.inner.is_err(),
            "missing verified claims must fail closed"
        );
    }
    #[tokio::test]
    async fn unknown_role_never_becomes_unrestricted() {
        for default_deny in [true, false] {
            let mut config = rbac_config_with_default_deny(default_deny);
            config
                .claim_to_role
                .insert("read-only".into(), "undefined".into());
            let mut svc = RbacService::new(MockService::with_tools(&["fs/write"]), config);
            let response = svc
                .call(request_with_scope(
                    "read-only admin",
                    McpRequest::ListTools(Default::default()),
                ))
                .await
                .unwrap();
            assert!(response.inner.is_err());
        }
    }

    #[test]
    fn constructor_rejects_invalid_role_definitions_and_targets() {
        let mut mapping = RoleMappingConfig {
            claim: "scope".into(),
            mapping: HashMap::from([("admin".into(), "admin".into())]),
            default_deny: false,
        };
        assert!(RbacConfig::new(&[], &mapping).is_err());
        let roles = [RoleConfig {
            name: "admin".into(),
            allow_tools: vec![],
            deny_tools: vec![],
        }];
        assert!(RbacConfig::new(&roles, &mapping).is_ok());
        mapping.mapping.insert("read".into(), "undefined".into());
        assert!(RbacConfig::new(&roles, &mapping).is_err());
    }

    #[test]
    fn every_scope_order_has_identical_combined_permissions() {
        // Rebuild independent hash maps/sets on every evaluation to exercise the
        // original nondeterministic selection as well as claim order.
        for _ in 0..100 {
            let config = test_rbac_config();
            for scope in [
                "read-only admin extra",
                "read-only extra admin",
                "admin read-only extra",
                "admin extra read-only",
                "extra admin read-only",
                "extra read-only admin",
            ] {
                let req = request_with_scope(scope, McpRequest::ListTools(Default::default()));
                let super::RoleResolution::Roles(roles) = config.resolve_role(&req.extensions)
                else {
                    panic!("recognized roles required");
                };
                assert!(config.is_tool_allowed(&roles, "fs/write"));
                assert!(config.is_tool_allowed(&roles, "fs/read"));
            }
        }
    }

    #[tokio::test]
    async fn missing_role_claim_obeys_explicit_default_policy() {
        for default_deny in [true, false] {
            let mut svc = RbacService::new(
                MockService::with_tools(&["fs/write"]),
                rbac_config_with_default_deny(default_deny),
            );
            let mut request =
                request_with_scope("unused", McpRequest::ListTools(Default::default()));
            let mut claims = request.extensions.get::<TokenClaims>().unwrap().clone();
            claims.scope = None;
            request.extensions.insert(claims);
            assert_eq!(
                svc.call(request).await.unwrap().inner.is_err(),
                default_deny
            );
        }
    }
}
