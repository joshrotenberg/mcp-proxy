//! Verify configured authentication headers on actual backend connections.
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use tower::{Service, ServiceExt};
use tower_mcp::protocol::{CallToolParams, McpRequest, RequestId};
use tower_mcp::router::{Extensions, RouterRequest};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

#[tokio::test]
async fn http_headers_reach_initialization_and_tool_calls() {
    let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
    let capture = captured.clone();
    let backend = McpRouter::new().tool(
        ToolBuilder::new("ping")
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("pong")) })
            .build(),
    );
    let router = tower_mcp::transport::http::HttpTransport::new(backend)
        .into_router()
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let capture = capture.clone();
                async move {
                    capture.lock().unwrap().push(req.headers().clone());
                    next.run(req).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
        [proxy]
        name = "headers-test"
        [proxy.listen]
        [[backends]]
        name = "api"
        transport = "http"
        url = "http://{addr}"
        bearer_token = "unused-bearer"
        [backends.headers]
        "X-API-Key" = "secret-key"
        "authorization" = "ApiKey explicit"
    "#
    ))
    .unwrap();
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    let mut service = proxy.mcp_proxy().clone();
    let response = service
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(100),
            inner: McpRequest::CallTool(CallToolParams {
                name: "api/ping".into(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();
    assert!(response.inner.is_ok(), "{response:?}");
    let headers = captured.lock().unwrap();
    assert!(
        headers.len() >= 3,
        "initialize, notification, discovery and call must reach the backend"
    );
    for request in headers.iter() {
        assert_eq!(request.get("x-api-key").unwrap(), "secret-key");
        let auth: Vec<_> = request.get_all("authorization").iter().collect();
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0], "ApiKey explicit");
    }
    server.abort();
}

#[cfg(feature = "websocket")]
// Tungstenite fixes the callback error type to an HTTP response.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn websocket_headers_reach_handshake() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_hdr_async(stream, |req: &Request, response: Response| {
            assert_eq!(req.headers()["x-api-key"], "secret-key");
            assert_eq!(req.headers()["authorization"], "ApiKey explicit");
            assert!(req.headers().contains_key("host"));
            Ok(response)
        })
        .await
        .unwrap()
    });
    let headers = [
        ("X-API-Key".into(), "secret-key".into()),
        ("AUTHORIZATION".into(), "ApiKey explicit".into()),
    ]
    .into();
    let _client = mcp_proxy::ws_transport::WebSocketClientTransport::connect_with_headers(
        &format!("ws://{addr}"),
        Some("unused-bearer"),
        &headers,
    )
    .await
    .unwrap();
    server.await.unwrap();
}
