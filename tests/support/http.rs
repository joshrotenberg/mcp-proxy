//! Bounded wire-level MCP session helpers. No automatic handshake/request retry.
#![allow(dead_code)]

use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const VERSION: &str = "2025-11-25";

pub struct Server {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn start(router: axum::Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::launch(listener, router)
    }
    pub fn launch(listener: tokio::net::TcpListener, router: axum::Router) -> Self {
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, task }
    }
    pub async fn stop(&mut self) {
        self.task.abort();
        let result = (&mut self.task).await;
        assert!(result.is_ok() || result.unwrap_err().is_cancelled());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}
pub fn initialization() -> Value {
    json!({"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"configured-contract","version":"1"}})
}

pub struct Session {
    pub client: Client,
    pub url: String,
    pub token: String,
    pub id: String,
    next: AtomicU64,
}
impl Session {
    pub async fn start(base: &str, token: &str) -> Self {
        let mut session = Self {
            client: client(),
            url: format!("{}/", base.trim_end_matches('/')),
            token: token.into(),
            id: String::new(),
            next: AtomicU64::new(1),
        };
        let response = session
            .request(
                json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":initialization()}),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "initialize");
        session.id = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .into();
        assert!(!session.id.is_empty());
        let reply = payload(response).await;
        assert_eq!(reply["id"], 0);
        let result = success(&reply);
        assert_eq!(result["protocolVersion"], VERSION);
        assert!(result["serverInfo"]["name"].is_string(), "{reply}");
        session.notify("notifications/initialized", json!({})).await;
        session
    }
    pub async fn request(&self, body: Value) -> Response {
        let mut request = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION);
        if !self.id.is_empty() {
            request = request.header("mcp-session-id", &self.id);
        }
        request
            .json(&body)
            .send()
            .await
            .expect("MCP POST failed (no retry)")
    }
    pub async fn notify(&self, method: &str, params: Value) {
        let response = self
            .request(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{method}");
        assert!(
            response.bytes().await.unwrap().is_empty(),
            "notification must not produce a response"
        );
    }
    pub async fn rpc(&self, method: &str, params: Value) -> Value {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let response = self
            .request(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        let reply = payload(response).await;
        assert_eq!(reply["id"], id, "{method}: {reply}");
        reply
    }
    pub async fn call(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
    pub async fn terminate(&self) {
        let response = self
            .client
            .delete(&self.url)
            .bearer_auth(&self.token)
            .header("mcp-protocol-version", VERSION)
            .header("mcp-session-id", &self.id)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // A deleted session cannot silently execute another tool call.
        let response = self
            .request(json!({"jsonrpc":"2.0","id":999,"method":"tools/list","params":{}}))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let reply = payload(response).await;
        assert_eq!(
            reply["error"]["code"],
            tower_mcp_types::JsonRpcError::session_not_found_with_id(&self.id).code
        );
    }
}
pub async fn payload(response: Response) -> Value {
    let text = response.text().await.unwrap();
    // These ordinary requests carry one terminal response, as JSON or SSE.
    let frames: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect();
    let reply: Value = if frames.is_empty() {
        serde_json::from_str(&text)
    } else {
        assert_eq!(frames.len(), 1, "unexpected SSE frames: {text}");
        serde_json::from_str(frames[0])
    }
    .expect("MCP response must contain JSON");
    assert_eq!(reply["jsonrpc"], "2.0");
    reply
}
pub fn success(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    let result = reply.get("result").expect("RPC result required");
    assert_ne!(result["isError"], true, "{reply}");
    result
}

/// Method/status-only wire evidence, including pending requests on fixture failure.
#[derive(Clone)]
pub struct WireTrace {
    pub events: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    started: std::time::Instant,
}
impl WireTrace {
    pub fn new() -> Self {
        Self {
            events: Default::default(),
            started: std::time::Instant::now(),
        }
    }
    pub fn snapshot(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn layer(&self, router: axum::Router) -> axum::Router {
        let trace = self.clone();
        router.layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
            let trace = trace.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
                let rpc = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                let method = rpc["method"].as_str().unwrap_or("GET/DELETE").to_string();
                let session = parts.headers.get("mcp-session-id").map(|s| s.to_str().unwrap().to_string());
                trace.events.lock().unwrap().push(json!({"phase":"received","method":method,"session":session,"elapsed_ms":trace.started.elapsed().as_millis()}));
                let response = next.run(axum::http::Request::from_parts(parts, axum::body::Body::from(bytes))).await;
                trace.events.lock().unwrap().push(json!({"phase":"replied","method":method,"status":response.status().as_u16(),"session":response.headers().get("mcp-session-id").map(|s| s.to_str().unwrap()),"elapsed_ms":trace.started.elapsed().as_millis()}));
                response
            }
        }))
    }
}
