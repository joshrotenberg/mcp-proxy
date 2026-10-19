//! The configured application shares explicit management authority across HTTP
//! and MCP. Tests use real local servers and complete, credential-bound sessions.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use tower_mcp::{
    CallToolResult, GetPromptResult, HttpTransport, McpRouter, PromptBuilder, ResourceBuilder,
    ToolBuilder,
};

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

fn capabilities(counter: Arc<AtomicUsize>) -> McpRouter {
    McpRouter::new()
        .tool(
            ToolBuilder::new("ping")
                .handler(move |_: Value| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async { Ok(CallToolResult::text("pong")) }
                })
                .build(),
        )
        .resource(
            ResourceBuilder::new("state://value")
                .name("State")
                .text("fixture state"),
        )
        .prompt(
            PromptBuilder::new("hello")
                .handler(|_: HashMap<String, String>| async {
                    Ok(GetPromptResult {
                        description: None,
                        messages: vec![],
                        meta: None,
                    })
                })
                .build(),
        )
}

struct Fixture {
    server: Server,
    backend: Server,
    counter: Arc<AtomicUsize>,
    temp: tempfile::TempDir,
    config: String,
    separator: String,
}
impl Fixture {
    async fn new(admin: bool, auth: bool, search: bool, separator: &str, resources: bool) -> Self {
        let counter = Arc::new(AtomicUsize::new(0));
        let backend = serve(HttpTransport::new(capabilities(counter.clone())).into_router()).await;
        let bearer = if auth {
            let scoped = if cfg!(feature = "oauth") {
                format!(
                    r#"[[auth.scoped_tokens]]
token = "scoped"
allow_tools = ["api{separator}ping", "proxy{separator}call_tool"]
"#
                )
            } else {
                String::new()
            };
            format!("[auth]\ntype = 'bearer'\ntokens = ['ordinary', 'other']\n{scoped}")
        } else {
            String::new()
        };
        let admin_config = if admin {
            "[security]\nadmin_token = 'manager'\n"
        } else {
            ""
        };
        let search = if search {
            "tool_exposure = 'search'"
        } else {
            ""
        };
        let config = format!(
            r#"
[proxy]
name = "management-test"
separator = "{separator}"
{search}
[proxy.listen]
{bearer}
{admin_config}
[performance]
coalesce_requests = true
[[backends]]
name = "api"
transport = "http"
url = "{backend_url}/"
[backends.cache]
tool_ttl_seconds = 60
resource_ttl_seconds = 60
[[composite_tools]]
name = "indirect_config"
description = "exercise indirect management access"
tools = ["proxy{separator}config"]
strategy = "parallel"
"#,
            backend_url = backend.url
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("proxy.toml");
        std::fs::write(&path, &config).unwrap();
        let config_value = mcp_proxy::ProxyConfig::load(&path).unwrap();
        let proxy = mcp_proxy::Proxy::from_config(config_value).await.unwrap();
        if resources {
            // Current management tools expose no resources. Install a trusted
            // fixture behind the same namespace to exercise all MCP surfaces.
            proxy.mcp_proxy().remove_backend("proxy").await;
            proxy
                .mcp_proxy()
                .add_backend(
                    "proxy",
                    tower_mcp::client::ChannelTransport::new(capabilities(Arc::new(
                        AtomicUsize::new(0),
                    ))),
                )
                .await
                .unwrap();
        }
        let (router, _) = proxy.into_router();
        Self {
            server: serve(router).await,
            backend,
            counter,
            temp,
            config,
            separator: separator.into(),
        }
    }
    fn tool(&self, name: &str) -> String {
        format!("proxy{}{name}", self.separator)
    }
    async fn session(&self, token: Option<&str>) -> Session {
        Session::new(&self.server.url, token).await
    }
}

struct Session {
    client: Client,
    url: String,
    token: Option<String>,
    id: String,
}
impl Session {
    async fn new(base: &str, token: Option<&str>) -> Self {
        let mut session = Self {
            client: Client::new(),
            url: format!("{base}/"),
            token: token.map(str::to_owned),
            id: String::new(),
        };
        let response = session.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"management-regression","version":"1"}})).await;
        assert_eq!(response.status(), StatusCode::OK);
        session.id = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let initialized = payload(response).await;
        assert!(initialized.get("error").is_none());
        let mut request = session
            .client
            .post(&session.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session.id)
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        if let Some(token) = &session.token {
            request = request.bearer_auth(token);
        }
        assert!(request.send().await.unwrap().status().is_success());
        session
    }
    async fn request(&self, method: &str, params: Value) -> reqwest::Response {
        let mut request = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if !self.id.is_empty() {
            request = request.header("mcp-session-id", &self.id);
        }
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self.request(method, params).await;
        assert_eq!(response.status(), StatusCode::OK);
        payload(response).await
    }
    async fn call(&self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }
}
async fn payload(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    let text = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or(&text);
    serde_json::from_str(text).expect("expected a JSON-RPC response")
}
fn denied(value: &Value) {
    assert!(
        value.get("error").is_some() || value["result"]["isError"] == true,
        "request must be denied"
    );
}
fn succeeded(value: &Value) {
    assert!(value.get("error").is_none(), "request must succeed");
    assert_ne!(value["result"]["isError"], true);
}

#[tokio::test]
async fn explicit_authority_gates_http_and_mcp_and_preserves_ordinary_capabilities() {
    let fixture = Fixture::new(true, true, false, "/", false).await;
    let manager = fixture.session(Some("manager")).await;
    // Warm the management path first: its data must not enter shared caches.
    succeeded(&manager.call(&fixture.tool("config"), json!({})).await);
    let mut tokens = vec!["ordinary", "other"];
    if cfg!(feature = "oauth") {
        tokens.push("scoped");
    }
    for token in tokens {
        let user = fixture.session(Some(token)).await;
        succeeded(&user.call("api/ping", json!({})).await);
        let tools = user.rpc("tools/list", json!({})).await;
        assert!(
            tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| !t["name"].as_str().unwrap().starts_with("proxy/"))
        );
        for name in [
            "config",
            "add_backend",
            "list_backends",
            "session_count",
            "health_check",
        ] {
            denied(
                &user
                    .call(
                        &fixture.tool(name),
                        json!({"name":"forbidden","url":fixture.backend.url}),
                    )
                    .await,
            );
        }
        // Composite dispatch must not turn an ordinary name into authority.
        let composite = user.call("indirect_config", json!({})).await;
        assert!(!composite.to_string().contains("[REDACTED]"));
        let prompts = user.rpc("prompts/list", json!({})).await;
        assert!(
            prompts["result"]["prompts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| !p["name"].as_str().unwrap().starts_with("proxy/"))
        );
        denied(
            &user
                .rpc("prompts/get", json!({"name":"proxy/explain_config"}))
                .await,
        );
        succeeded(&user.rpc("prompts/get", json!({"name":"api/hello"})).await);
        succeeded(
            &user
                .rpc("resources/read", json!({"uri":"api/state://value"}))
                .await,
        );
        for (method, path) in [
            (Method::GET, "/admin/config"),
            (Method::GET, "/admin/backends"),
            (Method::GET, "/admin/sessions/detail"),
            (Method::POST, "/admin/backends/add"),
            (Method::PUT, "/admin/backends/api"),
            (Method::DELETE, "/admin/backends/api"),
            (Method::POST, "/admin/cache/clear"),
            (Method::PUT, "/admin/config"),
            (Method::POST, "/admin/config/validate"),
        ] {
            let response = Client::new()
                .request(method, format!("{}{path}", fixture.server.url))
                .bearer_auth(token)
                .json(&json!({"name":"forbidden","url":fixture.backend.url}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "ordinary credentials must not authorize HTTP management"
            );
        }
        let response = Client::new()
            .delete(format!(
                "{}/admin/sessions/{}",
                fixture.server.url, manager.id
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        succeeded(&manager.rpc("ping", json!({})).await);
    }
    assert_eq!(
        std::fs::read_to_string(fixture.temp.path().join("proxy.toml")).unwrap(),
        fixture.config
    );
    let tools = manager.rpc("tools/list", json!({})).await;
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["name"].as_str().unwrap().starts_with("proxy/"))
    );
    denied(&manager.call("api/ping", json!({})).await);
    #[cfg(feature = "skills")]
    succeeded(
        &manager
            .rpc("prompts/get", json!({"name":"proxy/explain_config"}))
            .await,
    );
    let added = manager
        .call(
            "proxy/add_backend",
            json!({"name":"managed","url":fixture.backend.url}),
        )
        .await;
    succeeded(&added);
    assert!(added.to_string().contains("added successfully"));
    let admin = Client::new();
    assert_eq!(
        admin
            .get(format!("{}/admin/config", fixture.server.url))
            .bearer_auth("manager")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let response = admin
        .put(format!("{}/admin/backends/managed", fixture.server.url))
        .bearer_auth("manager")
        .json(&json!({"url":fixture.backend.url}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = admin
        .post(format!("{}/admin/backends/add", fixture.server.url))
        .bearer_auth("manager")
        .json(&json!({"name":"http-managed","url":fixture.backend.url}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = admin
        .delete(format!("{}/admin/backends/managed", fixture.server.url))
        .bearer_auth("manager")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut edited = fixture.config.clone();
    edited.push_str("\n# administrator edit\n");
    let response = admin
        .put(format!("{}/admin/config", fixture.server.url))
        .bearer_auth("manager")
        .body(edited.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read_to_string(fixture.temp.path().join("proxy.toml")).unwrap(),
        edited
    );
    assert!(fixture.counter.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn missing_management_credential_never_falls_back_to_data_or_anonymous_access() {
    for auth in [false, true] {
        let fixture = Fixture::new(false, auth, false, "/", false).await;
        let mut tokens = vec![if auth { Some("ordinary") } else { None }];
        if auth && cfg!(feature = "oauth") {
            tokens.push(Some("scoped"));
        }
        for token in tokens {
            let user = fixture.session(token).await;
            succeeded(&user.call("api/ping", json!({})).await);
            denied(
                &user
                    .call(
                        "proxy/add_backend",
                        json!({"name":"forbidden","url":fixture.backend.url}),
                    )
                    .await,
            );
            denied(&user.call("proxy/config", json!({})).await);
            let mut request = Client::new().get(format!("{}/admin/config", fixture.server.url));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }
}

#[tokio::test]
async fn credential_bound_sessions_reject_replay_streaming_and_termination() {
    let fixture = Fixture::new(true, true, false, "/", false).await;
    let manager = fixture.session(Some("manager")).await;
    let user = fixture.session(Some("ordinary")).await;
    let other = fixture.session(Some("other")).await;
    for (victim, attacker) in [(&manager, &user), (&user, &manager), (&user, &other)] {
        let impersonated = Session {
            client: Client::new(),
            url: victim.url.clone(),
            token: attacker.token.clone(),
            id: victim.id.clone(),
        };
        let rejected = payload(
            impersonated
                .request("tools/call", json!({"name":"proxy/config","arguments":{}}))
                .await,
        )
        .await;
        assert_eq!(
            rejected["error"]["code"],
            tower_mcp_types::JsonRpcError::session_not_found_with_id("unknown").code
        );
        for method in [Method::GET, Method::DELETE] {
            let response = Client::new()
                .request(method, &victim.url)
                .header("accept", "text/event-stream")
                .header("mcp-session-id", &victim.id)
                .header("mcp-protocol-version", "2025-11-25")
                .bearer_auth(attacker.token.as_ref().unwrap())
                .send()
                .await
                .unwrap();
            if response.headers().get("content-type").is_some() {
                let rejected = payload(response).await;
                assert_eq!(
                    rejected["error"]["code"],
                    tower_mcp_types::JsonRpcError::session_not_found_with_id("unknown").code
                );
            } else {
                // DELETE is idempotent even for an absent/mismatched session.
                assert_eq!(response.status(), StatusCode::OK);
            }
        }
        succeeded(&victim.rpc("ping", json!({})).await);
    }
    // Static auth accepts raw/ApiKey spellings too: they must resolve to the
    // same credential identity, never to an anonymous session shared by peers.
    let headers = if cfg!(feature = "oauth") {
        vec!["Bearer ordinary"] // ScopedBearerAuthLayer accepts only this spelling.
    } else {
        vec!["ordinary", "ApiKey ordinary", "bearer ordinary"]
    };
    for header in headers {
        let response = Client::new()
            .post(&user.url)
            .header("authorization", header)
            .header("mcp-session-id", &user.id)
            .header("mcp-protocol-version", "2025-11-25")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let response = Client::new()
        .delete(format!("{}/admin/sessions/{}", fixture.server.url, user.id))
        .bearer_auth("manager")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let removed = payload(user.request("ping", json!({})).await).await;
    assert_eq!(
        removed["error"]["code"],
        tower_mcp_types::JsonRpcError::session_not_found_with_id("unknown").code
    );
}

#[cfg(feature = "discovery")]
#[tokio::test]
async fn search_wrapper_and_custom_separator_cannot_reach_management() {
    for separator in ["/", "::"] {
        let fixture = Fixture::new(true, true, true, separator, false).await;
        let user = fixture.session(Some("ordinary")).await;
        let name = fixture.tool("call_tool");
        succeeded(
            &user
                .call(
                    &name,
                    json!({"name":format!("api{separator}ping"),"arguments":{}}),
                )
                .await,
        );
        for target in [
            fixture.tool("config"),
            fixture.tool("add_backend"),
            name.clone(),
        ] {
            denied(&user.call(&name,json!({"name":target,"arguments":{"name":fixture.tool("config"),"arguments":{}}})).await);
        }
        let manager = fixture.session(Some("manager")).await;
        succeeded(&manager.call(&fixture.tool("config"), json!({})).await);
    }
}

#[tokio::test]
async fn management_prompts_and_resources_remain_private_with_custom_separators() {
    for separator in ["/", "::"] {
        let fixture = Fixture::new(true, true, false, separator, true).await;
        let manager = fixture.session(Some("manager")).await;
        let user = fixture.session(Some("ordinary")).await;
        for (method, key, field) in [
            ("tools/list", "tools", "name"),
            ("prompts/list", "prompts", "name"),
            ("resources/list", "resources", "uri"),
        ] {
            let ordinary = user.rpc(method, json!({})).await;
            assert!(ordinary["result"][key].as_array().unwrap().iter().all(|v| {
                !v[field]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("proxy{separator}"))
            }));
            let privileged = manager.rpc(method, json!({})).await;
            assert!(!privileged["result"][key].as_array().unwrap().is_empty());
            assert!(
                privileged["result"][key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|v| v[field]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("proxy{separator}")))
            );
        }
        for (method, params) in [
            (
                "resources/read",
                json!({"uri":format!("proxy{separator}state://value")}),
            ),
            (
                "prompts/get",
                json!({"name":format!("proxy{separator}hello")}),
            ),
        ] {
            denied(&user.rpc(method, params.clone()).await);
            succeeded(&manager.rpc(method, params).await);
        }
        for method in [
            "resources/subscribe",
            "resources/unsubscribe",
            "completion/complete",
        ] {
            let params = if method == "completion/complete" {
                json!({"ref":{"type":"ref/resource","uri":format!("proxy{separator}state://value")},"argument":{"name":"key","value":""}})
            } else {
                json!({"uri":format!("proxy{separator}state://value")})
            };
            denied(&user.rpc(method, params).await);
        }
        succeeded(
            &user
                .rpc(
                    "resources/read",
                    json!({"uri":format!("api{separator}state://value")}),
                )
                .await,
        );
    }
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn admin_authentication_is_independent_of_oauth_validation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let metadata = json!({"issuer":issuer,"jwks_uri":format!("{issuer}/unused-jwks"),"introspection_endpoint":format!("{issuer}/introspect")});
    let router = axum::Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            axum::routing::get(move || {
                let metadata = metadata.clone();
                async move { axum::Json(metadata) }
            }),
        )
        .route(
            "/introspect",
            axum::routing::post(|| async { axum::Json(json!({"active":false})) }),
        );
    let auth_server = Server {
        url: issuer.clone(),
        task: tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    };
    let backend =
        serve(HttpTransport::new(capabilities(Arc::new(AtomicUsize::new(0)))).into_router()).await;
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
[proxy]
name = "oauth-management-test"
[proxy.listen]
[security]
admin_token = "manager"
[auth]
type = "oauth"
issuer = "{issuer}"
audience = "https://mcp.example.com/mcp"
token_validation = "introspection"
client_id = "fixture-client"
client_secret = "fixture-secret"
required_scopes = ["tools"]
[[backends]]
name = "api"
transport = "http"
url = "{backend_url}"
"#,
        backend_url = backend.url
    ))
    .unwrap();
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    let server = serve(proxy.into_router().0).await;
    let manager = Session::new(&server.url, Some("manager")).await;
    succeeded(&manager.call("proxy/config", json!({})).await);
    let response = Client::new().post(format!("{}/",server.url)).bearer_auth("invalid-oauth-token")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"invalid-token","version":"1"}}}))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        Client::new()
            .get(format!("{}/admin/config", server.url))
            .bearer_auth("invalid-oauth-token")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    drop(auth_server);
}
