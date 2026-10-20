//! Credential forwarding is unavailable; fixed backend identity is observable on wire.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;
#[cfg(feature = "oauth")]
#[path = "support/oauth.rs"]
mod oauth;

use axum::http::{HeaderMap, StatusCode};
use mcp_proxy::{Proxy, ProxyConfig};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

const UNSUPPORTED: &str = "backend.forward_auth = true is unsupported";

#[tokio::test]
async fn forwarding_rejected_for_every_transport_auth_and_build_before_effects() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    for transport in ["stdio", "http", "websocket"] {
        for auth in [
            Value::Null,
            json!({"type":"bearer","tokens":["private-inbound"]}),
            json!({"type":"jwt","issuer":"https://issuer.test","audience":"https://proxy.test/mcp","jwks_uri":"https://issuer.test/keys"}),
            json!({"type":"oauth","issuer":"https://issuer.test","audience":"https://proxy.test/mcp","token_validation":"introspection","introspection_endpoint":"https://issuer.test/inspect","client_id":"client","client_secret":"private-client"}),
        ] {
            let mut value = json!({
                "proxy":{"name":"unsupported","listen":{}}, "auth":auth,
                "security":{"admin_token":"manager"},
                "backends":[
                    {"name":"first","transport":"stdio","command":"touch","args":[marker]},
                    {"name":"api","transport":transport,"command":"touch","args":[marker],
                    "url":format!("{}://{addr}/",if transport=="websocket" {"ws"} else {"http"}),
                    "bearer_token":"private-backend","headers":{"Authorization":"private-header"},"forward_auth":true}
                ]
            });
            if transport == "stdio" {
                value["backends"][1]["headers"] = json!({});
                value["backends"][1]["bearer_token"] = Value::Null;
            }
            let mut config: ProxyConfig = serde_json::from_value(value).unwrap();
            let error = config.prepare().unwrap_err().to_string();
            assert!(error.contains(UNSUPPORTED), "{transport}: {error}");
            assert!(!error.contains("private-"));
            let text = toml::to_string(&config).unwrap();
            assert!(
                ProxyConfig::parse(&text)
                    .unwrap_err()
                    .to_string()
                    .contains(UNSUPPORTED)
            );
            let result = tokio::time::timeout(Duration::from_secs(2), Proxy::from_config(config))
                .await
                .expect("rejection must precede network/process startup");
            assert!(matches!(result,Err(error) if error.to_string().contains(UNSUPPORTED)));
            assert!(!marker.exists());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

fn peer(calls: Arc<AtomicUsize>) -> McpRouter {
    McpRouter::new().tool(
        ToolBuilder::new("identity")
            .handler(move |_: Value| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(CallToolResult::text("fixed-backend-result")) }
            })
            .build(),
    )
}

fn capture(router: axum::Router, headers: Arc<Mutex<Vec<HeaderMap>>>) -> axum::Router {
    router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let headers = headers.clone();
            async move {
                headers.lock().unwrap().push(req.headers().clone());
                next.run(req).await
            }
        },
    ))
}

async fn fixed_identity_contract(transport: &str, auth: &str, callers: [&str; 2]) {
    // Cover no backend credential, static bearer, and case-insensitive explicit
    // Authorization precedence. Cached results intentionally share this fixed
    // backend identity; this is not a per-caller cache/delegation contract.
    for (credentials, expected) in [
        ("", None),
        ("bearer_token='backend-only'", Some("Bearer backend-only")),
        (
            "bearer_token='unused-backend'\n[backends.headers]\naUtHoRiZaTiOn='ApiKey backend-only'",
            Some("ApiKey backend-only"),
        ),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let service = peer(calls.clone());
        let router = match transport {
            "http" => HttpTransport::new(service)
                .session_config(tower_mcp::transport::http::SessionConfig {
                    strict_initialization: true,
                    ..Default::default()
                })
                .into_router(),
            #[cfg(feature = "websocket")]
            "websocket" => {
                tower_mcp::transport::websocket::WebSocketTransport::new(service).into_router()
            }
            _ => unreachable!(),
        };
        let mut backend = http::Server::start(capture(router, headers.clone())).await;
        let url = if transport == "websocket" {
            backend.url.replacen("http://", "ws://", 1)
        } else {
            backend.url.clone()
        };
        let config = format!(
            "[proxy]\nname='fixed-identity'\n[proxy.listen]\n[security]\nadmin_token='manager'\n{auth}\n[[backends]]\nname='api'\ntransport='{transport}'\nurl='{url}/'\nforward_auth=false\n{credentials}\n[backends.cache]\ntool_ttl_seconds=60\n"
        );
        let mut app = app::App::embedded(&config).await;
        let first = http::Session::start(&app.url, callers[0]).await;
        let listed = first.rpc("tools/list", json!({})).await;
        assert_eq!(http::success(&listed)["tools"][0]["name"], "api/identity");
        let result = first.call("api/identity", json!({})).await;
        assert_eq!(
            http::success(&result)["content"][0]["text"],
            "fixed-backend-result"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Same session cannot silently switch to another valid caller.
        let denied = first.client.post(&first.url).bearer_auth(callers[1])
            .header("accept","application/json, text/event-stream")
            .header("mcp-session-id",&first.id).header("mcp-protocol-version",http::VERSION)
            .json(&json!({"jsonrpc":"2.0","id":90,"method":"tools/call","params":{"name":"api/identity","arguments":{}}}))
            .send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::OK);
        let denied = http::payload(denied).await;
        // Session ownership is checked before decoding the JSON-RPC body.
        assert_eq!(denied["id"], Value::Null);
        assert_eq!(
            denied["error"]["code"],
            tower_mcp_types::JsonRpcError::session_not_found_with_id(&first.id).code,
            "{denied}"
        );
        assert!(denied.get("result").is_none());
        let denied = first.client.post(&first.url).bearer_auth("untrusted-inbound")
            .header("accept","application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":91,"method":"initialize","params":http::initialization()}))
            .send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let second = http::Session::start(&app.url, callers[1]).await;
        let result = second.call("api/identity", json!({})).await;
        assert_eq!(
            http::success(&result)["content"][0]["text"],
            "fixed-backend-result"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fixed-identity cache should serve the second caller without another backend call"
        );
        first.terminate().await;
        second.terminate().await;
        app.shutdown().await;
        {
            let requests = headers.lock().unwrap();
            assert!(!requests.is_empty(), "must capture actual backend traffic");
            if transport == "http" {
                assert!(
                    requests.len() >= 4,
                    "initialize, initialized, discovery and tool call"
                );
            }
            for headers in requests.iter() {
                let actual: Vec<_> = headers
                    .get_all("authorization")
                    .iter()
                    .map(|value| value.to_str().unwrap())
                    .collect();
                assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
                // Covers all headers, not just Authorization.
                for value in headers.values() {
                    let value = value.to_str().unwrap();
                    for caller in callers {
                        assert!(
                            !value.contains(caller),
                            "inbound credential reached a backend"
                        );
                    }
                    assert!(!value.contains("untrusted-inbound"));
                }
            }
        }
        backend.stop().await;
    }
}

#[tokio::test]
async fn static_callers_never_reach_http_headers_and_share_only_fixed_backend_cache() {
    fixed_identity_contract(
        "http",
        "[auth]\ntype='bearer'\ntokens=['caller-one','caller-two']",
        ["caller-one", "caller-two"],
    )
    .await;
}

#[cfg(feature = "websocket")]
#[tokio::test]
async fn static_callers_never_reach_websocket_handshake() {
    fixed_identity_contract(
        "websocket",
        "[auth]\ntype='bearer'\ntokens=['caller-one','caller-two']",
        ["caller-one", "caller-two"],
    )
    .await;
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn verified_jwt_callers_do_not_change_backend_credentials() {
    let mut authority = oauth::Authority::new().await;
    let one = authority.jwt(&authority.claims("user-one"));
    let two = authority.jwt(&authority.claims("user-two"));
    let auth = format!(
        "[auth]\ntype='jwt'\nissuer='{}'\naudience='{}'\njwks_uri='{}/keys'",
        authority.state.issuer,
        oauth::RESOURCE,
        authority.server.url
    );
    fixed_identity_contract("http", &auth, [&one, &two]).await;
    #[cfg(feature = "websocket")]
    fixed_identity_contract("websocket", &auth, [&one, &two]).await;
    authority.server.stop().await;
}

#[cfg(feature = "oauth")]
#[tokio::test]
async fn introspected_callers_do_not_change_backend_credentials() {
    let mut authority = oauth::Authority::new().await;
    let one = authority.opaque("opaque-one", authority.claims("user-one"));
    let two = authority.opaque("opaque-two", authority.claims("user-two"));
    let auth = format!(
        "[auth]\ntype='oauth'\nissuer='{}'\naudience='{}'\ntoken_validation='introspection'\nintrospection_endpoint='{}/inspect'\nclient_id='fixture'\nclient_secret='fixture-secret'",
        authority.state.issuer,
        oauth::RESOURCE,
        authority.server.url
    );
    fixed_identity_contract("http", &auth, [&one, &two]).await;
    #[cfg(feature = "websocket")]
    fixed_identity_contract("websocket", &auth, [&one, &two]).await;
    authority.server.stop().await;
}

#[tokio::test]
async fn same_origin_redirect_retains_only_configured_backend_credentials() {
    use axum::response::Redirect;
    let calls = Arc::new(AtomicUsize::new(0));
    let headers = Arc::new(Mutex::new(Vec::new()));
    let router = HttpTransport::new(peer(calls.clone())).into_router().route(
        "/entry/",
        axum::routing::any(|| async { Redirect::temporary("/") }),
    );
    let mut backend = http::Server::start(capture(router, headers.clone())).await;
    let config = format!(
        "[proxy]\nname='redirect'\n[proxy.listen]\n[auth]\ntype='bearer'\ntokens=['caller-private']\n[[backends]]\nname='api'\ntransport='http'\nurl='{}/entry/'\nbearer_token='backend-only'",
        backend.url
    );
    let mut app = app::App::embedded(&config).await;
    let session = http::Session::start(&app.url, "caller-private").await;
    assert_eq!(
        http::success(&session.call("api/identity", json!({})).await)["content"][0]["text"],
        "fixed-backend-result"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    session.terminate().await;
    app.shutdown().await;
    for headers in headers.lock().unwrap().iter() {
        assert_eq!(headers["authorization"], "Bearer backend-only");
    }
    backend.stop().await;
}

async fn rejected_redirect(transport: &str) {
    use axum::response::Redirect;
    let hits = Arc::new(AtomicUsize::new(0));
    let target_hits = hits.clone();
    let mut target = http::Server::start(axum::Router::new().fallback(move || {
        target_hits.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::OK }
    }))
    .await;
    let destination = target.url.clone();
    let headers = Arc::new(Mutex::new(Vec::new()));
    let mut source = http::Server::start(capture(
        axum::Router::new().fallback(move || {
            let destination = destination.clone();
            async move { Redirect::temporary(&destination) }
        }),
        headers.clone(),
    ))
    .await;
    let url = if transport == "websocket" {
        source.url.replacen("http://", "ws://", 1)
    } else {
        source.url.clone()
    };
    let config=ProxyConfig::parse(&format!("[proxy]\nname='redirect'\n[proxy.listen]\n[[backends]]\nname='api'\ntransport='{transport}'\nurl='{url}'\nbearer_token='backend-only'\n[backends.headers]\nX-API-Key='private-backend-key'")).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), Proxy::from_config(config))
        .await
        .expect("redirect rejection deadline");
    assert!(
        result.is_err(),
        "redirected initialization unexpectedly succeeded"
    );
    assert!(!headers.lock().unwrap().is_empty());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "untrusted redirect target was contacted"
    );
    source.stop().await;
    target.stop().await;
}

#[tokio::test]
async fn cross_origin_http_redirect_is_rejected_without_contacting_destination() {
    rejected_redirect("http").await;
}
#[cfg(feature = "websocket")]
#[tokio::test]
async fn websocket_redirect_is_rejected_without_contacting_destination() {
    rejected_redirect("websocket").await;
}
