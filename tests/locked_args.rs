//! Locked arguments through the configured middleware stack.
//!
//! Every path that can reach a backend (direct, aliased, renamed parameter,
//! composite, search-mode wrapper, canary, failover, mirror) must deliver the
//! locked value, and a rejected call must not reach any backend.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use reqwest::Client;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

type Journal = Arc<Mutex<Vec<(String, Value)>>>;

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

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct QueryInput {
    sql: Option<String>,
    tenant: Option<String>,
}

/// A backend whose tools record `(label, arguments)` and echo the arguments.
async fn backend(label: &'static str, journal: &Journal) -> Server {
    let record = |journal: Journal| {
        move |arguments: Value| {
            journal
                .lock()
                .unwrap()
                .push((label.into(), arguments.clone()));
            async move { Ok(CallToolResult::json(arguments)) }
        }
    };
    let typed = journal.clone();
    let router = McpRouter::new()
        .tool(
            ToolBuilder::new("echo")
                .handler(record(journal.clone()))
                .build(),
        )
        .tool(
            ToolBuilder::new("query")
                .handler(move |input: QueryInput| {
                    let typed = typed.clone();
                    async move {
                        let arguments = json!({"sql": input.sql, "tenant": input.tenant});
                        typed
                            .lock()
                            .unwrap()
                            .push((label.into(), arguments.clone()));
                        Ok(CallToolResult::json(arguments))
                    }
                })
                .build(),
        );
    serve(HttpTransport::new(router).into_router()).await
}

/// Answers `tools/call` with HTTP 500 so the failover layer takes over.
async fn fail_tool_calls(request: Request<Body>, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    if bytes.windows(12).any(|w| w == b"\"tools/call\"") {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

struct Session {
    client: Client,
    url: String,
    id: Option<String>,
}
impl Session {
    async fn new(url: &str) -> Self {
        let mut session = Self {
            client: Client::new(),
            url: format!("{url}/"),
            id: None,
        };
        let response = session
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"locked-args","version":"1"}}),
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
    async fn call(&self, name: &str, arguments: Value) -> Value {
        payload(
            self.request("tools/call", json!({"name":name,"arguments":arguments}))
                .await,
        )
        .await
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
fn assert_success(value: &Value) {
    assert!(value.get("error").is_none(), "{value}");
    assert_ne!(value["result"]["isError"], true, "{value}");
}

struct Fixture {
    proxy: Server,
    journal: Journal,
    _backends: Vec<Server>,
}

impl Fixture {
    async fn new(exposure: &str) -> Self {
        let journal: Journal = Arc::new(Mutex::new(vec![]));
        let api = backend("api", &journal).await;
        let db = backend("db", &journal).await;
        let canary = backend("api-canary", &journal).await;
        let shadow = backend("api-shadow", &journal).await;
        let pay = {
            let router = McpRouter::new().tool(
                ToolBuilder::new("charge")
                    .handler(|arguments: Value| async move { Ok(CallToolResult::json(arguments)) })
                    .build(),
            );
            serve(
                HttpTransport::new(router)
                    .into_router()
                    .layer(axum::middleware::from_fn(fail_tool_calls)),
            )
            .await
        };
        let pay_backup = {
            let journal = journal.clone();
            let router = McpRouter::new().tool(
                ToolBuilder::new("charge")
                    .handler(move |arguments: Value| {
                        journal
                            .lock()
                            .unwrap()
                            .push(("pay-backup".into(), arguments.clone()));
                        async move { Ok(CallToolResult::json(arguments)) }
                    })
                    .build(),
            );
            serve(HttpTransport::new(router).into_router()).await
        };

        let config = mcp_proxy::ProxyConfig::parse(&format!(
            r#"
[proxy]
name = "locked-args"
tool_exposure = "{exposure}"
[proxy.listen]

[[backends]]
name = "api"
transport = "http"
url = "{api}/"
weight = 1
[[backends.aliases]]
from = "echo"
to = "public"
[[backends.locked_args]]
args = {{ tenant = "acme" }}

# No canary: param_overrides are keyed by the namespace the client called, so
# a rename on a canaried backend would not apply to canary traffic.
[[backends]]
name = "db"
transport = "http"
url = "{db}/"
[[backends.param_overrides]]
tool = "echo"
rename = {{ tenant = "org" }}
[[backends.locked_args]]
args = {{ tenant = "acme" }}

[[backends]]
name = "api-canary"
transport = "http"
url = "{canary}/"
canary_of = "api"
weight = 99

[[backends]]
name = "api-shadow"
transport = "http"
url = "{shadow}/"
mirror_of = "api"
mirror_percent = 100

[[backends]]
name = "pay"
transport = "http"
url = "{pay}/"
[[backends.locked_args]]
tools = ["charge"]
args = {{ currency = "EUR" }}
on_conflict = "replace"

[[backends]]
name = "pay-backup"
transport = "http"
url = "{pay_backup}/"
failover_for = "pay"

[[composite_tools]]
name = "fan"
description = "Delegates to the aliased tool"
tools = ["api/public"]
"#,
            api = api.url,
            db = db.url,
            canary = canary.url,
            shadow = shadow.url,
            pay = pay.url,
            pay_backup = pay_backup.url,
        ))
        .unwrap();
        let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
        Self {
            proxy: serve(proxy.into_router().0).await,
            journal,
            _backends: vec![api, db, canary, shadow, pay, pay_backup],
        }
    }

    /// Calls recorded by primary or canary backends (the mirror is async).
    fn served(&self) -> Vec<(String, Value)> {
        self.journal
            .lock()
            .unwrap()
            .iter()
            .filter(|(label, _)| label != "api-shadow")
            .cloned()
            .collect()
    }

    fn take(&self) -> Vec<(String, Value)> {
        std::mem::take(&mut *self.journal.lock().unwrap())
    }

    async fn wait_for_mirror(&self, count: usize) -> Vec<Value> {
        for _ in 0..200 {
            let mirrored: Vec<_> = self
                .journal
                .lock()
                .unwrap()
                .iter()
                .filter(|(label, _)| label == "api-shadow")
                .map(|(_, args)| args.clone())
                .collect();
            if mirrored.len() >= count {
                return mirrored;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("mirror did not receive {count} calls");
    }
}

async fn assert_locked_paths(fixture: &Fixture, search: bool) {
    let session = Session::new(&fixture.proxy.url).await;

    // Direct, aliased, renamed-parameter, and composite calls. Canary routing
    // is deterministic: with weights 1/99 the first call goes to the primary
    // and the rest to the canary.
    let mut calls = vec![
        ("api/echo", json!({"q": 1})),
        ("api/public", json!({"q": 2})),
        ("db/echo", json!({"org": "acme", "q": 3})),
        ("fan", json!({"q": 4})),
        ("api/query", serde_json::Value::Null),
    ];
    calls.extend(calls.clone());
    for (name, arguments) in &calls {
        let response = session.call(name, arguments.clone()).await;
        assert_success(&response);
    }
    if search {
        let response = session
            .call(
                "proxy/call_tool",
                json!({"name": "api/public", "arguments": {"q": 5}}),
            )
            .await;
        assert_success(&response);
    }
    let served = fixture.served();
    assert_eq!(served.len(), calls.len() + usize::from(search));
    assert!(served.iter().any(|(label, _)| label == "api"), "{served:?}");
    assert!(served.iter().any(|(label, _)| label == "db"), "{served:?}");
    assert!(
        served.iter().any(|(label, _)| label == "api-canary"),
        "{served:?}"
    );
    for (label, arguments) in &served {
        assert_eq!(arguments["tenant"], "acme", "{label}: {arguments}");
    }
    // The mirror copies every call addressed to `api`, wherever it is served.
    let api_calls = served.iter().filter(|(label, _)| label != "db").count();
    for arguments in fixture.wait_for_mirror(api_calls).await {
        assert_eq!(arguments["tenant"], "acme", "mirror: {arguments}");
    }
    fixture.take();

    // A conflicting value, sent directly or through the renamed parameter, is
    // rejected before any backend (or mirror) sees it.
    for (name, arguments) in [
        ("api/public", json!({"tenant": "other"})),
        ("db/echo", json!({"tenant": "other"})),
        ("db/echo", json!({"org": "other"})),
    ] {
        let response = session.call(name, arguments).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(
            !response.to_string().contains("acme"),
            "locked value leaked: {response}"
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        fixture.take().is_empty(),
        "rejected calls reached a backend"
    );

    // Failover: the primary fails every call, the backup inherits the lock, and
    // replace mode overwrites the client value.
    let response = session
        .call("pay/charge", json!({"amount": 5, "currency": "USD"}))
        .await;
    assert_success(&response);
    assert_eq!(
        fixture.take(),
        vec![(
            "pay-backup".to_string(),
            json!({"amount": 5, "currency": "EUR"})
        )]
    );

    // The locked key is not advertised.
    if !search {
        let listed = payload(session.request("tools/list", json!({})).await).await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        let query = tools
            .iter()
            .find(|tool| tool["name"] == "api/query")
            .expect("api/query listed");
        assert!(query["inputSchema"]["properties"].get("sql").is_some());
        assert!(
            query["inputSchema"]["properties"].get("tenant").is_none(),
            "{query}"
        );
    }
}

#[tokio::test]
async fn locked_values_reach_every_backend_path() {
    let fixture = Fixture::new("direct").await;
    assert_locked_paths(&fixture, false).await;
}

#[cfg(feature = "discovery")]
#[tokio::test]
async fn locked_values_apply_to_search_mode_wrapper() {
    let fixture = Fixture::new("search").await;
    assert_locked_paths(&fixture, true).await;
}
