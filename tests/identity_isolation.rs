//! Caller identity in the cache and coalescer (F04, #301): `cache_scope`
//! partitions per-caller backends, and task-augmented calls are never shared.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::Client;
use serde_json::{Value, json};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}

/// A backend with one `whoami` tool that counts the calls it receives.
async fn counting_backend(calls: Arc<AtomicUsize>) -> Server {
    let router = McpRouter::new().tool(
        ToolBuilder::new("whoami")
            .handler(move |_: Value| {
                let calls = calls.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(CallToolResult::text(format!("call {n}")))
                }
            })
            .build(),
    );
    serve(HttpTransport::new(router).into_router()).await
}

struct Session {
    client: Client,
    url: String,
    token: String,
    id: Option<String>,
}
impl Session {
    async fn new(url: &str, token: &str) -> Self {
        let mut session = Self {
            client: Client::new(),
            url: format!("{url}/"),
            token: token.to_owned(),
            id: None,
        };
        let response = session
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"identity","version":"1"}}),
            )
            .await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        session.id = Some(
            response.headers()["mcp-session-id"]
                .to_str()
                .unwrap()
                .into(),
        );
        payload(response).await;
        session
            .request("notifications/initialized", json!({}))
            .await;
        session
    }
    async fn request(&self, method: &str, params: Value) -> reqwest::Response {
        let mut request = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25");
        if let Some(id) = &self.id {
            request = request.header("mcp-session-id", id);
        }
        let mut rpc = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !method.starts_with("notifications/") {
            rpc["id"] = json!(7);
        }
        request.json(&rpc).send().await.unwrap()
    }
    async fn call(&self, name: &str) -> Value {
        let response = payload(
            self.request("tools/call", json!({"name": name, "arguments": {}}))
                .await,
        )
        .await;
        assert!(response.get("error").is_none(), "{response}");
        response
    }
}
async fn payload(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or(&text);
    serde_json::from_str(json).unwrap()
}

struct Fixture {
    proxy: Server,
    shared_calls: Arc<AtomicUsize>,
    private_calls: Arc<AtomicUsize>,
    _backends: [Server; 2],
}

async fn fixture() -> Fixture {
    let shared_calls = Arc::new(AtomicUsize::new(0));
    let private_calls = Arc::new(AtomicUsize::new(0));
    let shared = counting_backend(shared_calls.clone()).await;
    let private = counting_backend(private_calls.clone()).await;
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
[proxy]
name = "identity"
[proxy.listen]
[performance]
coalesce_requests = true

[auth]
type = "bearer"
tokens = ["alice", "bob"]

[[backends]]
name = "shared"
transport = "http"
url = "{shared}/"
[backends.cache]
tool_ttl_seconds = 60

[[backends]]
name = "private"
transport = "http"
url = "{private}/"
cache_scope = "principal"
[backends.cache]
tool_ttl_seconds = 60
"#,
        shared = shared.url,
        private = private.url,
    ))
    .unwrap();
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    Fixture {
        proxy: serve(proxy.into_router().0).await,
        shared_calls,
        private_calls,
        _backends: [shared, private],
    }
}

#[tokio::test]
async fn principal_scope_keeps_cached_responses_per_caller() {
    let fx = fixture().await;
    let alice = Session::new(&fx.proxy.url, "alice").await;
    let bob = Session::new(&fx.proxy.url, "bob").await;

    // A shared backend serves bob from alice's cached entry.
    alice.call("shared/whoami").await;
    bob.call("shared/whoami").await;
    assert_eq!(fx.shared_calls.load(Ordering::SeqCst), 1);

    // A principal-scoped backend never does; each caller gets its own entry.
    alice.call("private/whoami").await;
    bob.call("private/whoami").await;
    assert_eq!(fx.private_calls.load(Ordering::SeqCst), 2);
    alice.call("private/whoami").await;
    assert_eq!(
        fx.private_calls.load(Ordering::SeqCst),
        2,
        "alice hits her own entry"
    );

    // A second session for the same credential is the same principal.
    let alice_again = Session::new(&fx.proxy.url, "alice").await;
    alice_again.call("private/whoami").await;
    assert_eq!(fx.private_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn principal_scope_requires_authentication() {
    let err = mcp_proxy::ProxyConfig::parse(
        r#"
[proxy]
name = "identity"
[proxy.listen]
[[backends]]
name = "private"
transport = "stdio"
command = "fixture"
cache_scope = "principal"
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cache_scope"), "{err}");
}
