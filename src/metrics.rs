//! Prometheus metrics middleware for the gateway.
//!
//! Records per-request counters and duration histograms, labeled by
//! MCP method and backend namespace.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use metrics::{counter, histogram};
use tower::Service;
use tower_mcp::{RouterRequest, RouterResponse};

/// Tower service that records request metrics.
#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
}

impl<S> MetricsService<S> {
    /// Create a new metrics service wrapping `inner`.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<RouterRequest> for MetricsService<S>
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
        let method = req.inner.method_name().to_string();
        let start = Instant::now();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let result = fut.await;
            let duration = start.elapsed().as_secs_f64();

            let status = match &result {
                Ok(resp) => {
                    if resp.inner.is_ok() {
                        "ok"
                    } else {
                        "error"
                    }
                }
                Err(_) => "error",
            };

            counter!("mcp_gateway_requests_total", "method" => method.clone(), "status" => status)
                .increment(1);
            histogram!(
                "mcp_gateway_request_duration_seconds",
                "method" => method,
            )
            .record(duration);

            result
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::McpRequest;

    use super::MetricsService;
    use crate::test_util::{MockService, call_service};

    #[tokio::test]
    async fn test_metrics_passes_through_request() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = MetricsService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_metrics_passes_through_tool_call() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = MetricsService::new(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }
}
