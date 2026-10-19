//! Real configured OAuth sessions with an HTTPS public identity on a local bind.
#![cfg(feature = "oauth")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Form, Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

const RESOURCE: &str = "https://public.example.test/mcp";

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    launch(listener, router)
}
fn launch(listener: tokio::net::TcpListener, router: Router) -> Server {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}

#[derive(Clone)]
struct AuthorityState {
    issuer: String,
    base: String,
    mode: Arc<AtomicUsize>,
    discovery: Arc<AtomicUsize>,
    introspection: Arc<AtomicUsize>,
    tokens: Arc<Mutex<HashMap<String, Value>>>,
}
struct Authority {
    server: Server,
    state: AuthorityState,
}
impl Authority {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = AuthorityState {
            issuer: format!("{base}/tenant"),
            base,
            mode: Arc::new(AtomicUsize::new(0)),
            discovery: Arc::new(AtomicUsize::new(0)),
            introspection: Arc::new(AtomicUsize::new(0)),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        };
        let router = Router::new()
            .route(
                "/.well-known/oauth-authorization-server/tenant",
                get(metadata),
            )
            .route("/tenant/.well-known/openid-configuration", get(metadata))
            .route(
                "/keys",
                get(|| async {
                    let key: Value =
                        serde_json::from_str(include_str!("fixtures/rsa_public.jwk.json")).unwrap();
                    Json(json!({"keys":[key]}))
                }),
            )
            .route("/inspect", post(inspect))
            .with_state(state.clone());
        Self {
            server: launch(listener, router),
            state,
        }
    }
    fn claims(&self, subject: &str) -> Value {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        json!({"active":true,"iss":self.state.issuer,"sub":subject,"aud":RESOURCE,"scope":"tools read","exp":exp})
    }
    fn opaque(&self, name: &str, claims: Value) -> String {
        self.state
            .tokens
            .lock()
            .unwrap()
            .insert(name.into(), claims);
        name.into()
    }
    fn jwt(&self, claims: &Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test-key-1".into());
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!("fixtures/rsa_private.pem"))
                .unwrap(),
        )
        .unwrap()
    }
}
async fn metadata(State(state): State<AuthorityState>, uri: Uri) -> axum::response::Response {
    state.discovery.fetch_add(1, Ordering::SeqCst);
    let mode = state.mode.load(Ordering::SeqCst);
    if mode == 3 {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    if mode == 1 && uri.path().contains("oauth-authorization-server") {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "issuer":if mode == 2 { "https://different.example.test" } else { &state.issuer },
        "jwks_uri":format!("{}/keys",state.base),
        "introspection_endpoint":format!("{}/inspect",state.base)
    }))
    .into_response()
}
async fn inspect(
    State(state): State<AuthorityState>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    state.introspection.fetch_add(1, Ordering::SeqCst);
    let token = form.get("token").unwrap();
    if token == "slow" {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    if token == "oversized" {
        return Json(json!({"active":true,"padding":"x".repeat(70_000)}));
    }
    let response = state
        .tokens
        .lock()
        .unwrap()
        .get(token)
        .cloned()
        .unwrap_or(json!({"active":false}));
    Json(response)
}

struct Session {
    client: reqwest::Client,
    url: String,
    token: String,
    id: String,
}
impl Session {
    fn unaffiliated(base: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            url: format!("{base}/"),
            token: token.into(),
            id: String::new(),
        }
    }
    async fn start(base: &str, token: &str) -> Self {
        let mut session = Self::unaffiliated(base, token);
        let response = session.request("initialize", initialization(), token).await;
        assert_eq!(response.status(), StatusCode::OK);
        session.id = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .into();
        success(&payload(response).await);
        assert!(
            session
                .request("notifications/initialized", json!({}), token)
                .await
                .status()
                .is_success()
        );
        session
    }
    async fn request(&self, method: &str, params: Value, token: &str) -> reqwest::Response {
        let mut request = self
            .client
            .post(&self.url)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25");
        if !self.id.is_empty() {
            request = request.header("mcp-session-id", &self.id);
        }
        let mut body = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !method.starts_with("notifications/") {
            body["id"] = json!(1);
        }
        request.json(&body).send().await.unwrap()
    }
    async fn rpc(&self, method: &str, params: Value) -> Value {
        payload(self.request(method, params, &self.token).await).await
    }
}
fn initialization() -> Value {
    json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"oauth-regression","version":"1"}})
}
async fn payload(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    let text = text
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or(&text);
    serde_json::from_str(text).unwrap()
}
fn success(value: &Value) {
    assert!(value.get("error").is_none(), "{value}");
    assert_ne!(value["result"]["isError"], true);
}

struct Fixture {
    server: Server,
    _backend: Server,
    calls: Arc<AtomicUsize>,
}
async fn proxy(authority: &Authority, strategy: &str, explicit: bool) -> anyhow::Result<Fixture> {
    proxy_with_policy(authority, strategy, explicit, "").await
}
async fn proxy_with_policy(
    authority: &Authority,
    strategy: &str,
    explicit: bool,
    policy: &str,
) -> anyhow::Result<Fixture> {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let write_count = calls.clone();
    let delete_count = calls.clone();
    let backend = serve(
        HttpTransport::new(
            McpRouter::new()
                .tool(
                    ToolBuilder::new("write")
                        .handler(move |_: Value| {
                            write_count.fetch_add(1, Ordering::SeqCst);
                            async { Ok(CallToolResult::text("written")) }
                        })
                        .build(),
                )
                .tool(
                    ToolBuilder::new("delete")
                        .handler(move |_: Value| {
                            delete_count.fetch_add(1, Ordering::SeqCst);
                            async { Ok(CallToolResult::text("deleted")) }
                        })
                        .build(),
                )
                .tool(
                    ToolBuilder::new("ping")
                        .handler(move |_: Value| {
                            count.fetch_add(1, Ordering::SeqCst);
                            async { Ok(CallToolResult::text("pong")) }
                        })
                        .build(),
                ),
        )
        .into_router(),
    )
    .await;
    let auth = if strategy == "direct-jwt" {
        "jwt"
    } else {
        "oauth"
    };
    let strategy = if strategy == "direct-jwt" {
        "jwt"
    } else {
        strategy
    };
    let endpoints = if explicit {
        let mut fields = String::new();
        if strategy != "introspection" {
            fields += &format!("jwks_uri = '{}/keys'\n", authority.server.url);
        }
        if strategy != "jwt" {
            fields += &format!(
                "introspection_endpoint = '{}/inspect'\n",
                authority.server.url
            );
        }
        fields
    } else {
        String::new()
    };
    let config = mcp_proxy::ProxyConfig::parse(&format!(
        r#"
[proxy]
name = "oauth-contract"
[proxy.listen]
host = "127.0.0.1"
port = 0
[security]
admin_token = "manager"
[auth]
type = "{auth}"
issuer = "{issuer}"
audience = "{RESOURCE}"
token_validation = "{strategy}"
client_id = "fixture"
client_secret = "fixture-secret"
required_scopes = ["tools", "read"]
{endpoints}
{policy}
[[backends]]
name = "api"
transport = "http"
url = "{backend_url}/"
"#,
        issuer = authority.state.issuer,
        backend_url = backend.url
    ))?;
    let proxy = mcp_proxy::Proxy::from_config(config).await?;
    Ok(Fixture {
        server: serve(proxy.into_router().0).await,
        _backend: backend,
        calls,
    })
}

#[tokio::test]
async fn explicit_strategies_preserve_audience_scopes_issuer_and_session_ownership() {
    for strategy in ["direct-jwt", "jwt", "introspection", "both"] {
        let authority = Authority::new().await;
        let fixture = proxy(&authority, strategy, true).await.unwrap();
        assert_eq!(
            authority.state.discovery.load(Ordering::SeqCst),
            0,
            "explicit endpoints must skip discovery"
        );
        let client = reqwest::Client::new();
        let metadata: Value = client
            .get(format!(
                "{}/.well-known/oauth-protected-resource/mcp",
                fixture.server.url
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(metadata["resource"], RESOURCE);
        let challenge = client
            .post(format!("{}/", fixture.server.url))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(challenge.status(), StatusCode::UNAUTHORIZED);
        assert!(
            challenge.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .contains("https://public.example.test/.well-known/oauth-protected-resource/mcp")
        );
        for aud in [
            json!(RESOURCE),
            json!(["https://other.example.test", RESOURCE]),
        ] {
            let mut claims = authority.claims("owner-a");
            claims["aud"] = aud;
            let token = if strategy == "introspection" {
                authority.opaque("good", claims.clone())
            } else {
                authority.jwt(&claims)
            };
            let session = Session::start(&fixture.server.url, &token).await;
            success(
                &session
                    .rpc("tools/call", json!({"name":"api/ping","arguments":{}}))
                    .await,
            );
            let claims = authority.claims("owner-b");
            let other = if strategy == "introspection" {
                authority.opaque("other", claims)
            } else {
                authority.jwt(&claims)
            };
            let response = session
                .request(
                    "tools/call",
                    json!({"name":"api/ping","arguments":{}}),
                    &other,
                )
                .await;
            assert!(
                payload(response).await.get("error").is_some(),
                "session replay must be rejected"
            );
        }
        for mutation in [
            "missing-aud",
            "wrong-aud",
            "malformed-aud",
            "wrong-issuer",
            "expired",
            "missing-scope",
        ] {
            if mutation == "missing-scope" && strategy == "direct-jwt" {
                continue;
            }
            let mut claims = authority.claims("owner-a");
            match mutation {
                "missing-aud" => {
                    claims.as_object_mut().unwrap().remove("aud");
                }
                "wrong-aud" => claims["aud"] = json!("https://wrong.example.test"),
                "malformed-aud" => claims["aud"] = json!([RESOURCE, 1]),
                "wrong-issuer" => claims["iss"] = json!("https://wrong.example.test"),
                "expired" => claims["exp"] = json!(1),
                _ => claims["scope"] = json!("tools"),
            }
            let token = if strategy == "introspection" {
                authority.opaque(mutation, claims)
            } else {
                authority.jwt(&claims)
            };
            let request = Session::unaffiliated(&fixture.server.url, &token);
            let response = request
                .request("initialize", initialization(), &token)
                .await;
            assert!(
                payload(response).await.get("error").is_some(),
                "{strategy} must deny {mutation}"
            );
        }
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            2,
            "denied requests must never call the backend"
        );
        if strategy == "both" {
            let token = authority.opaque("opaque-fallback", authority.claims("owner-c"));
            let session = Session::start(&fixture.server.url, &token).await;
            success(
                &session
                    .rpc("tools/call", json!({"name":"api/ping","arguments":{}}))
                    .await,
            );
            assert!(authority.state.introspection.load(Ordering::SeqCst) > 0);
        }
    }
}

#[tokio::test]
async fn opaque_issuer_binding_and_client_ownership_survive_missing_optional_issuer() {
    let authority = Authority::new().await;
    let fixture = proxy(&authority, "introspection", true).await.unwrap();
    let mut claims = authority.claims("unused");
    claims.as_object_mut().unwrap().remove("iss");
    claims.as_object_mut().unwrap().remove("sub");
    claims["client_id"] = json!("client-a");
    let token = authority.opaque("client-a", claims.clone());
    let session = Session::start(&fixture.server.url, &token).await;
    success(
        &session
            .rpc("tools/call", json!({"name":"api/ping","arguments":{}}))
            .await,
    );
    claims["client_id"] = json!("client-b");
    let other = authority.opaque("client-b", claims);
    assert!(
        payload(
            session
                .request(
                    "tools/call",
                    json!({"name":"api/ping","arguments":{}}),
                    &other
                )
                .await
        )
        .await
        .get("error")
        .is_some()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn discovery_verifies_issuer_and_uses_rfc8414_paths_with_oidc_fallback() {
    for mode in [0, 1, 2] {
        let authority = Authority::new().await;
        authority.state.mode.store(mode, Ordering::SeqCst);
        let result = proxy(&authority, "both", false).await;
        if mode == 2 {
            let error = match result {
                Ok(_) => panic!("issuer mismatch must fail"),
                Err(e) => e,
            };
            assert!(format!("{error:#}").contains("issuer does not match"));
            assert_eq!(
                authority.state.discovery.load(Ordering::SeqCst),
                1,
                "issuer mismatch must not trigger fallback"
            );
        } else {
            let fixture = result.unwrap();
            let token = authority.opaque("discovered", authority.claims("discovered-owner"));
            let session = Session::start(&fixture.server.url, &token).await;
            success(
                &session
                    .rpc("tools/call", json!({"name":"api/ping","arguments":{}}))
                    .await,
            );
            assert_eq!(
                authority.state.discovery.load(Ordering::SeqCst),
                if mode == 0 { 1 } else { 2 }
            );
        }
    }
}

#[tokio::test]
async fn authentication_network_work_is_bounded_and_large_introspection_is_rejected() {
    use mcp_proxy::introspection::{IntrospectionValidator, discover_auth_server};
    use tower_mcp::oauth::TokenValidator;
    let authority = Authority::new().await;
    let validator = IntrospectionValidator::new(
        &format!("{}/inspect", authority.server.url),
        "fixture",
        "secret",
    )
    .expected_audience(RESOURCE);
    assert!(validator.validate_token("oversized").await.is_err());
    authority.state.mode.store(3, Ordering::SeqCst);
    let (discovery, introspection) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(12),
            discover_auth_server(&authority.state.issuer)
        ),
        tokio::time::timeout(Duration::from_secs(7), validator.validate_token("slow")),
    );
    assert!(
        discovery
            .expect("discovery must finish within its two request deadlines")
            .is_err()
    );
    assert!(
        introspection
            .expect("introspection must finish within its request deadline")
            .is_err()
    );
}

const RBAC_POLICY: &str = r#"
[[auth.roles]]
name = "reader"
allow_tools = ["api/ping"]
[[auth.roles]]
name = "writer"
allow_tools = ["api/write"]
deny_tools = ["api/delete"]
[[auth.roles]]
name = "admin"
[[auth.roles]]
name = "blocked"
deny_tools = ["api/write"]
[auth.role_mapping]
claim = "scope"
default_deny = true
[auth.role_mapping.mapping]
read = "reader"
write = "writer"
admin = "admin"
blocked = "blocked"
"#;

fn issued(authority: &Authority, strategy: &str, claims: Value) -> String {
    if strategy == "introspection" {
        // Unique opaque tokens let concurrent sessions retain their own claims.
        let name = format!("opaque-{}", authority.state.tokens.lock().unwrap().len());
        authority.opaque(&name, claims)
    } else {
        authority.jwt(&claims)
    }
}

async fn assert_tools(session: &Session, expected: &[&str]) {
    let listed = session.rpc("tools/list", json!({})).await;
    success(&listed);
    let mut names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, expected);
    for name in ["api/delete", "api/ping", "api/write"] {
        let response = session
            .rpc("tools/call", json!({"name":name,"arguments":{}}))
            .await;
        if expected.contains(&name) {
            success(&response);
        } else {
            assert!(response.get("error").is_some(), "denied {name}: {response}");
        }
    }
}

#[tokio::test]
async fn rbac_scopes_combine_grants_and_denies_independent_of_order() {
    for strategy in ["jwt", "introspection"] {
        let authority = Authority::new().await;
        let fixture = proxy_with_policy(&authority, strategy, true, RBAC_POLICY)
            .await
            .unwrap();
        let mut expected_calls = 0;
        for scopes in [
            "tools read write",
            "write tools read",
            "read write tools",
            "tools read admin blocked",
            "blocked admin read tools",
            "admin tools blocked read",
        ] {
            let mut claims = authority.claims("rbac-owner");
            claims["scope"] = json!(scopes);
            let token = issued(&authority, strategy, claims);
            let session = Session::start(&fixture.server.url, &token).await;
            let expected: &[&str] = if scopes.contains("admin") {
                &["api/delete", "api/ping"]
            } else {
                &["api/ping", "api/write"]
            };
            // Repeated identical claims must never vary across hash iterations.
            for _ in 0..8 {
                assert_tools(&session, expected).await;
                expected_calls += 2;
            }
        }
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            expected_calls,
            "rejected calls must not reach the backend"
        );
    }
}

#[tokio::test]
async fn rbac_custom_groups_survive_introspection_and_invalid_claims_fail_closed() {
    for strategy in ["jwt", "introspection"] {
        for default_deny in [true, false] {
            let authority = Authority::new().await;
            let policy = RBAC_POLICY
                .replace("claim = \"scope\"", "claim = \"groups\"")
                .replace(
                    "default_deny = true",
                    &format!("default_deny = {default_deny}"),
                );
            let fixture = proxy_with_policy(&authority, strategy, true, &policy)
                .await
                .unwrap();
            let mut expected_calls = 0;
            for groups in [
                json!(["read", "write"]),
                json!(["write", "read"]),
                json!(["admin", "blocked"]),
                json!(["blocked", "admin"]),
                json!("read write"),
                json!("write read"),
            ] {
                let mut claims = authority.claims("group-owner");
                claims["groups"] = groups.clone();
                let token = issued(&authority, strategy, claims);
                let session = Session::start(&fixture.server.url, &token).await;
                let expected: &[&str] = if groups.to_string().contains("admin") {
                    &["api/delete", "api/ping"]
                } else {
                    &["api/ping", "api/write"]
                };
                assert_tools(&session, expected).await;
                expected_calls += 2;
            }
            // No group or an unmapped group follows the explicit default policy.
            // JSON null, objects, numbers, and mixed arrays are always malformed.
            for (groups, malformed) in [
                (None, false),
                (Some(json!(["unknown"])), false),
                (Some(Value::Null), true),
                (Some(json!({"role":"admin"})), true),
                (Some(json!(1)), true),
                (Some(json!(["admin", 1])), true),
            ] {
                let mut claims = authority.claims("unmapped-owner");
                if let Some(groups) = groups {
                    claims["groups"] = groups;
                }
                let token = issued(&authority, strategy, claims);
                if default_deny || malformed {
                    let request = Session::unaffiliated(&fixture.server.url, &token);
                    let response = request
                        .request("initialize", initialization(), &token)
                        .await;
                    assert!(response.headers().get("mcp-session-id").is_none());
                    let response = payload(response).await;
                    assert!(response.get("error").is_some(), "{strategy}: {response}");
                } else {
                    let session = Session::start(&fixture.server.url, &token).await;
                    assert_tools(&session, &["api/delete", "api/ping", "api/write"]).await;
                    expected_calls += 3;
                }
            }
            assert_eq!(fixture.calls.load(Ordering::SeqCst), expected_calls);
        }
    }
}
