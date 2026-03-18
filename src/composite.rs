//! Composite tool middleware for fan-out to multiple backend tools.
//!
//! Composite tools are virtual tools that do not exist on any single backend.
//! When called, they fan out the request to multiple backend tools concurrently,
//! aggregating all results into a single response. This is useful for
//! cross-cutting operations like "search everything" or "health-check all
//! backends."
//!
//! # How it works
//!
//! The [`CompositeService`] intercepts two request types:
//!
//! - **`ListTools`** -- appends the composite tool definitions to the response
//!   so clients discover them alongside regular backend tools.
//! - **`CallTool`** -- if the tool name matches a composite, the same arguments
//!   are forwarded to every target tool concurrently using `tokio::JoinSet`.
//!   Results from all targets are collected into a single `CallToolResult`
//!   whose `content` is the concatenation of all individual results. If any
//!   target fails, the aggregated result's `is_error` flag is set to `true`,
//!   but successful results are still included.
//!
//! All other request types pass through unchanged.
//!
//! # Strategy
//!
//! The `strategy` field controls execution order. Currently one strategy
//! is supported:
//!
//! - **`parallel`** (default) -- all target tools execute concurrently via
//!   `tokio::JoinSet`. Results are returned in completion order.
//!
//! # Configuration
//!
//! Composite tools are defined at the top level in TOML, referencing
//! namespaced tool names from any backend:
//!
//! ```toml
//! [[composite_tools]]
//! name = "search_all"
//! description = "Search across all knowledge sources"
//! tools = ["github/search", "jira/search", "docs/search"]
//! strategy = "parallel"
//! ```
//!
//! Validation enforces that composite tool names are non-empty, unique,
//! and reference at least one target tool.
//!
//! # Middleware stack position
//!
//! Composite tools are the outermost middleware in the request-processing
//! stack, applied after aliasing. This means composite tool names are not
//! subject to alias rewriting, but the target tools they reference are
//! resolved through the full middleware chain (including aliases, filters,
//! and validation). The ordering in `proxy.rs`:
//!
//! 1. Request validation ([`crate::validation`])
//! 2. Capability filtering ([`crate::filter`])
//! 3. Search-mode filtering ([`crate::filter`])
//! 4. Tool aliasing ([`crate::alias`])
//! 5. **Composite tools** (this module)

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::task::JoinSet;
use tower::{Layer, Service};
use tower_mcp::protocol::{
    CallToolParams, CallToolResult, McpRequest, McpResponse, ToolDefinition,
};
use tower_mcp::router::{RouterRequest, RouterResponse};

use crate::config::CompositeToolConfig;

/// Tower layer that produces a [`CompositeService`].
///
/// # Example
///
/// ```rust,ignore
/// use tower::ServiceBuilder;
/// use mcp_proxy::composite::CompositeLayer;
/// use mcp_proxy::config::CompositeToolConfig;
///
/// let composites = vec![CompositeToolConfig {
///     name: "search_all".into(),
///     description: "Search everything".into(),
///     tools: vec!["github/search".into(), "docs/search".into()],
///     strategy: Default::default(),
/// }];
///
/// let service = ServiceBuilder::new()
///     .layer(CompositeLayer::new(composites))
///     .service(proxy);
/// ```
#[derive(Clone)]
pub struct CompositeLayer {
    composites: Arc<Vec<CompositeToolConfig>>,
}

impl CompositeLayer {
    /// Create a new composite layer with the given tool definitions.
    pub fn new(composites: Vec<CompositeToolConfig>) -> Self {
        Self {
            composites: Arc::new(composites),
        }
    }
}

impl<S> Layer<S> for CompositeLayer {
    type Service = CompositeService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CompositeService::new(inner, (*self.composites).clone())
    }
}

/// Tower service that intercepts `ListTools` and `CallTool` requests
/// to support composite tool fan-out.
#[derive(Clone)]
pub struct CompositeService<S> {
    inner: S,
    composites: Arc<Vec<CompositeToolConfig>>,
}

impl<S> CompositeService<S> {
    /// Create a new composite service wrapping `inner` with the given composite tool configs.
    pub fn new(inner: S, composites: Vec<CompositeToolConfig>) -> Self {
        Self {
            inner,
            composites: Arc::new(composites),
        }
    }
}

impl<S> Service<RouterRequest> for CompositeService<S>
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
        let composites = Arc::clone(&self.composites);

        // Check if this is a CallTool for a composite tool
        if let McpRequest::CallTool(ref params) = req.inner
            && let Some(composite) = composites.iter().find(|c| c.name == params.name)
        {
            let id = req.id.clone();
            let extensions = req.extensions.clone();
            let tool_names = composite.tools.clone();
            let arguments = params.arguments.clone();
            let meta = params.meta.clone();
            let task = params.task.clone();
            let inner = self.inner.clone();

            return Box::pin(async move {
                let mut join_set = JoinSet::new();

                for tool_name in tool_names {
                    let mut svc = inner.clone();
                    let tool_req = RouterRequest {
                        id: id.clone(),
                        inner: McpRequest::CallTool(CallToolParams {
                            name: tool_name,
                            arguments: arguments.clone(),
                            meta: meta.clone(),
                            task: task.clone(),
                        }),
                        extensions: extensions.clone(),
                    };
                    join_set.spawn(async move { svc.call(tool_req).await });
                }

                let mut all_content = Vec::new();
                let mut any_error = false;

                while let Some(result) = join_set.join_next().await {
                    match result {
                        Ok(Ok(resp)) => match resp.inner {
                            Ok(McpResponse::CallTool(call_result)) => {
                                if call_result.is_error {
                                    any_error = true;
                                }
                                all_content.extend(call_result.content);
                            }
                            Err(json_rpc_err) => {
                                any_error = true;
                                all_content.push(tower_mcp::protocol::Content::text(format!(
                                    "Error: {}",
                                    json_rpc_err.message
                                )));
                            }
                            Ok(other) => {
                                any_error = true;
                                all_content.push(tower_mcp::protocol::Content::text(format!(
                                    "Unexpected response type: {:?}",
                                    other
                                )));
                            }
                        },
                        Ok(Err(_infallible)) => {
                            // Infallible error -- cannot happen
                        }
                        Err(join_err) => {
                            any_error = true;
                            all_content.push(tower_mcp::protocol::Content::text(format!(
                                "Task failed: {}",
                                join_err
                            )));
                        }
                    }
                }

                let result = CallToolResult {
                    content: all_content,
                    is_error: any_error,
                    structured_content: None,
                    meta: None,
                };

                Ok(RouterResponse {
                    id,
                    inner: Ok(McpResponse::CallTool(result)),
                })
            });
        }

        // For ListTools, append composite tool definitions
        if matches!(req.inner, McpRequest::ListTools(_)) {
            let fut = self.inner.call(req);

            return Box::pin(async move {
                let mut result = fut.await;

                let Ok(ref mut resp) = result;
                if let Ok(McpResponse::ListTools(ref mut list_result)) = resp.inner {
                    for composite in composites.iter() {
                        list_result.tools.push(ToolDefinition {
                            name: composite.name.clone(),
                            title: None,
                            description: Some(composite.description.clone()),
                            input_schema: serde_json::json!({"type": "object"}),
                            output_schema: None,
                            icons: None,
                            annotations: None,
                            execution: None,
                            meta: None,
                        });
                    }
                }

                result
            });
        }

        // All other requests pass through unchanged
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::CompositeService;
    use crate::config::{CompositeStrategy, CompositeToolConfig};
    use crate::test_util::{ErrorMockService, MockService, call_service};

    fn test_composites() -> Vec<CompositeToolConfig> {
        vec![CompositeToolConfig {
            name: "search_all".to_string(),
            description: "Search across all sources".to_string(),
            tools: vec!["github/search".to_string(), "docs/search".to_string()],
            strategy: CompositeStrategy::Parallel,
        }]
    }

    #[tokio::test]
    async fn test_composite_appears_in_list_tools() {
        let mock = MockService::with_tools(&["github/search", "docs/search", "db/query"]);
        let mut svc = CompositeService::new(mock, test_composites());

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"github/search"));
                assert!(names.contains(&"docs/search"));
                assert!(names.contains(&"db/query"));
                assert!(
                    names.contains(&"search_all"),
                    "composite tool should appear"
                );
                // Verify description
                let composite_tool = result
                    .tools
                    .iter()
                    .find(|t| t.name == "search_all")
                    .unwrap();
                assert_eq!(
                    composite_tool.description.as_deref(),
                    Some("Search across all sources")
                );
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_composite_fan_out_aggregates_results() {
        let mock = MockService::with_tools(&["github/search", "docs/search"]);
        let mut svc = CompositeService::new(mock, test_composites());

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "search_all".to_string(),
                arguments: serde_json::json!({"q": "test"}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.content.len(), 2, "should aggregate both results");
                let texts: Vec<String> = result
                    .content
                    .iter()
                    .map(|c| c.as_text().unwrap().to_string())
                    .collect();
                assert!(texts.contains(&"called: github/search".to_string()));
                assert!(texts.contains(&"called: docs/search".to_string()));
                assert!(!result.is_error, "no errors expected");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_non_composite_call_passes_through() {
        let mock = MockService::with_tools(&["db/query"]);
        let mut svc = CompositeService::new(mock, test_composites());

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "db/query".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "called: db/query");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_partial_failure_returns_partial_results() {
        // Use ErrorMockService -- all calls will fail, producing error content
        let mock = ErrorMockService;
        let mut svc = CompositeService::new(mock, test_composites());

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "search_all".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(
                    result.content.len(),
                    2,
                    "should have error content for both tools"
                );
                assert!(result.is_error, "should be marked as error");
                for content in &result.content {
                    let text = content.as_text().unwrap();
                    assert!(
                        text.contains("Error:"),
                        "content should describe error: {text}"
                    );
                }
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_non_tool_requests_pass_through() {
        let mock = MockService::with_tools(&[]);
        let mut svc = CompositeService::new(mock, test_composites());

        let resp = call_service(&mut svc, McpRequest::Ping).await;
        match resp.inner.unwrap() {
            McpResponse::Pong(_) => {} // expected
            other => panic!("expected Pong, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_empty_composites_passes_through() {
        let mock = MockService::with_tools(&["tool1"]);
        let mut svc = CompositeService::new(mock, vec![]);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 1);
                assert_eq!(result.tools[0].name, "tool1");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }
}
