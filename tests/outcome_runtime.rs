//! Configured sessions preserve tool failures and apply response-aware resilience.
use mcp_proxy::{Proxy, ProxyConfig};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tower::ServiceExt;
use tower_mcp::client::{HttpClientTransport, McpClient};
use tower_mcp::protocol::McpRequest;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, RouterRequest, RouterResponse, ToolBuilder,
};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn serve(app: axum::Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (
        format!("http://{addr}/"),
        Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })),
    )
}
async fn backend(mode: &'static str) -> (String, Server, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let router = McpRouter::new().tool(
        ToolBuilder::new("work")
            .handler(|_: Value| async { Ok(CallToolResult::text("healthy")) })
            .build(),
    );
    let service = tower::service_fn(move |request: RouterRequest| {
        let router = router.clone();
        let count = count.clone();
        async move {
            if matches!(&request.inner, McpRequest::CallTool(_)) {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                let inner = match mode {
                    "cache" if attempt == 0 => Ok(tower_mcp::McpResponse::CallTool(
                        CallToolResult::error("application failure"),
                    )),
                    "app" => Ok(tower_mcp::McpResponse::CallTool(CallToolResult::error(
                        "application failure",
                    ))),
                    "rpc" => Err(tower_mcp_types::JsonRpcError::internal_error(
                        "server failure",
                    )),
                    "hedge" if attempt == 0 => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        Err(tower_mcp_types::JsonRpcError::internal_error(
                            "fast primary failure",
                        ))
                    }
                    "hedge" => {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(tower_mcp::McpResponse::CallTool(CallToolResult::text(
                            "hedge winner",
                        )))
                    }
                    _ => Ok(tower_mcp::McpResponse::CallTool(CallToolResult::text(
                        "recovered",
                    ))),
                };
                return Ok::<_, Infallible>(RouterResponse {
                    id: request.id,
                    inner,
                });
            }
            router.oneshot(request).await
        }
    });
    let (url, server) = serve(HttpTransport::from_service(service).into_router()).await;
    (url, server, calls)
}
async fn call(client: &McpClient, name: &str) -> CallToolResult {
    tokio::time::timeout(Duration::from_secs(5), client.call_tool(name, json!({})))
        .await
        .unwrap()
        .unwrap()
}
async fn breaker(base: &str, name: &str) -> Value {
    let response = reqwest::Client::new()
        .get(format!("{base}admin/circuit-breakers"))
        .bearer_auth("manager")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let statuses: Vec<Value> = response.json().await.unwrap();
    statuses
        .into_iter()
        .find(|status| status["backend"] == name)
        .unwrap()
}

#[tokio::test]
async fn configured_outcomes_preserve_tool_errors_cache_recovery_breaker_state_and_hedge_winners() {
    let (cache, _cache, cache_calls) = backend("cache").await;
    let (app, _app, app_calls) = backend("app").await;
    let (rpc, _rpc, rpc_calls) = backend("rpc").await;
    let (hedge, _hedge, hedge_calls) = backend("hedge").await;
    let config = ProxyConfig::parse(&format!(
        r#"
[proxy]
name="outcome-runtime"
[proxy.listen]
[auth]
type="bearer"
tokens=["ordinary"]
[security]
admin_token="manager"
[observability.metrics]
enabled={}
[[backends]]
name="cache"
transport="http"
url="{cache}"
[backends.cache]
tool_ttl_seconds=60
[backends.retry]
max_retries=2
initial_backoff_ms=0
[[backends]]
name="app"
transport="http"
url="{app}"
[backends.retry]
max_retries=2
initial_backoff_ms=0
[backends.circuit_breaker]
minimum_calls=2
failure_rate_threshold=0.5
[backends.outlier_detection]
consecutive_errors=2
max_ejection_percent=100
[[backends]]
name="rpc"
transport="http"
url="{rpc}"
[backends.circuit_breaker]
minimum_calls=2
failure_rate_threshold=0.5
[[backends]]
name="hedge"
transport="http"
url="{hedge}"
[backends.hedging]
delay_ms=1
max_hedges=1
"#,
        cfg!(feature = "metrics")
    ))
    .unwrap();
    let proxy = Proxy::from_config(config).await.unwrap();
    let (router, _) = proxy.into_router();
    let (base, _proxy) = serve(router).await;
    let ordinary = McpClient::connect(HttpClientTransport::new(&base).bearer_token("ordinary"))
        .await
        .unwrap();
    ordinary.initialize("outcome-test", "1").await.unwrap();
    ordinary.list_tools().await.unwrap();

    let failed = call(&ordinary, "cache/work").await;
    assert!(failed.is_error);
    assert_eq!(failed.all_text(), "application failure");
    for _ in 0..2 {
        let response = call(&ordinary, "cache/work").await;
        assert!(!response.is_error);
        assert_eq!(response.all_text(), "recovered");
    }
    assert_eq!(
        cache_calls.load(Ordering::SeqCst),
        2,
        "tool failure was retried or cached"
    );

    for _ in 0..3 {
        assert!(call(&ordinary, "app/work").await.is_error);
    }
    assert_eq!(
        app_calls.load(Ordering::SeqCst),
        3,
        "tool failures caused retry/ejection/breaker rejection"
    );
    assert_eq!(breaker(&base, "app").await["state"], "Closed");

    for _ in 0..3 {
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ordinary.call_tool("rpc/work", json!({})),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("-32603"));
    }
    assert_eq!(
        rpc_calls.load(Ordering::SeqCst),
        2,
        "breaker failed to open after encoded failures"
    );
    assert_eq!(breaker(&base, "rpc").await["state"], "Open");

    let winner = call(&ordinary, "hedge/work").await;
    assert!(!winner.is_error);
    assert_eq!(winner.all_text(), "hedge winner");
    assert_eq!(hedge_calls.load(Ordering::SeqCst), 2);

    let manager = McpClient::connect(HttpClientTransport::new(&base).bearer_token("manager"))
        .await
        .unwrap();
    manager.initialize("outcome-manager", "1").await.unwrap();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = format!("http://{}/", closed.local_addr().unwrap());
    drop(closed);
    let failed = tokio::time::timeout(
        Duration::from_secs(5),
        manager.call_tool(
            "proxy/add_backend",
            json!({"name":"unavailable","url":unavailable}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        failed.is_error,
        "management failure was returned as successful text"
    );
    assert!(failed.all_text().contains("Failed to add backend"));

    #[cfg(feature = "metrics")]
    {
        let text = reqwest::Client::new()
            .get(format!("{base}admin/metrics"))
            .bearer_auth("manager")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let line = text
            .lines()
            .find(|line| {
                line.starts_with("mcp_proxy_requests_total{")
                    && line.contains("method=\"tools/call\"")
                    && line.contains("outcome=\"tool_error\"")
                    && line.contains("status=\"error\"")
            })
            .expect("missing tool-error counter");
        assert_eq!(line.split_whitespace().last(), Some("5"));
    }
    ordinary.shutdown().await.unwrap();
    manager.shutdown().await.unwrap();
}
