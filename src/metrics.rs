//! Prometheus metrics middleware for the proxy.
//!
//! Request counters use MCP method, coarse status and the bounded
//! [`crate::outcome`] class. Duration histograms use MCP method. Tool `isError`
//! results count as errors without recording response bodies. Configured
//! requests add a bounded `namespace` label: `backend:<configured name>` or
//! `_aggregate`, `_management`, `_composite`, `_unknown`. It describes the
//! requested target, not a routed execution destination. No tool/user labels.
//!
//! [`MetricsRecorder`] owns Prometheus state without installing a global
//! recorder. Proxies use independent recorders or share an injected owner.
//! Each completion emits synchronously into its explicit owner, so async thread
//! migration cannot switch recorders. Maintenance runs on completions at most once per second
//! and before scraping/rendering, without a detached worker. Low-level
//! [`MetricsService::new`] retains facade global/thread-local behavior;
//! configured proxies use [`MetricsService::with_recorder`] instead.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use metrics::{counter, histogram};
use tower::{Layer, Service};
use tower_mcp::{RouterRequest, RouterResponse};

/// Shareable, explicitly owned Prometheus recorder for configured proxies.
#[derive(Clone)]
pub struct MetricsRecorder {
    recorder: Arc<metrics_exporter_prometheus::PrometheusRecorder>,
    upkeep: Arc<Mutex<Instant>>,
}
impl Default for MetricsRecorder {
    fn default() -> Self {
        Self::new(metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder())
    }
}
impl MetricsRecorder {
    /// Own a custom-built recorder without changing process-global state.
    pub fn new(recorder: metrics_exporter_prometheus::PrometheusRecorder) -> Self {
        Self {
            recorder: Arc::new(recorder),
            upkeep: Arc::new(Mutex::new(Instant::now())),
        }
    }
    /// Return a shared scrape handle; call `run_upkeep` before rendering it.
    pub fn handle(&self) -> metrics_exporter_prometheus::PrometheusHandle {
        self.recorder.handle()
    }
    /// Perform maintenance and render this recorder's Prometheus metrics.
    pub fn render(&self) -> String {
        let handle = self.handle();
        handle.run_upkeep();
        handle.render()
    }
    fn maintain(&self) {
        if let Ok(mut last) = self.upkeep.try_lock()
            && last.elapsed() >= std::time::Duration::from_secs(1)
        {
            *last = Instant::now();
            self.handle().run_upkeep();
        }
    }
}

/// Tower layer that produces a [`MetricsService`].
#[derive(Clone, Default)]
pub struct MetricsLayer;

impl MetricsLayer {
    /// Create a new metrics layer.
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService::new(inner)
    }
}

/// Tower service that records request metrics.
#[derive(Clone)]
pub struct MetricsService<S> {
    inner: S,
    recorder: Option<MetricsRecorder>,
    namespaces: crate::telemetry::Namespaces,
}

impl<S> MetricsService<S> {
    /// Create a new metrics service wrapping `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            recorder: None,
            namespaces: Default::default(),
        }
    }
    /// Use an owned/shared recorder and bounded configured namespaces.
    pub fn with_recorder(inner: S, recorder: MetricsRecorder, config: &crate::ProxyConfig) -> Self {
        Self {
            inner,
            recorder: Some(recorder),
            namespaces: crate::telemetry::Namespaces::new(config),
        }
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
        let method = crate::telemetry::method(&req.inner).to_string();
        let namespace = self.namespaces.requested(&req.inner);
        let recorder = self.recorder.clone();
        let start = Instant::now();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let result = fut.await;
            let duration = start.elapsed().as_secs_f64();

            let outcome = crate::outcome::Outcome::from_result(&result);
            let record = || {
                counter!("mcp_proxy_requests_total","method"=>method.clone(),"namespace"=>namespace.clone(),"status"=>outcome.status(),"outcome"=>outcome.as_str()).increment(1);
                histogram!("mcp_proxy_request_duration_seconds","method"=>method,"namespace"=>namespace).record(duration);
            };
            if let Some(recorder) = recorder {
                // The guard covers only these synchronous emissions, no await.
                metrics::with_local_recorder(recorder.recorder.as_ref(), record);
                recorder.maintain();
            } else {
                record();
            }

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
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_metrics_records_error_responses() {
        let mock = crate::test_util::ErrorMockService;
        let mut svc = MetricsService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        // ErrorMockService returns a JSON-RPC error; metrics should record status="error"
        assert!(resp.inner.is_err());
    }

    #[tokio::test]
    async fn test_metrics_handles_ping() {
        let mock = MockService::with_tools(&[]);
        let mut svc = MetricsService::new(mock);

        let resp = call_service(&mut svc, McpRequest::Ping).await;
        assert!(resp.inner.is_ok());
    }
}
