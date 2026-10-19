//! Configuration diagnostics must never return resolved credentials.
use mcp_proxy::config::ProxyConfig;
use tower::ServiceExt;

const SECRETS: &[&str] = &[
    "sentinel-inbound-token",
    "sentinel-scoped-token",
    "sentinel-admin-token",
    "sentinel-backend-token",
    "sentinel-header-value",
    "sentinel-environment-value",
    "sentinel-command-argument",
    "sentinel-default-argument",
    "sentinel-injected-argument",
    "sentinel-hidden-default",
    "sentinel-url-user",
    "sentinel-url-password",
    "sentinel-url-query",
    "sentinel-cache-password",
    "sentinel-tracing-password",
    "sentinel-instructions",
];

fn config(backend_url: &str) -> ProxyConfig {
    // The URL may contain user info/query credentials; arbitrary header/env
    // names ensure protection does not depend on a secret-key blacklist.
    ProxyConfig::parse(&format!(
        r#"
        [proxy]
        name = "redaction-test"
        instructions = "sentinel-instructions"
        [proxy.listen]
        [auth]
        type = "bearer"
        tokens = ["sentinel-inbound-token"]
        [[auth.scoped_tokens]]
        token = "sentinel-scoped-token"
        allow_tools = ["api/ping"]
        [security]
        admin_token = "sentinel-admin-token"
        [cache]
        url = "redis://user:sentinel-cache-password@localhost"
        [observability.tracing]
        endpoint = "http://user:sentinel-tracing-password@localhost:4317"
        [[backends]]
        name = "api"
        transport = "http"
        url = "{backend_url}"
        bearer_token = "sentinel-backend-token"
        args = ["--credential", "sentinel-command-argument"]
        env = {{ ORDINARY_NAME = "sentinel-environment-value" }}
        headers = {{ "X-Ordinary-Name" = "sentinel-header-value" }}
        default_args = {{ nested = {{ arbitrary = ["sentinel-default-argument"] }} }}
        [backends.timeout]
        seconds = 7
        [[backends.inject_args]]
        tool = "ping"
        args = {{ ordinary = "sentinel-injected-argument" }}
        [[backends.param_overrides]]
        tool = "ping"
        hide = ["credential"]
        defaults = {{ credential = "sentinel-hidden-default" }}
        "#
    ))
    .unwrap()
}

// Avoid Debug formatting the success value: it may contain credentials.
fn expect_failure<T, E>(result: Result<T, E>) -> E {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected failure; sensitive success value omitted"),
    }
}

fn assert_redacted(text: &str) {
    for (index, secret) in SECRETS.iter().enumerate() {
        assert!(
            !text.contains(secret),
            "credential sentinel {index} was disclosed"
        );
    }
    assert!(text.contains("[REDACTED]"));
    assert!(text.contains("redaction-test"));
    assert!(text.contains("http"));
    assert!(text.contains("seconds = 7"));
}

#[test]
fn parser_errors_and_cli_stderr_do_not_quote_config_source() {
    let input = "[proxy]\nname = \"sentinel-inbound-token\" trailing-input\n";
    let error = expect_failure(ProxyConfig::parse(input));
    for rendered in [format!("{error:#}"), format!("{error:?}")] {
        assert!(!rendered.contains("sentinel-inbound-token"));
        assert!(rendered.contains("byte"), "keep a useful error position");
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, input).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
        .args(["--check", "--config"])
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains("sentinel-inbound-token"));
    }
}

#[cfg(feature = "yaml")]
#[test]
fn yaml_errors_do_not_quote_rejected_string_values() {
    let input = "proxy: sentinel-client-secret\n";
    let error = expect_failure(ProxyConfig::parse_yaml(input));
    assert!(!format!("{error:#}").contains("sentinel-client-secret"));
    assert!(!format!("{error:?}").contains("sentinel-client-secret"));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.yaml");
    std::fs::write(&path, input).unwrap();
    let error = expect_failure(ProxyConfig::load(&path));
    assert!(!format!("{error:#}").contains("sentinel-client-secret"));
}

#[tokio::test]
async fn authenticated_http_admin_gets_only_redacted_diagnostics() {
    let proxy = tower_mcp::proxy::McpProxy::builder("redaction-test", "1")
        .backend(
            "fixture",
            tower_mcp::client::ChannelTransport::new(tower_mcp::McpRouter::new()),
        )
        .await
        .build_strict()
        .await
        .unwrap();
    let service = tower::util::BoxCloneService::new(proxy.clone());
    let (_, sessions) =
        tower_mcp::transport::http::HttpTransport::from_service(service).into_router_with_handle();
    let (state, _health_checker) = mcp_proxy::admin::spawn_health_checker(
        proxy.clone(),
        "redaction-test".into(),
        "1".into(),
        0,
        Default::default(),
    );
    let config =
        config("http://sentinel-url-user:sentinel-url-password@localhost/?key=sentinel-url-query");
    let router = mcp_proxy::admin::admin_router(
        state,
        None,
        sessions,
        None,
        proxy,
        &config,
        None,
        Default::default(),
    );
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/config")
                .header("authorization", "Bearer sentinel-admin-token")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert_redacted(&text);
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(parsed["backends"][0]["transport"].as_str(), Some("http"));
}

// Exercise actual configured MCP sessions so a test cannot accidentally bypass
// the admin-backend registration, snapshot generation, or scoped-token bridge.
#[cfg(feature = "oauth")]
#[tokio::test]
async fn configured_mcp_sessions_never_expose_secrets_to_ordinary_or_scoped_callers() {
    use tower_mcp::client::{HttpClientTransport, McpClient};
    use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    // The default current-thread test runtime keeps this dispatcher active
    // during the configured application's setup and request handling.
    let _guard = tracing::subscriber::set_default(subscriber);

    let backend = McpRouter::new().tool(
        ToolBuilder::new("ping")
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("pong")) })
            .build(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let backend_url = format!("http://{addr}/?key=sentinel-url-query");
    let backend = tower_mcp::transport::http::HttpTransport::new(backend).into_router();
    let backend_task = tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });
    let config = config(&backend_url);
    let proxy = mcp_proxy::Proxy::from_config(config).await.unwrap();
    let (router, _) = proxy.into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    for token in [
        "sentinel-inbound-token",
        "sentinel-scoped-token",
        "sentinel-admin-token",
    ] {
        let client = McpClient::connect(
            HttpClientTransport::new(format!("http://{addr}/")).bearer_token(token),
        )
        .await
        .unwrap();
        client
            .initialize("redaction-regression", "1")
            .await
            .unwrap();
        if token != "sentinel-admin-token" {
            let result = client
                .call_tool("api/ping", serde_json::json!({}))
                .await
                .unwrap();
            assert!(!result.is_error);
        }
        let result = client
            .call_tool("proxy/config", serde_json::json!({}))
            .await;
        if token != "sentinel-admin-token" {
            assert!(
                result.is_err(),
                "ordinary callers must not access the config tool"
            );
            let error = result.unwrap_err().to_string();
            for secret in SECRETS {
                assert!(!error.contains(secret));
            }
        } else {
            let serialized = serde_json::to_string(&result.unwrap()).unwrap();
            for (index, secret) in SECRETS.iter().enumerate() {
                assert!(
                    !serialized.contains(secret),
                    "config tool exposed credential sentinel {index}"
                );
            }
            assert!(serialized.contains("[REDACTED]"));
            assert!(serialized.contains("redaction-test"));
            let added = client
                .call_tool(
                    "proxy/add_backend",
                    serde_json::json!({
                        "name": "extra", "url": backend_url,
                    }),
                )
                .await
                .unwrap();
            let text = serde_json::to_string(&added).unwrap();
            assert!(text.contains("added successfully"));
            assert!(!text.contains("sentinel-url-query"));

            let failed = client
                .call_tool(
                    "proxy/add_backend",
                    serde_json::json!({
                        "name": "unreachable", "url": "http://127.0.0.1:0/?key=sentinel-error-url",
                    }),
                )
                .await
                .unwrap();
            let text = serde_json::to_string(&failed).unwrap();
            assert!(text.contains("Failed to add backend"));
            assert!(!text.contains("sentinel-error-url"));
        }
        #[cfg(feature = "skills")]
        for name in ["proxy/check_config", "proxy/explain_config"] {
            let prompt = client.get_prompt(name, None).await;
            if token != "sentinel-admin-token" {
                assert!(prompt.is_err());
                continue;
            }
            let prompt = prompt.unwrap();
            let text = serde_json::to_string(&prompt).unwrap();
            for (index, secret) in SECRETS.iter().enumerate() {
                assert!(
                    !text.contains(secret),
                    "prompt exposed credential sentinel {index}"
                );
            }
            assert!(text.contains("[REDACTED]"));
            assert!(text.contains("redaction-test"));
        }
    }
    let failed = reqwest::Client::new()
        .post(format!("http://{addr}/admin/backends/add"))
        .bearer_auth("sentinel-admin-token")
        .json(&serde_json::json!({
            "name": "http-unreachable", "url": "http://127.0.0.1:0/?key=sentinel-http-error-url",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), reqwest::StatusCode::CONFLICT);
    assert!(
        !failed
            .text()
            .await
            .unwrap()
            .contains("sentinel-http-error-url")
    );
    let captured = logs.0.lock().unwrap();
    let captured = String::from_utf8_lossy(&captured);
    assert!(
        captured.contains("MCP admin tools registered"),
        "capture must include application logs"
    );
    for (index, secret) in SECRETS
        .iter()
        .copied()
        .chain(["sentinel-error-url", "sentinel-http-error-url"])
        .enumerate()
    {
        assert!(
            !captured.contains(secret),
            "credential sentinel {index} appeared in logs"
        );
    }
    proxy_task.abort();
    backend_task.abort();
}
