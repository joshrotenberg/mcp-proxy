//! Configured HTTP contracts for both protocol eras and compile-time boundaries.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[allow(dead_code)]
#[path = "support/backend.rs"]
mod backend;
#[path = "support/http.rs"]
mod http;

use app::App;
use backend::Backend;
use http::{payload, success};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};

const MODERN: &str = "2026-07-28";
async fn start(cli: bool) -> (App, Backend) {
    let backend = Backend::new("http").await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let config = format!(
        "[proxy]\nname='protocol-contract'\n[proxy.listen]\nhost='127.0.0.1'\nport={port}\n[auth]\ntype='bearer'\ntokens=['data']\n[security]\nadmin_token='manager'\n[[backends]]\nname='api'\n{}\nhide_tools=['blocked']\n[[backends.aliases]]\nfrom='echo'\nto='public'\n[[backends.inject_args]]\ntool='echo'\nargs={{injected=true}}\noverwrite=true\n",
        backend.config
    );
    drop(reservation);
    let app = if cli {
        App::cli(&config, port).await
    } else {
        App::embedded(&config).await
    };
    (app, backend)
}
async fn count(app: &App, expected: usize) {
    let response = http::client()
        .get(format!("{}/admin/sessions", app.url))
        .bearer_auth("manager")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"active_sessions":expected})
    );
}
fn request(app: &App, version: &str, body: &Value) -> reqwest::RequestBuilder {
    request_token(app, version, body, "data")
}
fn request_token(app: &App, version: &str, body: &Value, token: &str) -> reqwest::RequestBuilder {
    http::client()
        .post(format!("{}/", app.url))
        .bearer_auth(token)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", version)
        .json(body)
}
fn exact_echo(reply: &Value, id: i64) {
    assert_eq!(reply["id"], id);
    let result = success(reply);
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        json!({"message":"protocol", "injected":true})
    );
}
async fn legacy(cli: bool) {
    let (mut app, backend) = start(cli).await;
    for (index, version) in [
        "2025-03-26",
        "2025-06-18",
        "2025-11-25",
        "2026-07-28",
        "2099-01-01",
    ]
    .into_iter()
    .enumerate()
    {
        count(&app, 0).await;
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"version-contract","version":"1"}}});
        let response = request(&app, "2025-11-25", &init).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{version}");
        let session = response.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_string();
        let reply = payload(response).await;
        assert_eq!(reply["id"], 1);
        // The pinned McpProxy advertises the documented session baseline, not
        // every transport revision. Clients continue with the selected version.
        assert_eq!(success(&reply)["protocolVersion"], "2025-11-25");
        let version = "2025-11-25";
        let notified = request(
            &app,
            version,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        )
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
        assert_eq!(notified.status(), StatusCode::ACCEPTED);
        assert!(notified.bytes().await.unwrap().is_empty());
        count(&app, 1).await;
        let listed = request(
            &app,
            version,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        )
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let reply = payload(listed).await;
        assert_eq!(reply["id"], 2);
        assert_eq!(
            success(&reply)["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["api/public"]
        );
        let called = request(&app, version, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"api/public","arguments":{"message":"protocol"}}})).header("mcp-session-id", &session).send().await.unwrap();
        assert_eq!(called.status(), StatusCode::OK);
        exact_echo(&payload(called).await, 3);
        let denied = request(&app, version, &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"api/blocked","arguments":{}}})).header("mcp-session-id", &session).send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::OK);
        let reply = payload(denied).await;
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(
            backend.calls().len(),
            index + 1,
            "denied call must not execute"
        );
        let deleted = http::client()
            .delete(format!("{}/", app.url))
            .bearer_auth("data")
            .header("mcp-protocol-version", version)
            .header("mcp-session-id", &session)
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);
        count(&app, 0).await;
    }
    assert_eq!(
        backend.calls(),
        vec![json!({"name":"echo","arguments":{"message":"protocol","injected":true}}); 5]
    );
    app.shutdown().await;
}
#[tokio::test]
async fn configured_legacy_initialization_selects_supported_baseline() {
    legacy(false).await;
}
#[tokio::test]
async fn cli_legacy_initialization_selects_supported_baseline() {
    legacy(true).await;
}

fn modern(method: &str, id: i64, mut params: Value) -> Value {
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":MODERN,"io.modelcontextprotocol/clientCapabilities":{}});
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
async fn post_modern(app: &App, body: &Value) -> Response {
    let mut builder =
        request(app, MODERN, body).header("mcp-method", body["method"].as_str().unwrap());
    if let Some(name) = body["params"]["name"].as_str() {
        builder = builder.header("mcp-name", name);
    }
    builder.send().await.unwrap()
}
async fn modern_contract(cli: bool) {
    let (mut app, backend) = start(cli).await;
    let body = modern("tools/list", 10, json!({}));
    let response = post_modern(&app, &body).await;
    if cfg!(feature = "protocol-2026-07-28") {
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("mcp-session-id").is_none());
        let reply = payload(response).await;
        assert_eq!(reply["id"], 10);
        assert_eq!(success(&reply)["tools"][0]["name"], "api/public");
        assert_eq!(success(&reply)["tools"].as_array().unwrap().len(), 1);
        let called = post_modern(
            &app,
            &modern(
                "tools/call",
                11,
                json!({"name":"api/public","arguments":{"message":"protocol"}}),
            ),
        )
        .await;
        assert_eq!(called.status(), StatusCode::OK);
        assert!(called.headers().get("mcp-session-id").is_none());
        exact_echo(&payload(called).await, 11);
        let denied = post_modern(
            &app,
            &modern(
                "tools/call",
                12,
                json!({"name":"api/blocked","arguments":{}}),
            ),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::OK);
        let reply = payload(denied).await;
        assert_eq!(reply["id"], 12);
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(
            backend.calls(),
            vec![json!({"name":"echo","arguments":{"message":"protocol","injected":true}})]
        );
    } else {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply = payload(response).await;
        assert_eq!(reply["id"], 10);
        assert_eq!(reply["error"]["code"], -32022);
        assert!(backend.calls().is_empty());
    }
    count(&app, 0).await;
    app.shutdown().await;
}
#[tokio::test]
async fn configured_modern_protocol_is_sessionless_or_explicitly_unsupported() {
    modern_contract(false).await;
}
#[tokio::test]
async fn cli_modern_protocol_is_sessionless_or_explicitly_unsupported() {
    modern_contract(true).await;
}

#[tokio::test]
async fn malformed_modern_requests_and_credentials_never_dispatch() {
    let (mut app, backend) = start(false).await;
    let valid = modern(
        "tools/call",
        50,
        json!({"name":"api/public","arguments":{}}),
    );
    let unauthorized = request_token(&app, MODERN, &valid, "invalid")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "api/public")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    for (body, version, method, name, code) in [
        (
            json!({"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"api/public","arguments":{}}}),
            MODERN,
            "tools/call",
            "api/public",
            -32602,
        ),
        (
            valid.clone(),
            "2025-11-25",
            "tools/call",
            "api/public",
            -32020,
        ),
    ] {
        let response = request(&app, version, &body)
            .header("mcp-method", method)
            .header("mcp-name", name)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply = payload(response).await;
        assert_eq!(reply["id"], 50);
        assert_eq!(reply["error"]["code"], code);
    }
    #[cfg(feature = "protocol-2026-07-28")]
    {
        for response in [
            request(&app, MODERN, &valid)
                .header("mcp-name", "api/public")
                .send()
                .await
                .unwrap(),
            request(&app, MODERN, &valid)
                .header("mcp-method", "tools/list")
                .header("mcp-name", "api/public")
                .send()
                .await
                .unwrap(),
            request(&app, MODERN, &valid)
                .header("mcp-method", "tools/call")
                .header("mcp-name", "api/blocked")
                .send()
                .await
                .unwrap(),
            request(&app, MODERN, &valid)
                .header("mcp-method", "tools/call")
                .header("mcp-method", "tools/call")
                .header("mcp-name", "api/public")
                .send()
                .await
                .unwrap(),
        ] {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let reply = payload(response).await;
            assert_eq!(reply["id"], 50);
            assert_eq!(reply["error"]["code"], -32020);
        }
        let removed = post_modern(&app, &modern("initialize", 51, json!({"protocolVersion":MODERN,"capabilities":{},"clientInfo":{"name":"removed","version":"1"}}))).await;
        assert_eq!(removed.status(), StatusCode::NOT_FOUND);
        let reply = payload(removed).await;
        assert_eq!(reply["id"], 51);
        assert_eq!(reply["error"]["code"], -32601);
    }
    assert!(backend.calls().is_empty());
    count(&app, 0).await;
    app.shutdown().await;
}
