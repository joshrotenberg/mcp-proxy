//! Configured retry budgets are exercised through a complete authenticated MCP session.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;

use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use tower_mcp::{HttpTransport, McpRouter, RouterRequest, RouterResponse, ToolBuilder};

#[tokio::test]
async fn configured_retry_volume_budget_preserves_errors_and_limits_actual_attempts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let router = McpRouter::new().tool(
        ToolBuilder::new("work")
            .handler(|_: serde_json::Value| async { Ok(tower_mcp::CallToolResult::text("unused")) })
            .build(),
    );
    let service = tower::service_fn(move |request: RouterRequest| {
        let router = router.clone();
        let calls = captured.clone();
        async move {
            if matches!(&request.inner, tower_mcp::protocol::McpRequest::CallTool(_)) {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::convert::Infallible>(RouterResponse {
                    id: request.id,
                    inner: Err(tower_mcp_types::JsonRpcError::internal_error(
                        "backend failure",
                    )),
                })
            } else {
                router.oneshot(request).await
            }
        }
    });
    let mut backend = http::Server::start(HttpTransport::from_service(service).into_router()).await;
    let config = format!(
        r#"
[proxy]
name='retry-runtime'
[proxy.listen]
[auth]
type='bearer'
tokens=['data']
[[backends]]
name='api'
transport='http'
url='{}'
[backends.retry]
max_retries=3
initial_backoff_ms=1
max_backoff_ms=1
budget_percent=20.0
min_retries_per_sec=0
[backends.timeout]
seconds=2
"#,
        backend.url
    );
    let mut app = app::App::embedded(&config).await;
    let session = http::Session::start(&app.url, "data").await;
    let listed = session.rpc("tools/list", json!({})).await;
    assert_eq!(http::success(&listed)["tools"][0]["name"], "api/work");
    for _ in 0..100 {
        let response = session.call("api/work", json!({})).await;
        assert_eq!(response["error"]["code"], -32603, "{response}");
        assert!(response.get("result").is_none());
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        120,
        "100 tool calls plus the successful list call fund exactly 20 retries"
    );
    session.terminate().await;
    app.shutdown().await;
    backend.stop().await;
}
