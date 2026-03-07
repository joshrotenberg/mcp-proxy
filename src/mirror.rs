//! Traffic mirroring / shadowing middleware.
//!
//! Sends a copy of traffic to a secondary backend (fire-and-forget, response
//! discarded). Useful for testing new backend versions, benchmarking, or
//! audit recording.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "api"
//! transport = "http"
//! url = "http://api.internal:8080"
//!
//! [[backends]]
//! name = "api-v2"
//! transport = "http"
//! url = "http://api-v2.internal:8080"
//! mirror_of = "api"        # mirror traffic from "api" backend
//! mirror_percent = 10      # mirror 10% of requests
//! ```
//!
//! # How it works
//!
//! 1. Request arrives targeting `api/search`
//! 2. Primary response is returned from the `api` backend as normal
//! 3. A copy of the request is rewritten to `api-v2/search` and sent
//!    fire-and-forget to the `api-v2` backend
//! 4. The mirror response is discarded; errors are logged but don't
//!    affect the primary response

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{CallToolParams, GetPromptParams, McpRequest, ReadResourceParams};

/// Mapping from a source backend namespace to its mirror configuration.
#[derive(Debug, Clone)]
struct MirrorMapping {
    /// Source namespace prefix (e.g. "api/").
    source_prefix: String,
    /// Mirror namespace prefix (e.g. "api-v2/").
    mirror_prefix: String,
    /// Percentage of requests to mirror (1-100).
    percent: u32,
    /// Atomic counter for deterministic percentage-based sampling.
    counter: Arc<AtomicU64>,
}

/// Traffic mirroring middleware.
///
/// Wraps the proxy service and sends copies of matching requests to
/// mirror backends. The primary response is always returned; mirror
/// responses are discarded.
#[derive(Clone)]
pub struct MirrorService<S> {
    inner: S,
    mappings: Arc<Vec<MirrorMapping>>,
}

impl<S> MirrorService<S> {
    /// Create a new mirror service.
    ///
    /// `mirrors` maps source backend names to `(mirror_name, percent)`.
    /// The `separator` is used to construct namespace prefixes.
    pub fn new(inner: S, mirrors: HashMap<String, (String, u32)>, separator: &str) -> Self {
        let mappings = mirrors
            .into_iter()
            .map(|(source, (mirror, percent))| MirrorMapping {
                source_prefix: format!("{source}{separator}"),
                mirror_prefix: format!("{mirror}{separator}"),
                percent: percent.clamp(1, 100),
                counter: Arc::new(AtomicU64::new(0)),
            })
            .collect();

        Self {
            inner,
            mappings: Arc::new(mappings),
        }
    }
}

/// Check if a request name starts with a namespace prefix and return the
/// matching mirror mapping.
fn find_mirror<'a>(name: &str, mappings: &'a [MirrorMapping]) -> Option<&'a MirrorMapping> {
    mappings.iter().find(|m| name.starts_with(&m.source_prefix))
}

/// Rewrite a namespaced name from source to mirror prefix.
fn rewrite_name(name: &str, source_prefix: &str, mirror_prefix: &str) -> String {
    let suffix = &name[source_prefix.len()..];
    format!("{mirror_prefix}{suffix}")
}

/// Clone a request with its name rewritten to the mirror namespace.
fn clone_for_mirror(
    req: &RouterRequest,
    source_prefix: &str,
    mirror_prefix: &str,
) -> Option<RouterRequest> {
    let new_inner = match &req.inner {
        McpRequest::CallTool(params) if params.name.starts_with(source_prefix) => {
            McpRequest::CallTool(CallToolParams {
                name: rewrite_name(&params.name, source_prefix, mirror_prefix),
                arguments: params.arguments.clone(),
                meta: params.meta.clone(),
                task: params.task.clone(),
            })
        }
        McpRequest::ReadResource(params) if params.uri.starts_with(source_prefix) => {
            McpRequest::ReadResource(ReadResourceParams {
                uri: rewrite_name(&params.uri, source_prefix, mirror_prefix),
                meta: params.meta.clone(),
            })
        }
        McpRequest::GetPrompt(params) if params.name.starts_with(source_prefix) => {
            McpRequest::GetPrompt(GetPromptParams {
                name: rewrite_name(&params.name, source_prefix, mirror_prefix),
                arguments: params.arguments.clone(),
                meta: params.meta.clone(),
            })
        }
        // List requests and other types aren't mirrored
        _ => return None,
    };

    Some(RouterRequest {
        id: req.id.clone(),
        inner: new_inner,
        extensions: Extensions::new(),
    })
}

/// Check if the sampling counter says this request should be mirrored.
fn should_mirror(mapping: &MirrorMapping) -> bool {
    if mapping.percent >= 100 {
        return true;
    }
    let count = mapping.counter.fetch_add(1, Ordering::Relaxed);
    (count % 100) < mapping.percent as u64
}

/// Extract the request name for namespace matching.
fn request_name(req: &McpRequest) -> Option<&str> {
    match req {
        McpRequest::CallTool(params) => Some(&params.name),
        McpRequest::ReadResource(params) => Some(&params.uri),
        McpRequest::GetPrompt(params) => Some(&params.name),
        _ => None,
    }
}

impl<S> Service<RouterRequest> for MirrorService<S>
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
        // Check if this request should be mirrored
        let mirror_req = request_name(&req.inner)
            .and_then(|name| find_mirror(name, &self.mappings))
            .filter(|mapping| should_mirror(mapping))
            .and_then(|mapping| {
                clone_for_mirror(&req, &mapping.source_prefix, &mapping.mirror_prefix)
            });

        // Send the primary request
        let primary_fut = self.inner.call(req);

        // If mirroring, clone the service and spawn a fire-and-forget task
        let mut mirror_svc = if mirror_req.is_some() {
            Some(self.inner.clone())
        } else {
            None
        };

        Box::pin(async move {
            // Spawn mirror request as a fire-and-forget task
            if let Some(mirror) = mirror_req
                && let Some(ref mut svc) = mirror_svc
            {
                let mut svc = svc.clone();
                tokio::spawn(async move {
                    match svc.call(mirror).await {
                        Ok(resp) => {
                            if resp.inner.is_err() {
                                tracing::debug!("Mirror request returned error (discarded)");
                            }
                        }
                        Err(e) => match e {},
                    }
                });
            }

            primary_fut.await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockService, call_service};
    use tower_mcp::protocol::RequestId;
    use tower_mcp::router::Extensions;
    use tower_mcp_types::protocol::McpRequest;

    fn make_mirrors(source: &str, mirror: &str, percent: u32) -> HashMap<String, (String, u32)> {
        let mut m = HashMap::new();
        m.insert(source.to_string(), (mirror.to_string(), percent));
        m
    }

    #[test]
    fn test_rewrite_name() {
        assert_eq!(
            rewrite_name("api/search", "api/", "api-v2/"),
            "api-v2/search"
        );
        assert_eq!(
            rewrite_name("api/nested/tool", "api/", "mirror/"),
            "mirror/nested/tool"
        );
    }

    #[test]
    fn test_find_mirror_match() {
        let mappings = vec![MirrorMapping {
            source_prefix: "api/".to_string(),
            mirror_prefix: "api-v2/".to_string(),
            percent: 100,
            counter: Arc::new(AtomicU64::new(0)),
        }];
        assert!(find_mirror("api/search", &mappings).is_some());
        assert!(find_mirror("other/search", &mappings).is_none());
    }

    #[test]
    fn test_should_mirror_100_percent() {
        let mapping = MirrorMapping {
            source_prefix: "api/".to_string(),
            mirror_prefix: "api-v2/".to_string(),
            percent: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };
        // All requests should be mirrored
        for _ in 0..10 {
            assert!(should_mirror(&mapping));
        }
    }

    #[test]
    fn test_should_mirror_percentage() {
        let mapping = MirrorMapping {
            source_prefix: "api/".to_string(),
            mirror_prefix: "api-v2/".to_string(),
            percent: 10,
            counter: Arc::new(AtomicU64::new(0)),
        };
        // Over 100 requests, exactly 10 should be mirrored
        let mirrored: u32 = (0..100).filter(|_| should_mirror(&mapping)).count() as u32;
        assert_eq!(mirrored, 10);
    }

    #[test]
    fn test_clone_for_mirror_call_tool() {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "api/search".to_string(),
                arguments: serde_json::json!({"q": "test"}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        };

        let mirrored = clone_for_mirror(&req, "api/", "api-v2/").unwrap();
        match &mirrored.inner {
            McpRequest::CallTool(params) => {
                assert_eq!(params.name, "api-v2/search");
                assert_eq!(params.arguments, serde_json::json!({"q": "test"}));
            }
            _ => panic!("expected CallTool"),
        }
    }

    #[test]
    fn test_clone_for_mirror_read_resource() {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "api/docs/readme".to_string(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let mirrored = clone_for_mirror(&req, "api/", "mirror/").unwrap();
        match &mirrored.inner {
            McpRequest::ReadResource(params) => {
                assert_eq!(params.uri, "mirror/docs/readme");
            }
            _ => panic!("expected ReadResource"),
        }
    }

    #[test]
    fn test_clone_for_mirror_list_tools_returns_none() {
        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(Default::default()),
            extensions: Extensions::new(),
        };
        assert!(clone_for_mirror(&req, "api/", "mirror/").is_none());
    }

    #[tokio::test]
    async fn test_mirror_service_passes_through() {
        let mock = MockService::with_tools(&["api/search", "api-v2/search"]);
        let mirrors = make_mirrors("api", "api-v2", 100);
        let mut svc = MirrorService::new(mock, mirrors, "/");

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "api/search".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        // Primary response should be returned
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_mirror_service_non_mirrored_passes_through() {
        let mock = MockService::with_tools(&["other/tool"]);
        let mirrors = make_mirrors("api", "api-v2", 100);
        let mut svc = MirrorService::new(mock, mirrors, "/");

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
    async fn test_mirror_service_list_tools_not_mirrored() {
        let mock = MockService::with_tools(&["api/search"]);
        let mirrors = make_mirrors("api", "api-v2", 100);
        let mut svc = MirrorService::new(mock, mirrors, "/");

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }
}
