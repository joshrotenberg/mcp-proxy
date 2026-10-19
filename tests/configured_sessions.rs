//! External contracts through the configured router and the actual executable.
#[path = "support/app.rs"]
mod app;
#[path = "support/backend.rs"]
mod backend;
#[path = "support/http.rs"]
mod http;
use app::App;
use backend::Backend;
use http::{Session, success};
use serde_json::{Value, json};

async fn contract(kind: &str, executable: bool, search: bool) {
    let backend = Backend::new(kind).await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let exposure = if search { "search" } else { "direct" };
    let config = format!(
        r#"
[proxy]
name = "configured-session-contract"
tool_exposure = "{exposure}"
[proxy.listen]
host = "127.0.0.1"
port = {port}
[auth]
type = "bearer"
tokens = ["data-token"]
[[backends]]
name = "api"
{}
hide_tools = ["blocked"]
[[backends.inject_args]]
tool = "echo"
args = {{ injected = true }}
overwrite = true
"#,
        backend.config
    );
    drop(reservation);
    let mut app = if executable {
        App::cli(&config, port).await
    } else {
        App::embedded(&config).await
    };
    backend.assert_http_handshake();
    let unauthorized = http::client()
        .post(format!("{}/", app.url))
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":http::initialization()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(backend.calls().is_empty());

    let session = Session::start(&app.url, "data-token").await;
    let listed = session.rpc("tools/list", json!({})).await;
    let tools = success(&listed)["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    if search {
        assert!(names.contains(&"proxy/call_tool"), "{names:?}");
        assert!(!names.contains(&"api/echo"), "{names:?}");
    } else {
        assert_eq!(names, ["api/echo"]);
    }
    assert!(!names.contains(&"api/blocked"));
    assert!(backend.calls().is_empty(), "listing dispatched a tool");
    let (name, args) = if search {
        (
            "proxy/call_tool",
            json!({"name":"api/blocked","arguments":{}}),
        )
    } else {
        ("api/blocked", json!({}))
    };
    let rejected = session.call(name, args).await;
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert!(backend.calls().is_empty(), "policy denial reached backend");
    let args = json!({"message":"hello"});
    let result = if search {
        session
            .call(
                "proxy/call_tool",
                json!({"name":"api/echo","arguments":args}),
            )
            .await
    } else {
        session.call("api/echo", args).await
    };
    let expected = json!({"message":"hello","injected":true});
    let content = &success(&result)["content"];
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(
        serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        backend.calls(),
        vec![json!({"name":"echo","arguments":expected})]
    );
    session.terminate().await;
    assert_eq!(backend.calls().len(), 1);
    app.shutdown().await;
}

macro_rules! contracts {
    ($embed:ident, $cli:ident, $kind:literal, $search:literal) => {
        #[tokio::test]
        async fn $embed() {
            contract($kind, false, $search).await;
        }
        #[tokio::test]
        async fn $cli() {
            contract($kind, true, $search).await;
        }
    };
}
contracts!(embedded_stdio_direct, cli_stdio_direct, "stdio", false);
contracts!(embedded_http_direct, cli_http_direct, "http", false);
#[cfg(feature = "websocket")]
contracts!(
    embedded_websocket_direct,
    cli_websocket_direct,
    "websocket",
    false
);
#[cfg(feature = "discovery")]
contracts!(embedded_stdio_search, cli_stdio_search, "stdio", true);
#[cfg(feature = "discovery")]
contracts!(embedded_http_search, cli_http_search, "http", true);
#[cfg(all(feature = "websocket", feature = "discovery"))]
contracts!(
    embedded_websocket_search,
    cli_websocket_search,
    "websocket",
    true
);

#[cfg(feature = "oauth")]
#[path = "support/oauth.rs"]
mod oauth;
#[cfg(feature = "oauth")]
#[tokio::test]
async fn cli_oauth_introspection_completes_a_session_with_verified_audience() {
    let authority = oauth::Authority::new().await;
    let backend = Backend::new("http").await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let config = format!(
        r#"
[proxy]
name = "configured-oauth-contract"
[security]
admin_token = "manager"
[proxy.listen]
host = "127.0.0.1"
port = {port}
[auth]
type = "oauth"
issuer = "{}"
audience = "{}"
token_validation = "introspection"
introspection_endpoint = "{}/inspect"
client_id = "fixture"
client_secret = "fixture-secret"
required_scopes = ["tools", "read"]
[[backends]]
name = "api"
{}
"#,
        authority.state.issuer,
        oauth::RESOURCE,
        authority.server.url,
        backend.config
    );
    drop(reservation);
    let mut app = App::cli(&config, port).await;
    let mut wrong_audience = authority.claims("caller");
    wrong_audience["aud"] = json!("https://other.example.test/mcp");
    let invalid = authority.opaque("wrong-audience", wrong_audience);
    let response = http::client()
        .post(format!("{}/", app.url))
        .bearer_auth(invalid)
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":http::initialization()}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(backend.calls().is_empty());
    let token = authority.opaque("valid", authority.claims("caller"));
    let session = Session::start(&app.url, &token).await;
    let listed = session.rpc("tools/list", json!({})).await;
    assert!(
        success(&listed)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "api/echo")
    );
    let result = session.call("api/echo", json!({"message":"oauth"})).await;
    assert_eq!(
        serde_json::from_str::<Value>(success(&result)["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        json!({"message":"oauth"})
    );
    assert_eq!(
        backend.calls(),
        vec![json!({"name":"echo","arguments":{"message":"oauth"}})]
    );
    assert!(
        authority
            .state
            .introspection
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );
    session.terminate().await;
    app.shutdown().await;
}
