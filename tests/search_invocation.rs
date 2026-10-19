//! Search wrappers and composites must delegate with the caller's authority.
#![cfg(feature = "discovery")]

use std::sync::{Arc, Mutex};

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
            token: token.into(),
            id: None,
        };
        let response = session.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"search-regression","version":"1"}})).await;
        if response.status() != reqwest::StatusCode::OK {
            panic!(
                "initialization rejected: {}",
                response.text().await.unwrap()
            );
        }
        session.id = Some(
            response.headers()["mcp-session-id"]
                .to_str()
                .unwrap()
                .into(),
        );
        success(&payload(response).await);
        assert!(
            session
                .request("notifications/initialized", json!({}))
                .await
                .status()
                .is_success()
        );
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
            rpc["id"] = json!(42);
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
    async fn wrapped(&self, separator: &str, name: &str, arguments: Value) -> Value {
        self.call(
            &format!("proxy{separator}call_tool"),
            json!({"name":name,"arguments":arguments}),
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
fn success(value: &Value) {
    assert!(value.get("error").is_none(), "{value}");
    assert_ne!(value["result"]["isError"], true, "{value}");
}
fn denied(value: &Value) {
    assert!(
        value.get("error").is_some() || value["result"]["isError"] == true,
        "{value}"
    );
}

struct Fixture {
    server: Server,
    _backend: Server,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}
impl Fixture {
    async fn new(auth: &str, separator: &str) -> Self {
        let calls = Arc::new(Mutex::new(vec![]));
        let mut backend = McpRouter::new();
        for name in ["echo", "hidden", "private"] {
            let calls = calls.clone();
            backend = backend.tool(
                ToolBuilder::new(name)
                    .handler(move |arguments: Value| {
                        calls.lock().unwrap().push((name.into(), arguments.clone()));
                        async move { Ok(CallToolResult::json(arguments)) }
                    })
                    .build(),
            );
        }
        let backend = serve(HttpTransport::new(backend).into_router()).await;
        let config = mcp_proxy::ProxyConfig::parse(&format!(
            r#"
[proxy]
name = "search-test"
separator = "{separator}"
tool_exposure = "search"
[proxy.listen]
{auth}
[security]
admin_token = "manager"
max_argument_size = 256
[performance]
coalesce_requests = true
[[backends]]
name = "api"
transport = "http"
url = "{backend_url}/"
hide_tools = ["hidden"]
default_args = {{ region = "default-region" }}
[[backends.aliases]]
from = "echo"
to = "public"
[[backends.param_overrides]]
tool = "echo"
hide = ["secret"]
defaults = {{ secret = "fixed" }}
rename = {{ original = "public_arg" }}
[[backends.inject_args]]
tool = "echo"
args = {{ injected = true }}
overwrite = true
[backends.cache]
tool_ttl_seconds = 60
[[composite_tools]]
name = "allowed_composite"
description = "Allowed target"
tools = ["api{separator}public"]
[[composite_tools]]
name = "denied_composite"
description = "Denied target"
tools = ["api{separator}private"]
[[composite_tools]]
name = "mixed_composite"
description = "Independent target authorization"
tools = ["api{separator}public", "api{separator}private"]
[[composite_tools]]
name = "nested_composite"
description = "Unsupported nested target"
tools = ["allowed_composite"]
"#,
            backend_url = backend.url
        ))
        .unwrap();
        let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
        Self {
            server: serve(proxy.into_router().0).await,
            _backend: backend,
            calls,
        }
    }
}

// Also runs with discovery alone, without the OAuth/default feature bundle.
#[tokio::test]
async fn wrapper_matches_direct_filters_transforms_limits_and_cache() {
    for separator in ["/", "::"] {
        let fixture =
            Fixture::new("[auth]\ntype = 'bearer'\ntokens = ['ordinary']", separator).await;
        let session = Session::new(&fixture.server.url, "ordinary").await;
        let public = format!("api{separator}public");
        let args = json!({"public_arg":"client", "region":"client-region"});
        let direct = session.call(&public, args.clone()).await;
        success(&direct);
        let wrapped = session.wrapped(separator, &public, args.clone()).await;
        success(&wrapped);
        assert_eq!(direct["result"], wrapped["result"]);
        assert_eq!(
            *fixture.calls.lock().unwrap(),
            vec![(
                "echo".into(),
                json!({"original":"client","region":"client-region","secret":"fixed","injected":true})
            )]
        );
        // Exercise a cache miss through the wrapper as well as shared hits.
        let other_args = json!({"public_arg":"wrapped-client"});
        let wrapped = session
            .wrapped(separator, &public, other_args.clone())
            .await;
        success(&wrapped);
        let direct = session.call(&public, other_args).await;
        success(&direct);
        assert_eq!(direct["result"], wrapped["result"]);
        assert_eq!(
            fixture.calls.lock().unwrap()[1],
            (
                "echo".into(),
                json!({"original":"wrapped-client","region":"default-region","secret":"fixed","injected":true})
            )
        );
        for wrapped in [false, true] {
            let name = format!("api{separator}hidden");
            let result = if wrapped {
                session.wrapped(separator, &name, json!({})).await
            } else {
                session.call(&name, json!({})).await
            };
            denied(&result);
            let oversized = json!({"public_arg":"x".repeat(300)});
            let result = if wrapped {
                session.wrapped(separator, &public, oversized).await
            } else {
                session.call(&public, oversized).await
            };
            denied(&result);
        }
        assert_eq!(
            fixture.calls.lock().unwrap().len(),
            2,
            "denied calls must not reach backend"
        );
    }
}

#[cfg(feature = "oauth")]
async fn assert_scoped_delegation(fixture: &Fixture, token: &str) {
    let session = Session::new(&fixture.server.url, token).await;
    // No permission to the wrapper name is needed: the target is authorized.
    success(&session.wrapped("/", "api/public", json!({})).await);
    success(&session.call("allowed_composite", json!({})).await);
    success(&session.wrapped("/", "allowed_composite", json!({})).await);
    for wrapped in [false, true] {
        for name in [
            "api/private",
            "denied_composite",
            "mixed_composite",
            "nested_composite",
        ] {
            let result = if wrapped {
                session.wrapped("/", name, json!({})).await
            } else {
                session.call(name, json!({})).await
            };
            denied(&result);
        }
    }
    denied(
        &session
            .call("allowed_composite", json!({"large":"x".repeat(300)}))
            .await,
    );
    denied(
        &session
            .wrapped("/", "allowed_composite", json!({"large":"x".repeat(300)}))
            .await,
    );
    let calls = fixture.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "authorized cache reuse; denied targets receive zero calls"
    );
    assert_eq!(calls[0].0, "echo");
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn scoped_bearer_authorizes_actual_targets_and_composite_delegation() {
    let fixture = Fixture::new(
        r#"
[auth]
type = "bearer"
[[auth.scoped_tokens]]
token = "scoped"
allow_tools = ["api/public", "allowed_composite", "denied_composite", "mixed_composite", "nested_composite"]
"#,
        "/",
    )
    .await;
    assert_scoped_delegation(&fixture, "scoped").await;
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn wrapper_permission_cannot_bypass_target_policy_or_read_a_warm_cache() {
    let fixture = Fixture::new(
        r#"
[auth]
type = "bearer"
tokens = ["ordinary"]
[[auth.scoped_tokens]]
token = "wrapper-only"
allow_tools = ["proxy/call_tool", "denied_composite"]
"#,
        "/",
    )
    .await;
    let ordinary = Session::new(&fixture.server.url, "ordinary").await;
    success(&ordinary.call("api/private", json!({})).await);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    let scoped = Session::new(&fixture.server.url, "wrapper-only").await;
    for wrapped in [false, true] {
        for name in ["api/private", "api/public", "denied_composite"] {
            let result = if wrapped {
                scoped.wrapped("/", name, json!({})).await
            } else {
                scoped.call(name, json!({})).await
            };
            denied(&result);
        }
    }
    assert_eq!(
        fixture.calls.lock().unwrap().len(),
        1,
        "no denied target reaches backend, even with an authorized cached result"
    );
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn signed_jwt_preserves_role_authority_for_wrappers_and_composites() {
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    let jwk: Value = serde_json::from_str(include_str!("fixtures/rsa_public.jwk.json")).unwrap();
    let jwks = serve(axum::Router::new().route(
        "/jwks",
        axum::routing::get(move || {
            let jwk = jwk.clone();
            async move { axum::Json(json!({"keys":[jwk]})) }
        }),
    ))
    .await;
    let fixture = Fixture::new(
        &format!(
            r#"
[auth]
type = "jwt"
issuer = "{issuer}"
audience = "http://127.0.0.1:8080"
jwks_uri = "{issuer}/jwks"
[[auth.roles]]
name = "reader"
allow_tools = ["api/public", "allowed_composite", "denied_composite", "mixed_composite", "nested_composite"]
deny_tools = ["api/private"]
[auth.role_mapping]
claim = "scope"
default_deny = true
[auth.role_mapping.mapping]
read = "reader"
"#,
            issuer = jwks.url
        ),
        "/",
    )
    .await;
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300;
    let claims = json!({"iss":jwks.url,"aud":"http://127.0.0.1:8080","sub":"reader-one","scope":"read","exp":exp});
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key-1".into());
    let token = jsonwebtoken::encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(include_bytes!("fixtures/rsa_private.pem")).unwrap(),
    )
    .unwrap();
    assert_scoped_delegation(&fixture, &token).await;
}
