//! Request tracing and telemetry ownership.
//!
//! Configured proxies emit `mcp.request` and `http.request` spans on the
//! `mcp::trace` target when tracing is enabled. MCP spans contain only the
//! protocol method, bounded requested namespace and numeric/typed outcome class.
//! Configured namespaces use `backend:<name>`; fixed `_aggregate`, `_management`,
//! `_composite` and `_unknown` labels cannot collide with backend names.
//! HTTP spans contain a coarse route, method and response status, including auth
//! rejections. Arguments, tool names, headers, request IDs and error bodies are
//! omitted. HTTP success describes transport status; MCP outcomes describe the
//! protocol result. Namespace attribution describes the public requested target,
//! not the actual destination of canary/failover/mirror/composite execution.
//! HTTP spans end when response headers are ready, not when SSE/body streaming
//! finishes. Cross-process trace-header extraction/injection is not provided.
//!
//! Enable tracing in configuration:
//!
//! ```toml
//! [observability.tracing]
//! enabled = true
//! endpoint = "http://localhost:4318/v1/traces"
//! service_name = "mcp-proxy"
//! ```
//!
//! The CLI owns its OTLP provider, exports only this target, and explicitly
//! shuts it down after proxy cleanup (also on startup/serve errors). The default
//! is OTLP HTTP/protobuf at `http://localhost:4318/v1/traces`; configured endpoints
//! are complete trace URLs, used as supplied. Migrate gRPC port4317 examples to
//! HTTP port4318 plus `/v1/traces`. Collector errors have a two-second request
//! timeout; CLI shutdown allows three seconds for exporter cleanup. SIGKILL,
//! process abort and collector failure cannot guarantee delivery.
//!
//! Library construction never installs a subscriber or global tracer provider.
//! Embedders own their subscriber/provider and may share it across proxies.
//! With feature `otel`, `tracer_provider` constructs the same bounded exporter;
//! obtain a tracer, attach `tracing_opentelemetry::layer()` filtered to
//! `mcp::trace`, retain the provider, and call `shutdown_with_timeout` on a blocking
//! worker after draining requests and shutting down proxies. Keep the subscriber
//! enabled for this INFO target independently of general application verbosity.
//! Host subscribers may otherwise attach their own events/fields; the CLI's
//! target filter prevents audit/access/application events entering exported spans.
//!
//! Metrics-enabled proxies use independent Prometheus recorders by default;
//! no process-global recorder is installed. See [`crate::Proxy`] for the
//! feature-gated `from_config_with_metrics` injection API and the `metrics`
//! module for shared recorder construction. Sharing intentionally aggregates
//! identical method/namespace series across proxies. Existing dashboards must
//! account for the new bounded `namespace` label. There are no per-user/tool
//! labels, live backend-health gauges, or claims of executed-backend attribution.
//! Dynamic namespaces absent from startup configuration map to `_unknown` until
//! restart; search-wrapper requests attribute to `_management`, not the target.
//! MCP timing starts at dispatch; outer HTTP spans also cover initial admission.
//!
//! ```rust,no_run
//! # #[cfg(feature="metrics")]
//! # async fn embed(first_config:mcp_proxy::ProxyConfig,second_config:mcp_proxy::ProxyConfig)->anyhow::Result<()> {
//! let owner=mcp_proxy::metrics::MetricsRecorder::default();
//! let first=mcp_proxy::Proxy::from_config_with_metrics(first_config,owner.clone()).await?;
//! let second=mcp_proxy::Proxy::from_config_with_metrics(second_config,owner.clone()).await?;
//! // Mount routers in the host, retaining shutdown handles before consuming.
//! let metrics=owner.render();
//! # drop((first,second,metrics));
//! # Ok(()) }
//! ```

use std::{
    collections::HashSet,
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
};
use tower::Service;
use tower_mcp::{McpRequest, RouterRequest, RouterResponse};
use tracing::Instrument;

pub(crate) fn method(request: &McpRequest) -> &str {
    if matches!(request, McpRequest::Unknown { .. }) {
        "_unknown"
    } else {
        request.method_name()
    }
}

#[derive(Clone, Default)]
pub(crate) struct Namespaces {
    separator: String,
    backends: Arc<HashSet<String>>,
    composites: Arc<HashSet<String>>,
}
impl Namespaces {
    pub(crate) fn new(config: &crate::ProxyConfig) -> Self {
        Self {
            separator: config.proxy.separator.clone(),
            backends: Arc::new(config.backends.iter().map(|b| b.name.clone()).collect()),
            composites: Arc::new(
                config
                    .composite_tools
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
            ),
        }
    }
    pub(crate) fn requested(&self, request: &McpRequest) -> String {
        let target = match request {
            McpRequest::CallTool(p) => Some(p.name.as_str()),
            McpRequest::ReadResource(p) => Some(p.uri.as_str()),
            McpRequest::GetPrompt(p) => Some(p.name.as_str()),
            _ => None,
        };
        let Some(target) = target else {
            return "_aggregate".into();
        };
        if self.composites.contains(target) {
            return "_composite".into();
        }
        if !self.separator.is_empty()
            && let Some((prefix, _)) = target.split_once(&self.separator)
        {
            if prefix == "proxy" {
                return "_management".into();
            }
            if self.backends.contains(prefix) {
                return format!("backend:{prefix}");
            }
        }
        "_unknown".into()
    }
}

#[derive(Clone)]
pub(crate) struct RequestSpans<S> {
    inner: S,
    namespaces: Namespaces,
}

struct Completion {
    span: tracing::Span,
    finished: bool,
}
impl Completion {
    fn mcp_result(&mut self, outcome: crate::outcome::Outcome) {
        self.span.record("mcp.outcome", outcome.as_str());
        self.span.record(
            "otel.status_code",
            if outcome.status() == "ok" {
                "OK"
            } else {
                "ERROR"
            },
        );
        self.finished = true;
    }
    fn http_result(&mut self, status: axum::http::StatusCode) {
        self.span
            .record("http.status_code", i64::from(status.as_u16()));
        self.span.record(
            "otel.status_code",
            if status.is_client_error() || status.is_server_error() {
                "ERROR"
            } else {
                "OK"
            },
        );
        self.finished = true;
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        if !self.finished {
            self.span.record("mcp.outcome", "cancelled");
            self.span.record("otel.status_code", "ERROR");
        }
    }
}
impl<S> RequestSpans<S> {
    pub(crate) fn new(inner: S, config: &crate::ProxyConfig) -> Self {
        Self {
            inner,
            namespaces: Namespaces::new(config),
        }
    }
}
impl<S> Service<RouterRequest> for RequestSpans<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send + 'static,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RouterResponse, Infallible>> + Send>,
    >;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let namespace = self.namespaces.requested(&req.inner);
        let span = tracing::info_span!(target:"mcp::trace","mcp.request",
            mcp.method=method(&req.inner),mcp.namespace=%namespace,
            mcp.outcome=tracing::field::Empty,otel.kind="server",otel.status_code=tracing::field::Empty);
        let mut completion = Completion {
            span: span.clone(),
            finished: false,
        };
        let future = self.inner.call(req);
        Box::pin(
            async move {
                let result = future.await;
                let outcome = crate::outcome::Outcome::from_result(&result);
                completion.mcp_result(outcome);
                result
            }
            .instrument(span),
        )
    }
}

pub(crate) fn http_spans(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::from_fn(|req:axum::extract::Request,next:axum::middleware::Next| async move {
        let route=match req.uri().path() {
            "/"=>"mcp", "/livez"=>"livez", "/readyz"=>"readyz",
            path if path.starts_with("/admin/")=>"admin", _=>"other",
        };
        let method=match req.method().as_str() { "GET"=>"GET","POST"=>"POST","PUT"=>"PUT","DELETE"=>"DELETE","OPTIONS"=>"OPTIONS","HEAD"=>"HEAD","PATCH"=>"PATCH",_=>"other" };
        let span=tracing::info_span!(target:"mcp::trace","http.request",http.method=method,http.route=route,
                http.status_code=tracing::field::Empty,otel.kind="server",otel.status_code=tracing::field::Empty);
        let mut completion=Completion {span:span.clone(),finished:false};
        async move {
            let response=next.run(req).await;
            completion.http_result(response.status());
            response
        }.instrument(span).await
    }))
}

/// Build an owned OTLP HTTP/protobuf provider without installing global state.
///
/// The caller owns subscriber attachment and must explicitly flush/shut down
/// after draining requests. Run blocking SDK shutdown methods on a blocking
/// worker, keeping the async collector/runtime runnable. Endpoints are complete
/// trace URLs; SDK OTLP environment settings may configure headers/TLS.
#[cfg(feature = "otel")]
pub fn tracer_provider(
    config: &crate::config::TracingConfig,
) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&config.endpoint)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|_| anyhow::anyhow!("building OTLP span exporter failed"))?;
    Ok(opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(config.service_name.clone())
                .build(),
        )
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> crate::ProxyConfig {
        crate::ProxyConfig::parse("[proxy]\nname='labels'\nseparator='::'\n[proxy.listen]\n[[backends]]\nname='api'\ntransport='stdio'\ncommand='echo'\n[[composite_tools]]\nname='bundle'\ndescription='bundle'\ntools=['api::echo']").unwrap()
    }
    #[test]
    fn namespace_and_method_cardinality_exclude_untrusted_names() {
        let config = config();
        assert_eq!(
            config.observability.tracing.endpoint,
            "http://localhost:4318/v1/traces"
        );
        let namespaces = Namespaces::new(&config);
        for (name, expected) in [
            ("api::anything", "backend:api"),
            ("proxy::config", "_management"),
            ("bundle", "_composite"),
        ] {
            let request = McpRequest::CallTool(
                serde_json::from_value(serde_json::json!({"name":name,"arguments":{}})).unwrap(),
            );
            assert_eq!(namespaces.requested(&request), expected);
        }
        assert_eq!(
            namespaces.requested(&McpRequest::ListTools(Default::default())),
            "_aggregate"
        );
        let mut unusual: crate::ProxyConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        unusual.composite_tools.clear();
        unusual.backends[0].name = "_unknown".into();
        unusual.prepare().unwrap();
        let request = McpRequest::CallTool(
            serde_json::from_value(serde_json::json!({"name":"_unknown::echo","arguments":{}}))
                .unwrap(),
        );
        assert_eq!(
            Namespaces::new(&unusual).requested(&request),
            "backend:_unknown"
        );
        for i in 0..1024 {
            let request = McpRequest::Unknown {
                method: format!("private-{i}"),
                params: None,
            };
            assert_eq!(method(&request), "_unknown");
            let request = McpRequest::CallTool(
                serde_json::from_value(
                    serde_json::json!({"name":format!("private-{i}::secret"),"arguments":{}}),
                )
                .unwrap(),
            );
            assert_eq!(namespaces.requested(&request), "_unknown");
        }
    }
    #[cfg(feature = "otel")]
    #[test]
    fn dropping_unpolled_request_ends_one_cancelled_span() {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
        use std::sync::Mutex;
        use tracing_subscriber::layer::SubscriberExt;
        #[derive(Debug, Clone, Default)]
        struct Capture(Arc<Mutex<Vec<SpanData>>>);
        impl SpanExporter for Capture {
            async fn export(
                &self,
                batch: Vec<SpanData>,
            ) -> opentelemetry_sdk::error::OTelSdkResult {
                self.0.lock().unwrap().extend(batch);
                Ok(())
            }
        }
        let capture = Capture::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(capture.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("cancellation")));
        tracing::subscriber::with_default(subscriber, || {
            let inner = tower::service_fn(|_: RouterRequest| {
                std::future::pending::<Result<RouterResponse, Infallible>>()
            });
            let mut service = RequestSpans::new(inner, &config());
            let future = service.call(RouterRequest {
                id: tower_mcp::RequestId::Number(1),
                inner: McpRequest::Ping,
                extensions: tower_mcp::Extensions::new(),
            });
            assert!(capture.0.lock().unwrap().is_empty());
            drop(future);
        });
        provider.shutdown().unwrap();
        let spans = capture.0.lock().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0]
                .attributes
                .iter()
                .any(|a| a.key.as_str() == "mcp.outcome" && a.value.as_str() == "cancelled")
        );
        assert!(matches!(
            spans[0].status,
            opentelemetry::trace::Status::Error { .. }
        ));
    }
}
