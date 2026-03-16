//! Canary / weighted routing middleware.
//!
//! Routes a percentage of requests to a canary backend instead of the primary.
//! The canary backend is registered as a separate backend with its own namespace,
//! but its tools are hidden from `ListTools` (via capability filtering). When a
//! request targets the primary namespace, this middleware probabilistically
//! rewrites it to target the canary namespace instead.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "api"
//! transport = "http"
//! url = "http://api-v1.internal:8080"
//! weight = 90
//!
//! [[backends]]
//! name = "api-canary"
//! transport = "http"
//! url = "http://api-v2.internal:8080"
//! weight = 10
//! canary_of = "api"  # share namespace with api
//! ```
//!
//! # How it works
//!
//! 1. Both `api` and `api-canary` are registered as separate backends
//! 2. `api-canary`'s tools are auto-hidden via capability filtering
//! 3. When `CallTool("api/search")` arrives, this middleware rolls a weighted
//!    random selection: 90% chance it passes through to `api`, 10% chance it
//!    rewrites to `CallTool("api-canary/search")`
//! 4. `ListTools` always returns only the primary's tools

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{CallToolParams, GetPromptParams, McpRequest, ReadResourceParams};

/// Tower layer that produces a [`CanaryService`].
#[derive(Clone)]
pub struct CanaryLayer {
    canaries: HashMap<String, (String, u32, u32)>,
    separator: String,
}

impl CanaryLayer {
    /// Create a new canary routing layer.
    ///
    /// `canaries` maps primary backend names to `(canary_name, primary_weight, canary_weight)`.
    pub fn new(
        canaries: HashMap<String, (String, u32, u32)>,
        separator: impl Into<String>,
    ) -> Self {
        Self {
            canaries,
            separator: separator.into(),
        }
    }
}

impl<S> Layer<S> for CanaryLayer {
    type Service = CanaryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CanaryService::new(inner, self.canaries.clone(), &self.separator)
    }
}

/// Mapping from a primary backend namespace to its canary configuration.
#[derive(Debug, Clone)]
struct CanaryMapping {
    /// Primary namespace prefix (e.g. "api/").
    primary_prefix: String,
    /// Canary namespace prefix (e.g. "api-canary/").
    canary_prefix: String,
    /// Weight of the primary (e.g. 90).
    primary_weight: u32,
    /// Total weight (primary + canary, e.g. 100).
    total_weight: u32,
    /// Atomic counter for deterministic weight-based routing.
    counter: Arc<AtomicU64>,
}

/// Canary routing middleware.
///
/// Wraps the proxy service and probabilistically rewrites requests from
/// the primary namespace to the canary namespace based on configured weights.
#[derive(Clone)]
pub struct CanaryService<S> {
    inner: S,
    mappings: Arc<Vec<CanaryMapping>>,
}

impl<S> CanaryService<S> {
    /// Create a new canary service.
    ///
    /// `canaries` maps primary backend names to `(canary_name, primary_weight, canary_weight)`.
    /// The `separator` is used to construct namespace prefixes.
    pub fn new(inner: S, canaries: HashMap<String, (String, u32, u32)>, separator: &str) -> Self {
        let mappings = canaries
            .into_iter()
            .map(
                |(primary, (canary, primary_weight, canary_weight))| CanaryMapping {
                    primary_prefix: format!("{primary}{separator}"),
                    canary_prefix: format!("{canary}{separator}"),
                    primary_weight,
                    total_weight: primary_weight + canary_weight,
                    counter: Arc::new(AtomicU64::new(0)),
                },
            )
            .collect();

        Self {
            inner,
            mappings: Arc::new(mappings),
        }
    }
}

/// Check if a request targets a primary namespace and return the mapping.
fn find_canary<'a>(name: &str, mappings: &'a [CanaryMapping]) -> Option<&'a CanaryMapping> {
    mappings
        .iter()
        .find(|m| name.starts_with(&m.primary_prefix))
}

/// Deterministic check: should this request go to the canary?
fn should_route_to_canary(mapping: &CanaryMapping) -> bool {
    let count = mapping.counter.fetch_add(1, Ordering::Relaxed);
    let position = count % mapping.total_weight as u64;
    // Primary gets the first primary_weight slots, canary gets the rest
    position >= mapping.primary_weight as u64
}

/// Rewrite a request to target the canary namespace.
fn rewrite_to_canary(req: RouterRequest, mapping: &CanaryMapping) -> RouterRequest {
    let new_inner = match req.inner {
        McpRequest::CallTool(params) if params.name.starts_with(&mapping.primary_prefix) => {
            let suffix = &params.name[mapping.primary_prefix.len()..];
            McpRequest::CallTool(CallToolParams {
                name: format!("{}{suffix}", mapping.canary_prefix),
                arguments: params.arguments,
                meta: params.meta,
                task: params.task,
            })
        }
        McpRequest::ReadResource(params) if params.uri.starts_with(&mapping.primary_prefix) => {
            let suffix = &params.uri[mapping.primary_prefix.len()..];
            McpRequest::ReadResource(ReadResourceParams {
                uri: format!("{}{suffix}", mapping.canary_prefix),
                meta: params.meta,
            })
        }
        McpRequest::GetPrompt(params) if params.name.starts_with(&mapping.primary_prefix) => {
            let suffix = &params.name[mapping.primary_prefix.len()..];
            McpRequest::GetPrompt(GetPromptParams {
                name: format!("{}{suffix}", mapping.canary_prefix),
                arguments: params.arguments,
                meta: params.meta,
            })
        }
        other => other,
    };

    RouterRequest {
        id: req.id,
        inner: new_inner,
        extensions: Extensions::new(),
    }
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

impl<S> Service<RouterRequest> for CanaryService<S>
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
        // Check if this request should be routed to a canary
        let should_canary = request_name(&req.inner)
            .and_then(|name| find_canary(name, &self.mappings))
            .filter(|mapping| should_route_to_canary(mapping))
            .cloned();

        let req = if let Some(ref mapping) = should_canary {
            tracing::debug!(
                primary = %mapping.primary_prefix,
                canary = %mapping.canary_prefix,
                "Routing request to canary backend"
            );
            rewrite_to_canary(req, mapping)
        } else {
            req
        };

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{MockService, call_service};
    use tower_mcp::protocol::RequestId;

    fn make_canaries(
        primary: &str,
        canary: &str,
        primary_weight: u32,
        canary_weight: u32,
    ) -> HashMap<String, (String, u32, u32)> {
        let mut m = HashMap::new();
        m.insert(
            primary.to_string(),
            (canary.to_string(), primary_weight, canary_weight),
        );
        m
    }

    #[test]
    fn test_find_canary_match() {
        let mappings = vec![CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 90,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        }];
        assert!(find_canary("api/search", &mappings).is_some());
        assert!(find_canary("other/search", &mappings).is_none());
    }

    #[test]
    fn test_should_route_to_canary_weights() {
        let mapping = CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 90,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };

        // Over 100 requests, exactly 10 should go to canary
        let canary_count: u32 = (0..100)
            .filter(|_| should_route_to_canary(&mapping))
            .count() as u32;
        assert_eq!(canary_count, 10);
    }

    #[test]
    fn test_should_route_to_canary_50_50() {
        let mapping = CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 50,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };

        let canary_count: u32 = (0..100)
            .filter(|_| should_route_to_canary(&mapping))
            .count() as u32;
        assert_eq!(canary_count, 50);
    }

    #[test]
    fn test_rewrite_to_canary_call_tool() {
        let mapping = CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 90,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };

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

        let rewritten = rewrite_to_canary(req, &mapping);
        match &rewritten.inner {
            McpRequest::CallTool(params) => {
                assert_eq!(params.name, "api-canary/search");
                assert_eq!(params.arguments, serde_json::json!({"q": "test"}));
            }
            _ => panic!("expected CallTool"),
        }
    }

    #[test]
    fn test_rewrite_to_canary_read_resource() {
        let mapping = CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 90,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ReadResource(ReadResourceParams {
                uri: "api/docs/readme".to_string(),
                meta: None,
            }),
            extensions: Extensions::new(),
        };

        let rewritten = rewrite_to_canary(req, &mapping);
        match &rewritten.inner {
            McpRequest::ReadResource(params) => {
                assert_eq!(params.uri, "api-canary/docs/readme");
            }
            _ => panic!("expected ReadResource"),
        }
    }

    #[test]
    fn test_rewrite_leaves_non_matching_unchanged() {
        let mapping = CanaryMapping {
            primary_prefix: "api/".to_string(),
            canary_prefix: "api-canary/".to_string(),
            primary_weight: 90,
            total_weight: 100,
            counter: Arc::new(AtomicU64::new(0)),
        };

        let req = RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(Default::default()),
            extensions: Extensions::new(),
        };

        let rewritten = rewrite_to_canary(req, &mapping);
        assert!(matches!(rewritten.inner, McpRequest::ListTools(_)));
    }

    #[tokio::test]
    async fn test_canary_service_routes_to_canary() {
        // Weight 0 primary / 100 canary = always canary
        let mock = MockService::with_tools(&["api/search", "api-canary/search"]);
        let canaries = make_canaries("api", "api-canary", 0, 100);
        let mut svc = CanaryService::new(mock, canaries, "/");

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

        // Should succeed (rewritten to api-canary/search)
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_canary_service_passes_through_primary() {
        // Weight 100 primary / 0 would panic, so use 100/1 (99% primary)
        let mock = MockService::with_tools(&["api/search"]);
        let canaries = make_canaries("api", "api-canary", 100, 1);
        let mut svc = CanaryService::new(mock, canaries, "/");

        // First request goes to primary (position 0 < 100)
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

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_canary_service_non_matching_passes_through() {
        let mock = MockService::with_tools(&["other/tool"]);
        let canaries = make_canaries("api", "api-canary", 0, 100);
        let mut svc = CanaryService::new(mock, canaries, "/");

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
    async fn test_canary_service_list_tools_not_affected() {
        let mock = MockService::with_tools(&["api/search"]);
        let canaries = make_canaries("api", "api-canary", 0, 100);
        let mut svc = CanaryService::new(mock, canaries, "/");

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }
}
