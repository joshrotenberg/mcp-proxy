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
use tower::Service;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

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
}
