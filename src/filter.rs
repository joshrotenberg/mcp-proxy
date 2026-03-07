//! Capability filtering middleware for the gateway.
//!
//! Wraps a `Service<RouterRequest>` and filters tools, resources, and prompts
//! based on per-backend allow/deny lists from config.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;

use tower_mcp::protocol::{McpRequest, McpResponse};
use tower_mcp::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use crate::config::BackendFilter;

/// Middleware that filters capabilities from proxy responses.
#[derive(Clone)]
pub struct CapabilityFilterService<S> {
    inner: S,
    filters: Arc<Vec<BackendFilter>>,
}

impl<S> CapabilityFilterService<S> {
    pub fn new(inner: S, filters: Vec<BackendFilter>) -> Self {
        Self {
            inner,
            filters: Arc::new(filters),
        }
    }
}

impl<S> Service<RouterRequest> for CapabilityFilterService<S>
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
        let filters = Arc::clone(&self.filters);
        let request_id = req.id.clone();

        // Check if this is a call/read/get for a filtered capability
        match &req.inner {
            McpRequest::CallTool(params) => {
                if let Some(reason) = check_tool_denied(&filters, &params.name) {
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id: request_id,
                            inner: Err(JsonRpcError::invalid_params(reason)),
                        })
                    });
                }
            }
            McpRequest::ReadResource(params) => {
                if let Some(reason) = check_resource_denied(&filters, &params.uri) {
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id: request_id,
                            inner: Err(JsonRpcError::invalid_params(reason)),
                        })
                    });
                }
            }
            McpRequest::GetPrompt(params) => {
                if let Some(reason) = check_prompt_denied(&filters, &params.name) {
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id: request_id,
                            inner: Err(JsonRpcError::invalid_params(reason)),
                        })
                    });
                }
            }
            _ => {}
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut resp = fut.await?;

            // Filter list responses
            if let Ok(ref mut mcp_resp) = resp.inner {
                match mcp_resp {
                    McpResponse::ListTools(result) => {
                        result.tools.retain(|tool| {
                            for f in filters.iter() {
                                if let Some(local_name) = tool.name.strip_prefix(&f.namespace) {
                                    return f.tool_filter.allows(local_name);
                                }
                            }
                            true
                        });
                    }
                    McpResponse::ListResources(result) => {
                        result.resources.retain(|resource| {
                            for f in filters.iter() {
                                if let Some(local_uri) = resource.uri.strip_prefix(&f.namespace) {
                                    return f.resource_filter.allows(local_uri);
                                }
                            }
                            true
                        });
                    }
                    McpResponse::ListResourceTemplates(result) => {
                        result.resource_templates.retain(|template| {
                            for f in filters.iter() {
                                if let Some(local_uri) =
                                    template.uri_template.strip_prefix(&f.namespace)
                                {
                                    return f.resource_filter.allows(local_uri);
                                }
                            }
                            true
                        });
                    }
                    McpResponse::ListPrompts(result) => {
                        result.prompts.retain(|prompt| {
                            for f in filters.iter() {
                                if let Some(local_name) = prompt.name.strip_prefix(&f.namespace) {
                                    return f.prompt_filter.allows(local_name);
                                }
                            }
                            true
                        });
                    }
                    _ => {}
                }
            }

            Ok(resp)
        })
    }
}

/// Check if a namespaced tool name is denied by any filter.
/// Returns Some(reason) if denied.
fn check_tool_denied(filters: &[BackendFilter], namespaced_name: &str) -> Option<String> {
    for f in filters {
        if let Some(local_name) = namespaced_name.strip_prefix(&f.namespace) {
            if !f.tool_filter.allows(local_name) {
                return Some(format!("Tool not available: {}", namespaced_name));
            }
            return None;
        }
    }
    None
}

/// Check if a namespaced resource URI is denied by any filter.
fn check_resource_denied(filters: &[BackendFilter], namespaced_uri: &str) -> Option<String> {
    for f in filters {
        if let Some(local_uri) = namespaced_uri.strip_prefix(&f.namespace) {
            if !f.resource_filter.allows(local_uri) {
                return Some(format!("Resource not available: {}", namespaced_uri));
            }
            return None;
        }
    }
    None
}

/// Check if a namespaced prompt name is denied by any filter.
fn check_prompt_denied(filters: &[BackendFilter], namespaced_name: &str) -> Option<String> {
    for f in filters {
        if let Some(local_name) = namespaced_name.strip_prefix(&f.namespace) {
            if !f.prompt_filter.allows(local_name) {
                return Some(format!("Prompt not available: {}", namespaced_name));
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::CapabilityFilterService;
    use crate::config::{BackendFilter, NameFilter};
    use crate::test_util::{MockService, call_service};

    fn allow_filter(namespace: &str, tools: &[&str]) -> BackendFilter {
        BackendFilter {
            namespace: namespace.to_string(),
            tool_filter: NameFilter::AllowList(tools.iter().map(|s| s.to_string()).collect()),
            resource_filter: NameFilter::PassAll,
            prompt_filter: NameFilter::PassAll,
        }
    }

    fn deny_filter(namespace: &str, tools: &[&str]) -> BackendFilter {
        BackendFilter {
            namespace: namespace.to_string(),
            tool_filter: NameFilter::DenyList(tools.iter().map(|s| s.to_string()).collect()),
            resource_filter: NameFilter::PassAll,
            prompt_filter: NameFilter::PassAll,
        }
    }

    #[tokio::test]
    async fn test_filter_allow_list_tools() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "fs/delete"]);
        let filters = vec![allow_filter("fs/", &["read", "write"])];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"fs/read"));
                assert!(names.contains(&"fs/write"));
                assert!(!names.contains(&"fs/delete"), "delete should be filtered");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_filter_deny_list_tools() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "fs/delete"]);
        let filters = vec![deny_filter("fs/", &["delete"])];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"fs/read"));
                assert!(names.contains(&"fs/write"));
                assert!(!names.contains(&"fs/delete"));
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_filter_denies_call_to_hidden_tool() {
        let mock = MockService::with_tools(&["fs/read", "fs/delete"]);
        let filters = vec![allow_filter("fs/", &["read"])];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/delete".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        let err = resp.inner.unwrap_err();
        assert!(
            err.message.contains("not available"),
            "should deny: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_filter_allows_call_to_permitted_tool() {
        let mock = MockService::with_tools(&["fs/read"]);
        let filters = vec![allow_filter("fs/", &["read"])];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/read".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok(), "allowed tool should succeed");
    }

    #[tokio::test]
    async fn test_filter_pass_all_allows_everything() {
        let mock = MockService::with_tools(&["fs/read", "fs/write", "fs/delete"]);
        let filters = vec![BackendFilter {
            namespace: "fs/".to_string(),
            tool_filter: NameFilter::PassAll,
            resource_filter: NameFilter::PassAll,
            prompt_filter: NameFilter::PassAll,
        }];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 3);
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_filter_unmatched_namespace_passes_through() {
        let mock = MockService::with_tools(&["db/query"]);
        let filters = vec![allow_filter("fs/", &["read"])];
        let mut svc = CapabilityFilterService::new(mock, filters);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                assert_eq!(result.tools.len(), 1, "unmatched namespace should pass");
                assert_eq!(result.tools[0].name, "db/query");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }
}
