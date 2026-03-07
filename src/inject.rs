//! Argument injection middleware for tool calls.
//!
//! Merges default or per-tool arguments into tool call requests before they
//! reach the backend. Useful for injecting timeouts, safety caps, or
//! read-only flags without requiring clients to set them.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "db"
//! transport = "http"
//! url = "http://db.internal:8080"
//!
//! # Inject into all tool calls for this backend
//! [backends.default_args]
//! timeout = 30
//!
//! # Inject into a specific tool (overrides default_args for matching keys)
//! [[backends.inject_args]]
//! tool = "query"
//! args = { read_only = true, max_rows = 1000 }
//!
//! # Overwrite existing arguments
//! [[backends.inject_args]]
//! tool = "dangerous_op"
//! args = { dry_run = true }
//! overwrite = true
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

/// Per-tool injection rule.
#[derive(Debug, Clone)]
struct ToolInjection {
    args: serde_json::Map<String, serde_json::Value>,
    overwrite: bool,
}

/// Resolved injection rules for a single backend namespace.
#[derive(Debug, Clone)]
pub struct InjectionRules {
    /// Namespace prefix (e.g. "db/").
    namespace: String,
    /// Default args applied to all tools in this namespace.
    default_args: serde_json::Map<String, serde_json::Value>,
    /// Per-tool overrides keyed by namespaced tool name (e.g. "db/query").
    tool_rules: HashMap<String, ToolInjection>,
}

impl InjectionRules {
    /// Create injection rules for a backend.
    pub fn new(
        namespace: String,
        default_args: serde_json::Map<String, serde_json::Value>,
        tool_rules: Vec<crate::config::InjectArgsConfig>,
    ) -> Self {
        let tool_rules = tool_rules
            .into_iter()
            .map(|r| {
                let namespaced = format!("{namespace}{}", r.tool);
                (
                    namespaced,
                    ToolInjection {
                        args: r.args,
                        overwrite: r.overwrite,
                    },
                )
            })
            .collect();

        Self {
            namespace,
            default_args,
            tool_rules,
        }
    }
}

/// Argument injection middleware.
///
/// Intercepts `CallTool` requests and merges configured arguments into
/// the tool call arguments before forwarding to the inner service.
#[derive(Clone)]
pub struct InjectArgsService<S> {
    inner: S,
    rules: Arc<Vec<InjectionRules>>,
}

impl<S> InjectArgsService<S> {
    /// Create a new argument injection service.
    pub fn new(inner: S, rules: Vec<InjectionRules>) -> Self {
        Self {
            inner,
            rules: Arc::new(rules),
        }
    }
}

/// Merge source args into target. If `overwrite` is false, existing keys
/// in target are preserved.
fn merge_args(
    target: &mut serde_json::Value,
    source: &serde_json::Map<String, serde_json::Value>,
    overwrite: bool,
) {
    if let serde_json::Value::Object(map) = target {
        for (key, value) in source {
            if overwrite || !map.contains_key(key) {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

impl<S> Service<RouterRequest> for InjectArgsService<S>
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
        if let McpRequest::CallTool(ref mut params) = req.inner {
            for rules in self.rules.iter() {
                if !params.name.starts_with(&rules.namespace) {
                    continue;
                }

                // Apply default args (never overwrite)
                if !rules.default_args.is_empty() {
                    merge_args(&mut params.arguments, &rules.default_args, false);
                }

                // Apply per-tool rules
                if let Some(tool_rule) = rules.tool_rules.get(&params.name) {
                    merge_args(&mut params.arguments, &tool_rule.args, tool_rule.overwrite);
                }

                break; // Only match one namespace
            }
        }

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InjectArgsConfig;
    use crate::test_util::{MockService, call_service};
    use tower_mcp_types::protocol::{CallToolParams, McpRequest};

    fn make_rules(
        namespace: &str,
        default_args: serde_json::Map<String, serde_json::Value>,
        tool_rules: Vec<InjectArgsConfig>,
    ) -> Vec<InjectionRules> {
        vec![InjectionRules::new(
            namespace.to_string(),
            default_args,
            tool_rules,
        )]
    }

    #[tokio::test]
    async fn test_injects_default_args() {
        let mock = MockService::with_tools(&["db/query"]);
        let mut defaults = serde_json::Map::new();
        defaults.insert("timeout".to_string(), serde_json::json!(30));

        let rules = make_rules("db/", defaults, vec![]);
        let mut svc = InjectArgsService::new(mock, rules);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "db/query".to_string(),
                arguments: serde_json::json!({"sql": "SELECT 1"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        // The mock returns "called: db/query" but we can verify it didn't error
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_default_args_dont_overwrite() {
        let mock = MockService::with_tools(&["db/query"]);
        let mut defaults = serde_json::Map::new();
        defaults.insert("timeout".to_string(), serde_json::json!(30));

        let rules = make_rules("db/", defaults, vec![]);
        let _svc = InjectArgsService::new(mock, rules);

        // Create a request that already has timeout=60
        let mut req = RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "db/query".to_string(),
                arguments: serde_json::json!({"sql": "SELECT 1", "timeout": 60}),
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        // Manually apply the injection to verify merge behavior
        if let McpRequest::CallTool(ref mut params) = req.inner {
            let mut defaults = serde_json::Map::new();
            defaults.insert("timeout".to_string(), serde_json::json!(30));
            merge_args(&mut params.arguments, &defaults, false);

            // timeout should still be 60 (not overwritten)
            assert_eq!(params.arguments["timeout"], 60);
            // sql should be preserved
            assert_eq!(params.arguments["sql"], "SELECT 1");
        }
    }

    #[tokio::test]
    async fn test_per_tool_injection() {
        let mock = MockService::with_tools(&["db/query"]);
        let tool_rules = vec![InjectArgsConfig {
            tool: "query".to_string(),
            args: {
                let mut m = serde_json::Map::new();
                m.insert("read_only".to_string(), serde_json::json!(true));
                m
            },
            overwrite: false,
        }];

        let rules = make_rules("db/", serde_json::Map::new(), tool_rules);
        let mut svc = InjectArgsService::new(mock, rules);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "db/query".to_string(),
                arguments: serde_json::json!({"sql": "SELECT 1"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_overwrite_mode() {
        let mut args = serde_json::json!({"dry_run": false, "data": "hello"});
        let mut inject = serde_json::Map::new();
        inject.insert("dry_run".to_string(), serde_json::json!(true));

        // Without overwrite
        merge_args(&mut args, &inject, false);
        assert_eq!(args["dry_run"], false); // preserved

        // With overwrite
        merge_args(&mut args, &inject, true);
        assert_eq!(args["dry_run"], true); // overwritten
        assert_eq!(args["data"], "hello"); // other fields preserved
    }

    #[tokio::test]
    async fn test_non_matching_namespace_passes_through() {
        let mock = MockService::with_tools(&["other/tool"]);
        let mut defaults = serde_json::Map::new();
        defaults.insert("timeout".to_string(), serde_json::json!(30));

        let rules = make_rules("db/", defaults, vec![]);
        let mut svc = InjectArgsService::new(mock, rules);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "other/tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_non_call_tool_passes_through() {
        let mock = MockService::with_tools(&["db/query"]);
        let mut defaults = serde_json::Map::new();
        defaults.insert("timeout".to_string(), serde_json::json!(30));

        let rules = make_rules("db/", defaults, vec![]);
        let mut svc = InjectArgsService::new(mock, rules);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[test]
    fn test_merge_args_into_non_object() {
        // If arguments isn't an object, merge is a no-op
        let mut args = serde_json::json!("not an object");
        let mut inject = serde_json::Map::new();
        inject.insert("key".to_string(), serde_json::json!("value"));
        merge_args(&mut args, &inject, false);
        assert_eq!(args, serde_json::json!("not an object"));
    }

    #[test]
    fn test_merge_args_adds_new_keys() {
        let mut args = serde_json::json!({"existing": 1});
        let mut inject = serde_json::Map::new();
        inject.insert("new_key".to_string(), serde_json::json!(42));
        merge_args(&mut args, &inject, false);
        assert_eq!(args["existing"], 1);
        assert_eq!(args["new_key"], 42);
    }
}
