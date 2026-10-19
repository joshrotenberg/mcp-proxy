//! Configured HTTP/MCP sessions enforce coalescing bounds and recover after expiry.
use std::time::Duration;

use mcp_proxy::{Proxy, ProxyConfig};
use tokio::sync::{mpsc, oneshot};
use tower_mcp::client::{HttpClientTransport, McpClient};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

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

#[tokio::test]
async fn configured_coalescing_enforces_capacity_deadline_and_reuses_expired_key() {
    let (started, mut calls) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    let backend = McpRouter::new().tool(
        ToolBuilder::new("work")
            .handler(move |_: serde_json::Value| {
                let started = started.clone();
                async move {
                    let (finish, finished) = oneshot::channel();
                    started.send(finish).unwrap();
                    let _ = finished.await;
                    Ok(CallToolResult::text("completed"))
                }
            })
            .build(),
    );
    let (backend_url, _backend) = serve(HttpTransport::new(backend).into_router()).await;
    let config = ProxyConfig::parse(&format!(
        r#"
[proxy]
name = "coalescing-runtime"
[proxy.listen]
[auth]
type = "bearer"
tokens = ["data-token"]
[performance]
coalesce_requests = true
[performance.coalesce]
max_entries = 1
max_waiters = 1
timeout_seconds = 1
[[backends]]
name = "api"
transport = "http"
url = "{backend_url}"
"#
    ))
    .unwrap();
    let proxy = Proxy::from_config(config).await.unwrap();
    let (app, _) = proxy.into_router();
    let (url, _proxy) = serve(app).await;
    let client = std::sync::Arc::new(
        McpClient::connect(HttpClientTransport::new(url).bearer_token("data-token"))
            .await
            .unwrap(),
    );
    client.initialize("coalescing-test", "1").await.unwrap();
    assert_eq!(client.list_tools().await.unwrap().tools[0].name, "api/work");

    let leader_client = client.clone();
    let leader = tokio::spawn(async move {
        leader_client
            .call_tool_text("api/work", serde_json::json!({"key":1}))
            .await
    });
    let pending = tokio::time::timeout(Duration::from_secs(3), calls.recv())
        .await
        .unwrap()
        .unwrap();
    let rejected = tokio::time::timeout(
        Duration::from_secs(2),
        client.call_tool_text("api/work", serde_json::json!({"key":2})),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("coalescing capacity exceeded"),
        "{rejected}"
    );
    assert!(
        calls.try_recv().is_err(),
        "overload dispatched backend work"
    );
    let expired = tokio::time::timeout(Duration::from_secs(3), leader)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        expired.to_string().contains("Coalesced request timed out"),
        "{expired}"
    );
    // Completion/cancellation of remote work is independent of local expiry.
    let _ = pending.send(());

    let recovery_client = client.clone();
    let recovery = tokio::spawn(async move {
        recovery_client
            .call_tool_text("api/work", serde_json::json!({"key":1}))
            .await
    });
    let pending = tokio::time::timeout(Duration::from_secs(3), calls.recv())
        .await
        .unwrap()
        .unwrap();
    pending.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), recovery)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "completed"
    );
    assert!(
        calls.try_recv().is_err(),
        "expired operation was automatically replayed"
    );
}
