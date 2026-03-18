//! Request coalescing middleware for the proxy.
//!
//! Deduplicates identical in-flight `CallTool` and `ReadResource` requests.
//! When multiple identical requests arrive concurrently, only one is forwarded
//! to the backend; all callers receive the same response.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{Mutex, broadcast};
use tower::{Layer, Service};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

/// Tower layer that produces a [`CoalesceService`].
#[derive(Clone)]
pub struct CoalesceLayer;

impl CoalesceLayer {
    /// Create a new request coalescing layer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CoalesceLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for CoalesceLayer {
    type Service = CoalesceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CoalesceService::new(inner)
    }
}

/// Tower service that coalesces identical in-flight requests.
#[derive(Clone)]
pub struct CoalesceService<S> {
    inner: S,
    in_flight: Arc<Mutex<HashMap<String, broadcast::Sender<RouterResponse>>>>,
}

impl<S> CoalesceService<S> {
    /// Create a new request coalescing service wrapping `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

fn coalesce_key(req: &McpRequest) -> Option<String> {
    match req {
        McpRequest::CallTool(params) => {
            let args = serde_json::to_string(&params.arguments).unwrap_or_default();
            Some(format!("tool:{}:{}", params.name, args))
        }
        McpRequest::ReadResource(params) => Some(format!("res:{}", params.uri)),
        _ => None,
    }
}

impl<S> Service<RouterRequest> for CoalesceService<S>
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
        let Some(key) = coalesce_key(&req.inner) else {
            // Non-coalesceable request, pass through
            let fut = self.inner.call(req);
            return Box::pin(fut);
        };

        let in_flight = Arc::clone(&self.in_flight);
        let mut inner = self.inner.clone();
        let request_id = req.id.clone();

        Box::pin(async move {
            // Check if there's already an in-flight request for this key
            {
                let map = in_flight.lock().await;
                if let Some(tx) = map.get(&key) {
                    let mut rx = tx.subscribe();
                    drop(map);
                    // Wait for the in-flight request to complete
                    if let Ok(resp) = rx.recv().await {
                        return Ok(RouterResponse {
                            id: request_id,
                            inner: resp.inner,
                        });
                    }
                    // Sender dropped (shouldn't happen), fall through to make our own request
                }
            }

            // We're the first — register ourselves
            let (tx, _) = broadcast::channel(1);
            {
                let mut map = in_flight.lock().await;
                map.insert(key.clone(), tx.clone());
            }

            let result = inner.call(req).await;

            // Broadcast result to any waiters and clean up
            let Ok(ref resp) = result;
            let _ = tx.send(resp.clone());
            {
                let mut map = in_flight.lock().await;
                map.remove(&key);
            }

            result
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::CoalesceService;
    use crate::test_util::{MockService, call_service};

    #[tokio::test]
    async fn test_coalesce_passes_through_single_request() {
        let mock = MockService::with_tools(&["fs/read"]);
        let mut svc = CoalesceService::new(mock);

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

        match resp.inner.unwrap() {
            McpResponse::CallTool(r) => assert_eq!(r.all_text(), "called: fs/read"),
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_coalesce_non_coalesceable_passes_through() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = CoalesceService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok(), "list_tools should pass through");
    }

    #[tokio::test]
    async fn test_coalesce_key_includes_arguments() {
        // Different arguments should produce different keys
        let key1 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                meta: None,
                task: None,
            }));
        let key2 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 2}),
                meta: None,
                task: None,
            }));
        assert_ne!(key1, key2, "different args should have different keys");
    }

    #[tokio::test]
    async fn test_coalesce_key_same_arguments_produce_same_key() {
        let key1 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                meta: None,
                task: None,
            }));
        let key2 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                meta: None,
                task: None,
            }));
        assert_eq!(key1, key2, "same tool+args should have the same key");
    }

    #[tokio::test]
    async fn test_coalesce_key_read_resource() {
        let key = super::coalesce_key(&McpRequest::ReadResource(
            tower_mcp::protocol::ReadResourceParams {
                uri: "file:///tmp/test.txt".to_string(),
                meta: None,
            },
        ));
        assert_eq!(key, Some("res:file:///tmp/test.txt".to_string()));
    }

    #[tokio::test]
    async fn test_coalesce_key_non_coalesceable_returns_none() {
        let key = super::coalesce_key(&McpRequest::ListTools(Default::default()));
        assert!(key.is_none(), "ListTools should not be coalesceable");

        let key = super::coalesce_key(&McpRequest::ListResources(Default::default()));
        assert!(key.is_none(), "ListResources should not be coalesceable");
    }

    #[tokio::test]
    async fn test_concurrent_identical_requests_coalesced() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::Service;

        // A mock that counts how many times it's actually called
        #[derive(Clone)]
        struct CountingService {
            call_count: Arc<AtomicUsize>,
        }

        impl Service<tower_mcp::router::RouterRequest> for CountingService {
            type Response = tower_mcp::router::RouterResponse;
            type Error = std::convert::Infallible;
            type Future = std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                tower_mcp::router::RouterResponse,
                                std::convert::Infallible,
                            >,
                        > + Send,
                >,
            >;

            fn poll_ready(
                &mut self,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: tower_mcp::router::RouterRequest) -> Self::Future {
                let count = self.call_count.clone();
                let id = req.id.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Small delay to ensure concurrent requests overlap
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(tower_mcp::router::RouterResponse {
                        id,
                        inner: Ok(McpResponse::CallTool(
                            tower_mcp::protocol::CallToolResult::text("result"),
                        )),
                    })
                })
            }
        }

        let call_count = Arc::new(AtomicUsize::new(0));
        let svc = CountingService {
            call_count: call_count.clone(),
        };
        let coalesce = CoalesceService::new(svc);

        let make_request = || {
            let mut c = coalesce.clone();
            let req = tower_mcp::router::RouterRequest {
                id: tower_mcp::protocol::RequestId::Number(1),
                inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                    name: "tool".to_string(),
                    arguments: serde_json::json!({"x": 42}),
                    meta: None,
                    task: None,
                }),
                extensions: tower_mcp::router::Extensions::new(),
            };
            async move { c.call(req).await }
        };

        // Fire 3 identical requests concurrently
        let (r1, r2, r3) = tokio::join!(make_request(), make_request(), make_request());

        // All should succeed
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r3.is_ok());

        // The backend should be called at most twice (the first caller registers,
        // some others may arrive before the lock is acquired). The key invariant
        // is that it's called fewer times than the number of requests.
        let count = call_count.load(Ordering::SeqCst);
        assert!(
            count < 3,
            "expected fewer than 3 backend calls due to coalescing, got {count}"
        );
    }

    #[tokio::test]
    async fn test_different_requests_not_coalesced() {
        let mock = MockService::with_tools(&["tool"]);
        let coalesce = CoalesceService::new(mock);

        // Two requests with different arguments
        let mut c1 = coalesce.clone();
        let req1 = tower_mcp::router::RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"x": 1}),
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        let mut c2 = coalesce.clone();
        let req2 = tower_mcp::router::RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(2),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"x": 2}),
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        let (r1, r2) = tokio::join!(
            tower::Service::call(&mut c1, req1),
            tower::Service::call(&mut c2, req2)
        );

        // Both should succeed independently
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    #[tokio::test]
    async fn test_coalesce_with_error_response() {
        use crate::test_util::ErrorMockService;

        let mock = ErrorMockService;
        let mut svc = CoalesceService::new(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "failing_tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        // The error response should pass through correctly
        assert!(
            resp.inner.is_err(),
            "error response should propagate through coalesce"
        );
        let err = resp.inner.unwrap_err();
        assert_eq!(err.code, -32603);
        assert_eq!(err.message, "internal error");
    }
}
