//! Discovery through the configured proxy: the catalog shows the public,
//! caller-visible tool list and follows backend changes on every path.
#![cfg(feature = "discovery")]

use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};
use tower_mcp::client::ChannelTransport;
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

fn tool(name: &str, description: &str, schema: Value) -> tower_mcp::Tool {
    ToolBuilder::new(name)
        .description(description)
        .input_schema(schema)
        .handler(|_: Value| async { Ok(CallToolResult::text("ok")) })
        .build()
}

async fn api_backend() -> Server {
    let router = McpRouter::new()
        .tool(tool(
            "echo",
            "Echo a message back",
            json!({"type":"object"}),
        ))
        .tool(tool(
            "delete",
            "Delete every record permanently",
            json!({"type":"object"}),
        ))
        .tool(tool(
            "lookup",
            "Look up customer records",
            json!({"type":"object","properties":{
                "query":{"type":"string"},
                "tenant":{"type":"string"},
                "debug":{"type":"boolean"}
            },"required":["query","tenant"]}),
        ));
    serve(HttpTransport::new(router).into_router()).await
}

async fn single_tool_backend(name: &str, description: &str) -> Server {
    let router = McpRouter::new().tool(tool(name, description, json!({"type":"object"})));
    serve(HttpTransport::new(router).into_router()).await
}

struct Session {
    client: Client,
    url: String,
    token: Option<String>,
    id: Option<String>,
}
impl Session {
    async fn new(url: &str, token: Option<&str>) -> Self {
        let mut session = Self {
            client: Client::new(),
            url: format!("{url}/"),
            token: token.map(str::to_owned),
            id: None,
        };
        let response = session
            .request(
                "initialize",
                json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"discovery","version":"1"}}),
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
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
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
    /// The JSON a discovery tool returned, or `None` for a tool error.
    async fn discover(&self, sep: &str, tool: &str, arguments: Value) -> Option<Value> {
        let response = self.call(&format!("proxy{sep}{tool}"), arguments).await;
        assert!(response.get("error").is_none(), "{response}");
        if response["result"]["isError"] == true {
            return None;
        }
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        Some(serde_json::from_str(text).unwrap())
    }
    async fn search_ids(&self, sep: &str, query: &str) -> Vec<String> {
        let results = self
            .discover(sep, "search_tools", json!({"query": query, "top_k": 50}))
            .await
            .unwrap();
        results
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect()
    }
    async fn listed_names(&self) -> Vec<String> {
        let response = payload(self.request("tools/list", json!({})).await).await;
        response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
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
    raw: tower_mcp::proxy::McpProxy,
    catalog: mcp_proxy::discovery::DiscoveryCatalog,
    _backends: Vec<Server>,
}

/// `extra` is appended to the generated TOML (auth, security, more backends).
async fn fixture(sep: &str, exposure: &str, extra: &str) -> Fixture {
    let api = api_backend().await;
    let db = single_tool_backend("select", "Run a database query").await;
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
[proxy]
name = "discovery"
separator = "{sep}"
tool_exposure = "{exposure}"
tool_discovery = true
[proxy.listen]
{extra}

[[backends]]
name = "api"
transport = "http"
url = "{api}/"
hide_tools = ["delete"]
[[backends.aliases]]
from = "echo"
to = "public"
[[backends.param_overrides]]
tool = "lookup"
hide = ["debug"]
rename = {{ query = "q" }}
[[backends.locked_args]]
tools = ["lookup"]
args = {{ tenant = "acme" }}

[[backends]]
name = "db"
transport = "http"
url = "{db}/"

[[composite_tools]]
name = "fanout"
description = "Broadcast through the public echo"
tools = ["api{sep}public"]
"#,
        api = api.url,
        db = db.url,
    ))
    .unwrap();
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    let raw = proxy.mcp_proxy().clone();
    let catalog = proxy.discovery_catalog().unwrap().clone();
    Fixture {
        proxy: serve(proxy.into_router().0).await,
        raw,
        catalog,
        _backends: vec![api, db],
    }
}

/// Wait until `check` holds for the search results of `query`.
async fn eventually(session: &Session, sep: &str, query: &str, check: impl Fn(&[String]) -> bool) {
    for _ in 0..200 {
        if check(&session.search_ids(sep, query).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "search for {query:?} never matched; last: {:?}",
        session.search_ids(sep, query).await
    );
}

const ADMIN: &str = r#"
[security]
admin_token = "management-token"
"#;

async fn assert_public_catalog(sep: &str, exposure: &str) {
    let fx = fixture(sep, exposure, ADMIN).await;
    let session = Session::new(&fx.proxy.url, None).await;
    // similar_tools and tool_categories require management authority (#310).
    let admin = Session::new(&fx.proxy.url, Some("management-token")).await;

    // Aliases: the public name is searchable, the original is not.
    let ids = session.search_ids(sep, "echo message").await;
    assert!(ids.contains(&"api:public".to_string()), "{ids:?}");
    assert!(!ids.contains(&"api:echo".to_string()), "{ids:?}");

    // Hidden tools are absent from search, similar, categories and get_tool.
    let ids = session.search_ids(sep, "delete records permanently").await;
    assert!(!ids.contains(&"api:delete".to_string()), "{ids:?}");
    assert!(
        session
            .discover(sep, "get_tool", json!({"tool_id":"api:delete"}))
            .await
            .is_none()
    );
    let similar = admin
        .discover(
            sep,
            "similar_tools",
            json!({"tool_id":"api:lookup","top_k":10}),
        )
        .await
        .unwrap();
    assert!(!similar.to_string().contains("api:delete"), "{similar}");

    // get_tool shows the public schema: renamed, hidden and locked keys applied.
    let lookup = session
        .discover(sep, "get_tool", json!({"tool_id":"api:lookup"}))
        .await
        .unwrap();
    assert_eq!(lookup["name"], format!("api{sep}lookup"));
    let properties = lookup["input_schema"]["properties"].as_object().unwrap();
    assert!(properties.contains_key("q"), "{lookup}");
    for key in ["query", "debug", "tenant"] {
        assert!(!properties.contains_key(key), "{key} leaked: {lookup}");
    }

    // Composite tools are searchable.
    let ids = session.search_ids(sep, "broadcast").await;
    assert!(ids.contains(&"default:fanout".to_string()), "{ids:?}");

    let categories = admin
        .discover(sep, "tool_categories", json!({}))
        .await
        .unwrap();
    let count = |name: &str| {
        categories["categories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["tool_count"].as_u64().unwrap())
    };
    assert_eq!(count("api"), Some(2), "{categories}");
    assert_eq!(count("db"), Some(1), "{categories}");
    assert_eq!(count("proxy"), None, "{categories}");

    // The internal unfiltered listing does not leak into tools/list.
    let listed = session.listed_names().await;
    if exposure == "search" {
        // Backend tools stay hidden; composite tools are listed as before.
        assert!(
            listed.iter().all(
                |n| !n.starts_with(&format!("api{sep}")) && !n.starts_with(&format!("db{sep}"))
            ),
            "{listed:?}"
        );
    } else {
        assert!(listed.contains(&format!("api{sep}public")), "{listed:?}");
    }
}

#[tokio::test]
async fn catalog_uses_public_names_and_schemas() {
    assert_public_catalog("/", "direct").await;
}

#[tokio::test]
async fn catalog_uses_public_names_in_search_mode() {
    assert_public_catalog("/", "search").await;
}

#[tokio::test]
async fn catalog_supports_alternate_separators() {
    assert_public_catalog("::", "direct").await;
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn results_are_limited_to_what_the_caller_can_see() {
    let auth = r#"
[auth]
type = "bearer"
tokens = ["full"]
[[auth.scoped_tokens]]
token = "narrow"
allow_tools = ["api/public", "api/lookup", "proxy/search_tools", "proxy/get_tool"]
"#;
    let fx = fixture("/", "direct", auth).await;
    let full = Session::new(&fx.proxy.url, Some("full")).await;
    let narrow = Session::new(&fx.proxy.url, Some("narrow")).await;

    assert!(
        full.search_ids("/", "database query")
            .await
            .contains(&"db:select".to_string())
    );
    let ids = narrow.search_ids("/", "database query").await;
    assert!(!ids.contains(&"db:select".to_string()), "{ids:?}");
    let ids = narrow.search_ids("/", "customer records echo").await;
    assert!(ids.contains(&"api:lookup".to_string()), "{ids:?}");
    assert!(
        ids.iter()
            .all(|id| id == "api:lookup" || id == "api:public"),
        "{ids:?}"
    );

    // A tool the caller cannot see answers like an unknown id.
    assert!(
        narrow
            .discover("/", "get_tool", json!({"tool_id":"db:select"}))
            .await
            .is_none()
    );

    // A discovery tool outside the caller's scope is refused by policy.
    let refused = narrow
        .call("proxy/get_tool", json!({"tool_id":"api:lookup"}))
        .await;
    assert_ne!(refused["result"]["isError"], true, "{refused}");
    let refused = full
        .call("proxy/similar_tools", json!({"tool_id":"api:lookup"}))
        .await;
    assert!(
        refused.get("error").is_some() || refused["result"]["isError"] == true,
        "{refused}"
    );
}

#[tokio::test]
async fn admin_changes_update_the_catalog() {
    let fx = fixture("/", "direct", ADMIN).await;
    let session = Session::new(&fx.proxy.url, None).await;
    let extra = single_tool_backend("zebra", "Count zebra stripes").await;
    let admin = Client::new();

    let added = admin
        .post(format!("{}/admin/backends/add", fx.proxy.url))
        .bearer_auth("management-token")
        .json(&json!({"name":"extra","url":format!("{}/", extra.url)}))
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), reqwest::StatusCode::CREATED);
    eventually(&session, "/", "zebra stripes", |ids| {
        ids.contains(&"extra:zebra".to_string())
    })
    .await;

    let removed = admin
        .delete(format!("{}/admin/backends/db", fx.proxy.url))
        .bearer_auth("management-token")
        .send()
        .await
        .unwrap();
    assert!(removed.status().is_success(), "{}", removed.status());
    eventually(&session, "/", "database query", |ids| {
        !ids.contains(&"db:select".to_string())
    })
    .await;
}

#[tokio::test]
async fn backend_list_changes_reach_the_catalog_after_earlier_removals() {
    let fx = fixture("/", "direct", "").await;
    let session = Session::new(&fx.proxy.url, None).await;
    let (router, registry) = McpRouter::new()
        .tool(tool(
            "first",
            "Initial dynamic tool",
            json!({"type":"object"}),
        ))
        .with_dynamic_tools();
    fx.raw
        .add_backend("dyn", ChannelTransport::new(router))
        .await
        .unwrap();
    eventually(&session, "/", "initial dynamic", |ids| {
        ids.contains(&"dyn:first".to_string())
    })
    .await;

    // Remove a backend registered before `dyn`, then change dyn's tools.
    assert!(fx.raw.remove_backend("api").await);
    eventually(&session, "/", "customer records", |ids| {
        !ids.contains(&"api:lookup".to_string())
    })
    .await;
    let before = fx.catalog.revision();
    registry.register(tool(
        "second",
        "Freshly announced tool",
        json!({"type":"object"}),
    ));
    eventually(&session, "/", "freshly announced", |ids| {
        ids.contains(&"dyn:second".to_string())
    })
    .await;
    assert!(fx.catalog.revision() > before);
}
