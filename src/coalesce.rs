//! Request coalescing middleware for the gateway.
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
