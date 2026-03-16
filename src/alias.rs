//! Tool aliasing middleware for the proxy.
//!
//! Rewrites tool names in list responses and call requests based on
//! per-backend alias configuration.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{McpRequest, McpResponse};

/// Tower layer that produces an [`AliasService`].
///
/// # Example
///
/// ```rust,ignore
/// use tower::ServiceBuilder;
/// use mcp_proxy::alias::{AliasLayer, AliasMap};
///
/// let aliases = AliasMap::new(vec![
///     ("math/".into(), "add".into(), "sum".into()),
/// ]).unwrap();
///
/// let service = ServiceBuilder::new()
///     .layer(AliasLayer::new(aliases))
///     .service(proxy);
/// ```
#[derive(Clone)]
pub struct AliasLayer {
    aliases: AliasMap,
}

impl AliasLayer {
    /// Create a new alias layer with the given alias map.
    pub fn new(aliases: AliasMap) -> Self {
        Self { aliases }
    }
}

impl<S> Layer<S> for AliasLayer {
    type Service = AliasService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AliasService::new(inner, self.aliases.clone())
    }
}

/// Resolved alias mappings for all backends.
#[derive(Clone)]
pub struct AliasMap {
    /// Maps "namespace/original" -> "namespace/alias" (for list responses)
    pub forward: HashMap<String, String>,
    /// Maps "namespace/alias" -> "namespace/original" (for call requests)
    reverse: HashMap<String, String>,
}

impl AliasMap {
    /// Build an alias map from `(namespace, from, to)` triples. Returns `None` if empty.
    pub fn new(mappings: Vec<(String, String, String)>) -> Option<Self> {
        if mappings.is_empty() {
            return None;
        }
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        for (namespace, from, to) in mappings {
            let original = format!("{}{}", namespace, from);
            let aliased = format!("{}{}", namespace, to);
            forward.insert(original.clone(), aliased.clone());
            reverse.insert(aliased, original);
        }
        Some(Self { forward, reverse })
    }
}

/// Tower service that rewrites tool names based on alias configuration.
#[derive(Clone)]
pub struct AliasService<S> {
    inner: S,
    aliases: Arc<AliasMap>,
}

impl<S> AliasService<S> {
    /// Create a new alias service wrapping `inner` with the given alias map.
    pub fn new(inner: S, aliases: AliasMap) -> Self {
        Self {
            inner,
            aliases: Arc::new(aliases),
        }
    }
}

impl<S> Service<RouterRequest> for AliasService<S>
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
        let aliases = Arc::clone(&self.aliases);

        // Reverse-map aliased names back to originals in requests
        match &mut req.inner {
            McpRequest::CallTool(params) => {
                if let Some(original) = aliases.reverse.get(&params.name) {
                    params.name = original.clone();
                }
            }
            McpRequest::ReadResource(params) => {
                if let Some(original) = aliases.reverse.get(&params.uri) {
                    params.uri = original.clone();
                }
            }
            McpRequest::GetPrompt(params) => {
                if let Some(original) = aliases.reverse.get(&params.name) {
                    params.name = original.clone();
                }
            }
            _ => {}
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut result = fut.await;

            // Forward-map original names to aliases in responses
            let Ok(ref mut resp) = result;
            if let Ok(mcp_resp) = &mut resp.inner {
                match mcp_resp {
                    McpResponse::ListTools(r) => {
                        for tool in &mut r.tools {
                            if let Some(aliased) = aliases.forward.get(&tool.name) {
                                tool.name = aliased.clone();
                            }
                        }
                    }
                    McpResponse::ListResources(r) => {
                        for res in &mut r.resources {
                            if let Some(aliased) = aliases.forward.get(&res.uri) {
                                res.uri = aliased.clone();
                            }
                        }
                    }
                    McpResponse::ListPrompts(r) => {
                        for prompt in &mut r.prompts {
                            if let Some(aliased) = aliases.forward.get(&prompt.name) {
                                prompt.name = aliased.clone();
                            }
                        }
                    }
                    _ => {}
                }
            }

            result
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::{AliasMap, AliasService};
    use crate::test_util::{MockService, call_service};

    fn test_aliases() -> AliasMap {
        AliasMap::new(vec![
            ("files/".into(), "read_file".into(), "read".into()),
            ("files/".into(), "write_file".into(), "write".into()),
        ])
        .unwrap()
    }

    #[test]
    fn test_alias_map_empty_returns_none() {
        assert!(AliasMap::new(vec![]).is_none());
    }

    #[test]
    fn test_alias_map_forward_and_reverse() {
        let aliases = test_aliases();
        assert_eq!(
            aliases.forward.get("files/read_file").unwrap(),
            "files/read"
        );
        assert_eq!(aliases.forward.len(), 2);
    }

    #[tokio::test]
    async fn test_alias_rewrites_list_tools() {
        let mock = MockService::with_tools(&["files/read_file", "files/write_file", "db/query"]);
        let mut svc = AliasService::new(mock, test_aliases());

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_str()).collect();
                assert!(names.contains(&"files/read"));
                assert!(names.contains(&"files/write"));
                assert!(names.contains(&"db/query")); // unchanged
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_alias_reverse_maps_call_tool() {
        let mock = MockService::with_tools(&["files/read_file"]);
        let mut svc = AliasService::new(mock, test_aliases());

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "files/read".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(result) => {
                assert_eq!(result.all_text(), "called: files/read_file");
            }
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_alias_passthrough_non_aliased() {
        let mock = MockService::with_tools(&["db/query"]);
        let mut svc = AliasService::new(mock, test_aliases());

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
}
