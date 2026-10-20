//! Real collector exports and explicitly owned/shared embedded telemetry.
#![cfg(any(feature = "otel", feature = "metrics"))]
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;
use serde_json::{Value, json};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

const PRIVATE: &str = "private-290-token-arguments-response-target";

async fn backend() -> http::Server {
    let mut router = McpRouter::new();
    for name in ["ok", "blocked"] {
        router = router.tool(
            ToolBuilder::new(name)
                .handler(|_: Value| async { Ok(CallToolResult::text("ok")) })
                .build(),
        );
    }
    router = router.tool(
        ToolBuilder::new("tool_error")
            .handler(|_: Value| async { Ok(CallToolResult::error(PRIVATE)) })
            .build(),
    );
    router = router.tool(
        ToolBuilder::new("rpc_error")
            .handler(|_: Value| async { Err(tower_mcp::Error::internal(PRIVATE)) })
            .build(),
    );
    use tower::ServiceExt;
    let service = tower::service_fn(move |req: tower_mcp::RouterRequest| {
        let router = router.clone();
        async move {
            if matches!(&req.inner,tower_mcp::McpRequest::CallTool(p) if p.name=="rpc_error") {
                Ok(tower_mcp::RouterResponse {
                    id: req.id,
                    inner: Err(tower_mcp_types::JsonRpcError::internal_error(PRIVATE)),
                })
            } else {
                router.oneshot(req).await
            }
        }
    });
    http::Server::start(HttpTransport::from_service(service).into_router()).await
}
fn config(url: &str, namespace: &str, metrics: bool) -> String {
    format!(
        "[proxy]\nname='telemetry'\n[proxy.listen]\n[auth]\ntype='bearer'\ntokens=['{PRIVATE}']\n[security]\nadmin_token='manager'\n[observability]\naudit=true\nlog_level='warn'\n[observability.access_log]\nenabled=true\n[observability.metrics]\nenabled={metrics}\n[[backends]]\nname='{namespace}'\ntransport='http'\nurl='{url}'\nhide_tools=['blocked']\n"
    )
}
async fn exercise(url: &str, namespace: &str) {
    let session = http::Session::start(url, PRIVATE).await;
    let listed = session.rpc("tools/list", json!({})).await;
    assert_eq!(http::success(&listed)["tools"].as_array().unwrap().len(), 3);
    assert_eq!(
        http::success(
            &session
                .call(&format!("{namespace}/ok"), json!({"secret":PRIVATE}))
                .await
        )["content"][0]["text"],
        "ok"
    );
    assert_eq!(
        session
            .call(&format!("{namespace}/tool_error"), json!({}))
            .await["result"]["isError"],
        true
    );
    assert_eq!(
        session
            .call(&format!("{namespace}/rpc_error"), json!({}))
            .await["error"]["code"],
        -32603
    );
    assert_eq!(
        session
            .call(&format!("{namespace}/blocked"), json!({}))
            .await["error"]["code"],
        -32602
    );
    // Malicious unknown namespace/tool names must not become telemetry labels.
    assert!(
        session
            .call(&format!("{PRIVATE}/tool-{PRIVATE}"), json!({}))
            .await
            .get("error")
            .is_some()
    );
    assert_eq!(
        session.rpc(&format!("unknown-{PRIVATE}"), json!({})).await["error"]["code"],
        -32601
    );
    let response = http::client()
        .post(url)
        .bearer_auth("untrusted-token")
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":http::initialization()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    session.terminate().await;
}

#[cfg(feature = "otel")]
mod traces {
    use super::*;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_proto::tonic::{
        collector::trace::v1::ExportTraceServiceRequest, common::v1::any_value::Value as Attribute,
        trace::v1::Span,
    };
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{layer::SubscriberExt, prelude::*};
    type Batches = Arc<Mutex<Vec<ExportTraceServiceRequest>>>;
    async fn collector() -> (http::Server, Batches) {
        let batches = Batches::default();
        let received = batches.clone();
        let server = http::Server::start(axum::Router::new().route(
            "/v1/traces",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let received = received.clone();
                    async move {
                        assert_eq!(headers["content-type"], "application/x-protobuf");
                        received
                            .lock()
                            .unwrap()
                            .push(ExportTraceServiceRequest::decode(body).unwrap());
                        (
                            [("content-type", "application/x-protobuf")],
                            Vec::<u8>::new(),
                        )
                    }
                },
            ),
        ))
        .await;
        (server, batches)
    }
    fn spans(batches: &Batches) -> Vec<Span> {
        batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| b.resource_spans.iter())
            .flat_map(|r| r.scope_spans.iter())
            .flat_map(|s| s.spans.clone())
            .collect()
    }
    fn attribute<'a>(span: &'a Span, key: &str) -> Option<&'a Attribute> {
        span.attributes
            .iter()
            .find(|a| a.key == key)
            .and_then(|a| a.value.as_ref())
            .and_then(|a| a.value.as_ref())
    }
    fn assert_exports(batches: &Batches, namespaces: &[&str]) {
        let data = spans(batches);
        assert!(!data.is_empty(), "collector received no decoded spans");
        for namespace in namespaces {
            for outcome in ["success", "tool_error", "server_error", "client_error"] {
                let span = data
                    .iter()
                    .find(|s| {
                        s.name == "mcp.request"
                            && attribute(s, "mcp.namespace")
                                == Some(&Attribute::StringValue(format!("backend:{namespace}")))
                            && attribute(s, "mcp.outcome")
                                == Some(&Attribute::StringValue(outcome.into()))
                    })
                    .expect("missing classified MCP request span");
                assert_eq!(
                    span.status.as_ref().unwrap().code,
                    if outcome == "success" { 1 } else { 2 }
                );
                assert!(span.end_time_unix_nano >= span.start_time_unix_nano);
                assert_eq!(span.kind, 2, "server span");
            }
        }
        assert!(data.iter().any(|s| s.name == "http.request"
            && attribute(s, "http.status_code") == Some(&Attribute::IntValue(401))
            && s.status.as_ref().unwrap().code == 2));
        let dump = format!("{:?}", batches.lock().unwrap());
        for private in [
            PRIVATE,
            "untrusted-token",
            "/blocked",
            "/rpc_error",
            "/tool_error",
            "/ok",
        ] {
            assert!(
                !dump.contains(private),
                "private or unbounded target entered exported spans: {private}"
            );
        }
        assert!(
            data.iter().all(|s| s.events.is_empty()),
            "audit/access/application events must not enter exports"
        );
    }
    #[tokio::test]
    async fn cli_exports_queued_spans_on_sigterm_with_http_rejections_and_safe_outcomes() {
        let mut backend = backend().await;
        let (mut collector, batches) = collector().await;
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let config = format!(
            "{}\n[observability.tracing]\nenabled=true\nendpoint='{}/v1/traces'\nservice_name='cli-telemetry'\n",
            config(&backend.url, "api", false).replacen(
                "[proxy.listen]",
                &format!("[proxy.listen]\nport={port}"),
                1
            ),
            collector.url
        );
        drop(reservation);
        // No batch can be sent on a periodic timer during this short test.
        let mut app =
            app::App::cli_with_env(&config, port, &[("OTEL_BSP_SCHEDULE_DELAY", "60000")]).await;
        exercise(&app.url, "api").await;
        assert!(
            batches.lock().unwrap().is_empty(),
            "unexpected periodic export before shutdown"
        );
        app.shutdown().await;
        assert_exports(&batches, &["api"]);
        collector.stop().await;
        backend.stop().await;
    }
    #[tokio::test]
    async fn embedded_proxies_share_host_provider_and_explicit_shutdown_flushes() {
        let mut backend = backend().await;
        let (mut collector, batches) = collector().await;
        let trace = mcp_proxy::config::TracingConfig {
            enabled: true,
            endpoint: format!("{}/v1/traces", collector.url),
            service_name: "embedded-host".into(),
        };
        let provider = mcp_proxy::telemetry::tracer_provider(&trace).unwrap();
        let layer = tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("embedded-host"))
            .with_filter(tracing_subscriber::filter::filter_fn(|m| {
                m.target() == "mcp::trace"
            }));
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            .unwrap();
        for namespace in ["one", "two"] {
            let text = format!(
                "{}\n[observability.tracing]\nenabled=true\nendpoint='{}/v1/traces'\n",
                config(&backend.url, namespace, false),
                collector.url
            );
            let mut app = app::App::embedded(&text).await;
            exercise(&app.url, namespace).await;
            app.shutdown().await;
        }
        tokio::task::spawn_blocking(move || {
            provider.shutdown_with_timeout(std::time::Duration::from_secs(3))
        })
        .await
        .unwrap()
        .unwrap();
        assert_exports(&batches, &["one", "two"]);
        collector.stop().await;
        backend.stop().await;
    }
}

#[cfg(feature = "metrics")]
mod metrics {
    use super::*;
    use mcp_proxy::{Proxy, ProxyConfig, metrics::MetricsRecorder};
    #[tokio::test]
    async fn injecting_metrics_into_disabled_config_is_rejected_before_connecting() {
        let config = ProxyConfig::parse(&config("http://127.0.0.1:1", "api", false)).unwrap();
        let result = Proxy::from_config_with_metrics(config, MetricsRecorder::default()).await;
        assert!(
            matches!(result,Err(error) if error.to_string().contains("injected metrics require"))
        );
    }
    async fn start(
        text: &str,
        owner: Option<MetricsRecorder>,
    ) -> (http::Server, mcp_proxy::ShutdownHandle) {
        let config = ProxyConfig::parse(text).unwrap();
        let proxy = if let Some(owner) = owner {
            Proxy::from_config_with_metrics(config, owner).await
        } else {
            Proxy::from_config(config).await
        }
        .unwrap();
        let shutdown = proxy.shutdown_handle();
        (http::Server::start(proxy.into_router().0).await, shutdown)
    }
    async fn scrape(url: &str) -> String {
        let response = http::client()
            .get(format!("{url}/admin/metrics"))
            .bearer_auth("manager")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.text().await.unwrap()
    }
    fn count(text: &str, namespace: &str, outcome: &str) -> u64 {
        text.lines()
            .filter(|line| {
                line.starts_with("mcp_proxy_requests_total{")
                    && line.contains("method=\"tools/call\"")
                    && line.contains(&format!(
                        "namespace=\"{}\"",
                        if namespace.starts_with('_') {
                            namespace.to_string()
                        } else {
                            format!("backend:{namespace}")
                        }
                    ))
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .map(|line| {
                line.split_whitespace()
                    .last()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .sum()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_metrics_enabled_proxies_have_independent_recorders() {
        let mut backend = backend().await;
        let config = config(&backend.url, "api", true);
        let ((mut one, s1), (mut two, s2)) =
            tokio::join!(start(&config, None), start(&config, None));
        exercise(&one.url, "api").await;
        let first = scrape(&one.url).await;
        let second = scrape(&two.url).await;
        for outcome in ["success", "tool_error", "server_error", "client_error"] {
            assert_eq!(count(&first, "api", outcome), 1);
            assert_eq!(count(&second, "api", outcome), 0);
        }
        assert!(
            count(&first, "_unknown", "server_error") + count(&first, "_unknown", "client_error")
                > 0
        );
        assert!(!first.contains(PRIVATE));
        exercise(&two.url, "api").await;
        assert_eq!(count(&scrape(&one.url).await, "api", "success"), 1);
        assert_eq!(count(&scrape(&two.url).await, "api", "success"), 1);
        s1.shutdown().await.unwrap();
        s2.shutdown().await.unwrap();
        one.stop().await;
        two.stop().await;
        backend.stop().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_custom_recorder_is_shared_across_async_proxies() {
        let mut backend = backend().await;
        let recorder = MetricsRecorder::new(
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .add_global_label("deployment", "shared")
                .build_recorder(),
        );
        let config = config(&backend.url, "api", true);
        let ((mut one, s1), (mut two, s2)) = tokio::join!(
            start(&config, Some(recorder.clone())),
            start(&config, Some(recorder.clone()))
        );
        tokio::join!(exercise(&one.url, "api"), exercise(&two.url, "api"));
        for text in [
            scrape(&one.url).await,
            scrape(&two.url).await,
            recorder.render(),
        ] {
            assert_eq!(count(&text, "api", "success"), 2);
            assert_eq!(count(&text, "api", "tool_error"), 2);
            assert!(text.contains("deployment=\"shared\""));
            assert!(text.contains("mcp_proxy_request_duration_seconds"));
            assert!(!text.contains(PRIVATE));
        }
        s1.shutdown().await.unwrap();
        s2.shutdown().await.unwrap();
        one.stop().await;
        two.stop().await;
        backend.stop().await;
    }
}
