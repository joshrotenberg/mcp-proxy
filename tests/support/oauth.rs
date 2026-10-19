//! Local OAuth/JWKS/introspection authority; modes retain failure/latency controls.
#![allow(dead_code)]
use super::http::Server;
use axum::{
    Form, Json, Router,
    extract::State,
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
pub const RESOURCE: &str = "https://public.example.test/mcp";

#[derive(Clone)]
pub struct AuthorityState {
    pub issuer: String,
    pub base: String,
    pub mode: Arc<AtomicUsize>,
    pub discovery: Arc<AtomicUsize>,
    pub introspection: Arc<AtomicUsize>,
    pub tokens: Arc<Mutex<HashMap<String, Value>>>,
}
pub struct Authority {
    pub server: Server,
    pub state: AuthorityState,
}
impl Authority {
    pub async fn new() -> Self {
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
                        serde_json::from_str(include_str!("../fixtures/rsa_public.jwk.json"))
                            .unwrap();
                    Json(json!({"keys":[key]}))
                }),
            )
            .route("/inspect", post(inspect))
            .with_state(state.clone());
        Self {
            server: Server::launch(listener, router),
            state,
        }
    }
    pub fn claims(&self, subject: &str) -> Value {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 300;
        json!({"active":true,"iss":self.state.issuer,"sub":subject,"aud":RESOURCE,"scope":"tools read","exp":exp})
    }
    pub fn opaque(&self, name: &str, claims: Value) -> String {
        self.state
            .tokens
            .lock()
            .unwrap()
            .insert(name.into(), claims);
        name.into()
    }
    pub fn jwt(&self, claims: &Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("test-key-1".into());
        jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!("../fixtures/rsa_private.pem"))
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
