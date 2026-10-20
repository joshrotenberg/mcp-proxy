//! Real configured calls and backend ownership preserve outlier fleet bounds.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;

use mcp_proxy::config::OutlierDetectionConfig;
use mcp_proxy::outlier::{OutlierDetectionLayer, OutlierDetector};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use tower_mcp::protocol::{McpRequest, McpResponse};
use tower_mcp::proxy::McpProxy;
use tower_mcp::{
    ChannelTransport, HttpTransport, McpRouter, RouterRequest, RouterResponse, ToolBuilder,
};

fn router() -> McpRouter {
    McpRouter::new().tool(
        ToolBuilder::new("work")
            .handler(|_: serde_json::Value| async {
                Ok(tower_mcp::CallToolResult::text("healthy"))
            })
            .build(),
    )
}

#[tokio::test]
async fn configured_competing_failures_eject_exactly_one_backend_and_preserve_rpc_errors() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut servers = Vec::new();
    let mut counts = Vec::new();
    for _ in 0..2 {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let barrier = barrier.clone();
        let router = router();
        let backend = tower::service_fn(move |request: RouterRequest| {
            let router = router.clone();
            let calls = captured.clone();
            let barrier = barrier.clone();
            async move {
                if matches!(&request.inner, McpRequest::CallTool(_)) {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        barrier.wait().await;
                    }
                    Ok::<_, std::convert::Infallible>(RouterResponse {
                        id: request.id,
                        inner: Err(tower_mcp_types::JsonRpcError::internal_error(
                            "controlled server failure",
                        )),
                    })
                } else {
                    router.oneshot(request).await
                }
            }
        });
        servers.push(http::Server::start(HttpTransport::from_service(backend).into_router()).await);
        counts.push(calls);
    }
    let mut config = String::from(
        "[proxy]\nname='outlier-runtime'\n[proxy.listen]\n[auth]\ntype='bearer'\ntokens=['data']\n",
    );
    for (name, server) in ["one", "two"].into_iter().zip(&servers) {
        config.push_str(&format!("[[backends]]\nname='{name}'\ntransport='http'\nurl='{}'\n[backends.outlier_detection]\nconsecutive_errors=1\nbase_ejection_seconds=3600\nmax_ejection_percent=50\n", server.url));
    }
    let mut app = app::App::embedded(&config).await;
    let session = http::Session::start(&app.url, "data").await;
    let listed = session.rpc("tools/list", json!({})).await;
    let tools = http::success(&listed)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(tools, ["one/work", "two/work"].into_iter().collect());
    let (one, two) = tokio::join!(
        session.call("one/work", json!({})),
        session.call("two/work", json!({}))
    );
    for response in [one, two] {
        assert_eq!(response["error"]["code"], -32603);
    }
    let mut ejected = 0;
    for (name, calls) in ["one/work", "two/work"].into_iter().zip(counts) {
        let response = session.call(name, json!({})).await;
        match response["error"]["code"].as_i64().unwrap() {
            -32000 => {
                ejected += 1;
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            -32603 => assert_eq!(calls.load(Ordering::SeqCst), 2),
            code => panic!("unexpected response code {code}"),
        }
    }
    assert_eq!(ejected, 1, "fleet quota must block a second ejection");
    session.terminate().await;
    app.shutdown().await;
    for server in &mut servers {
        server.stop().await;
    }
}

#[tokio::test]
async fn removing_ejected_backend_releases_registration_before_reusing_its_namespace() {
    let detector = OutlierDetector::new(100);
    let layer = tower::ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "api".into(),
            OutlierDetectionConfig {
                consecutive_errors: 1,
                interval_seconds: 10,
                base_ejection_seconds: 3600,
                max_ejection_percent: 100,
            },
            detector.clone(),
        ))
        .map_response(|mut response: RouterResponse| {
            if matches!(&response.inner, Ok(McpResponse::CallTool(_))) {
                response.inner = Err(tower_mcp_types::JsonRpcError::internal_error(
                    "controlled failure",
                ));
            }
            response
        });
    let proxy = McpProxy::builder("outlier-lifetime", "1")
        .separator("/")
        .backend("api", ChannelTransport::new(router()))
        .await
        .backend_layer(layer.clone())
        .build_strict()
        .await
        .unwrap();
    for generation in 0..2 {
        assert_eq!(detector.total_backends(), 1);
        assert_eq!(detector.ejected_count(), 0);
        let request = RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(generation),
            inner: McpRequest::CallTool(
                serde_json::from_value(json!({"name":"api/work", "arguments":{}})).unwrap(),
            ),
            extensions: Default::default(),
        };
        let response = proxy.clone().oneshot(request).await.unwrap();
        assert_eq!(response.inner.unwrap_err().code, -32603);
        assert_eq!(detector.ejected_count(), 1);
        assert!(proxy.remove_backend("api").await);
        assert_eq!(detector.total_backends(), 0);
        assert_eq!(detector.ejected_count(), 0);
        if generation == 0 {
            proxy
                .add_backend_with_layer("api", ChannelTransport::new(router()), layer.clone())
                .await
                .unwrap();
        }
    }
}
