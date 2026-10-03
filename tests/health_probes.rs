//! Process probes remain available while MCP and admin routes require auth.
use tower::ServiceExt;

#[tokio::test]
async fn probes_do_not_bypass_admin_or_mcp_authentication() {
    let backend = tower_mcp::McpRouter::new();
    let transport = tower_mcp::transport::http::HttpTransport::new(backend).into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, transport).await.unwrap() });
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
        [proxy]
        name = "probe-test"
        [proxy.listen]
        [auth]
        type = "bearer"
        tokens = ["mcp-token"]
        [security]
        admin_token = "admin-token"
        [[backends]]
        name = "api"
        transport = "http"
        url = "http://{addr}"
        "#
    ))
    .unwrap();
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    let (router, _) = proxy.into_router();
    for path in ["/livez", "/readyz"] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{path}");
        let body = axum::body::to_bytes(response.into_body(), 32)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"ok");
    }
    for path in ["/admin/health", "/admin/config", "/mcp"] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/admin/health")
                .header("authorization", "Bearer admin-token")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    server.abort();
}
