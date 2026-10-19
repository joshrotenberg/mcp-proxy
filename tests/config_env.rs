//! Required secrets must fail before runtime construction or backend effects.
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::time::Duration;

use mcp_proxy::config::{AuthConfig, EnvResolutionError, EnvResolutionErrorKind, ProxyConfig};

const BASE: &str = r#"
[proxy]
name = "required-secrets"
[proxy.listen]
[security]
admin_token = "manager"
[[backends]]
name = "api"
transport = "stdio"
command = "echo"
"#;

fn reference_config(field: &str, reference: &str) -> ProxyConfig {
    let mut config = ProxyConfig::parse(BASE).unwrap();
    match field {
        "admin" => config.security.admin_token = Some(reference.into()),
        "token" | "scoped" => {
            config.auth = Some(AuthConfig::Bearer {
                tokens: if field == "token" {
                    vec![reference.into()]
                } else {
                    vec![]
                },
                scoped_tokens: if field == "scoped" {
                    vec![mcp_proxy::config::BearerTokenConfig {
                        token: reference.into(),
                        allow_tools: vec!["api/read".into()],
                        deny_tools: vec![],
                    }]
                } else {
                    vec![]
                },
            });
        }
        "backend" => config.backends[0].bearer_token = Some(reference.into()),
        "env" => {
            config.backends[0]
                .env
                .insert("API_TOKEN".into(), reference.into());
        }
        "header" => {
            config.backends[0].transport = mcp_proxy::config::TransportType::Http;
            config.backends[0].url = Some("http://127.0.0.1:1".into());
            config.backends[0]
                .headers
                .insert("X-API-Key".into(), reference.into());
        }
        "oauth" => {
            config = ProxyConfig::parse(&format!("{BASE}\n[auth]\ntype='oauth'\nissuer='https://issuer.test'\naudience='gateway'\ntoken_validation='introspection'\nclient_id='fixture'\nclient_secret='{reference}'")).unwrap();
        }
        _ => unreachable!(),
    }
    config
}

async fn cli(
    config: &ProxyConfig,
    check: bool,
    environment: &[(&str, &OsStr)],
) -> std::process::Output {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, toml::to_string(config).unwrap()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"));
    command.kill_on_drop(true).arg("--config").arg(path);
    if check {
        command.arg("--check");
    }
    for (name, value) in environment {
        command.env(name, value);
    }
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("configuration validation must terminate before serving")
        .unwrap()
}

fn rejected(output: &std::process::Output, expected: &str) {
    assert!(!output.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostic.contains(expected), "{diagnostic}");
    assert!(!diagnostic.contains("Config OK"));
    assert!(
        !diagnostic.contains("sentinel"),
        "must not echo secret values"
    );
}

#[tokio::test]
async fn check_and_startup_reject_every_missing_required_reference() {
    let absent = format!("MCP_PROXY_MISSING_{}", uuid::Uuid::new_v4().simple());
    let reference = format!("${{{absent}}}");
    for field in [
        "admin", "token", "scoped", "backend", "env", "header", "oauth",
    ] {
        let config = reference_config(field, &reference);
        for check in [true, false] {
            rejected(
                &cli(&config, check, &[]).await,
                "required environment variable is not set",
            );
        }
    }
}

#[tokio::test]
async fn library_rejects_missing_references_before_starting_a_backend() {
    let absent = format!("MCP_PROXY_MISSING_{}", uuid::Uuid::new_v4().simple());
    let reference = format!("${{{absent}}}");
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("backend-started");
    for field in [
        "admin", "token", "scoped", "backend", "env", "header", "oauth",
    ] {
        let mut config = reference_config(field, &reference);
        config.backends[0].command = Some("touch".into());
        config.backends[0].args = vec![marker.to_string_lossy().into()];
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            mcp_proxy::Proxy::from_config(config),
        )
        .await
        .unwrap();
        let error = match result {
            Ok(_) => panic!("missing {field} reference must fail"),
            Err(e) => e,
        };
        let error = error
            .downcast_ref::<EnvResolutionError>()
            .expect("structured resolution error");
        assert_eq!(error.kind, EnvResolutionErrorKind::Missing);
        assert!(
            !marker.exists(),
            "runtime construction must not start the backend"
        );
    }
}

#[tokio::test]
async fn cli_rejects_empty_invalid_recursive_and_non_unicode_values() {
    let config = reference_config("token", "${MCP_PROXY_272_TOKEN}");
    for value in ["", "sentinel\ninvalid", "${sentinel_nested}"] {
        for check in [true, false] {
            rejected(
                &cli(
                    &config,
                    check,
                    &[("MCP_PROXY_272_TOKEN", OsStr::new(value))],
                )
                .await,
                "auth.tokens[0]",
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let value = OsString::from_vec(vec![b's', 0xff]);
        rejected(
            &cli(&config, true, &[("MCP_PROXY_272_TOKEN", &value)]).await,
            "not valid Unicode",
        );
    }
}

#[tokio::test]
async fn resolved_token_collisions_and_header_values_are_validated_and_redacted() {
    let mut config = reference_config("token", "${MCP_PROXY_272_TOKEN}");
    let Some(AuthConfig::Bearer { scoped_tokens, .. }) = &mut config.auth else {
        panic!()
    };
    scoped_tokens.push(mcp_proxy::config::BearerTokenConfig {
        token: "${MCP_PROXY_272_SCOPED}".into(),
        allow_tools: vec!["api/read".into()],
        deny_tools: vec![],
    });
    for check in [true, false] {
        rejected(
            &cli(
                &config,
                check,
                &[
                    ("MCP_PROXY_272_TOKEN", OsStr::new("sentinel-collision")),
                    ("MCP_PROXY_272_SCOPED", OsStr::new("sentinel-collision")),
                ],
            )
            .await,
            "duplicate bearer token",
        );
        let config = reference_config("token", "${MCP_PROXY_272_TOKEN}");
        rejected(
            &cli(
                &config,
                check,
                &[("MCP_PROXY_272_TOKEN", OsStr::new("manager"))],
            )
            .await,
            "security.admin_token",
        );
        let config = reference_config("header", "${MCP_PROXY_272_HEADER}");
        rejected(
            &cli(
                &config,
                check,
                &[("MCP_PROXY_272_HEADER", OsStr::new("sentinel\ninvalid"))],
            )
            .await,
            "invalid value for header",
        );
    }
}

#[tokio::test]
async fn check_resolves_valid_references_and_never_prints_their_values() {
    let config = reference_config("admin", "${MCP_PROXY_272_ADMIN}");
    let output = cli(
        &config,
        true,
        &[("MCP_PROXY_272_ADMIN", OsStr::new("sentinel-resolved-admin"))],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Config OK"));
    assert!(
        !format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .contains("sentinel")
    );
}

#[tokio::test]
async fn management_preserves_references_and_invalid_reload_keeps_active_backends() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

    async fn wait_for_log(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>, pattern: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = rx.recv().await {
                assert!(
                    !line.contains("sentinel"),
                    "logs must not disclose secret values"
                );
                if line.contains(pattern) {
                    return;
                }
            }
            panic!("server log ended before {pattern}");
        })
        .await
        .expect("expected runtime log milestone");
    }

    let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_url = format!("http://{}", backend_listener.local_addr().unwrap());
    let backend = HttpTransport::new(
        McpRouter::new().tool(
            ToolBuilder::new("ping")
                .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("pong")) })
                .build(),
        ),
    )
    .into_router();
    let backend_task =
        tokio::spawn(async move { axum::serve(backend_listener, backend).await.unwrap() });
    let port_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = port_reservation.local_addr().unwrap().port();
    let mut config = ProxyConfig::parse(BASE).unwrap();
    config.proxy.hot_reload = true;
    config.proxy.listen.port = port;
    config.backends[0].transport = mcp_proxy::config::TransportType::Http;
    config.backends[0].url = Some(backend_url);
    let original = toml::to_string(&config).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, &original).unwrap();
    drop(port_reservation);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
        .arg("--config")
        .arg(&path)
        .env("MCP_PROXY_272_ADMIN", "manager")
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let log_task = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    wait_for_log(&mut rx, "Hot reload watching config file").await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.get(format!("{base}/readyz")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let absent = format!("${{MCP_PROXY_MISSING_{}}}", uuid::Uuid::new_v4().simple());
    let mut invalid = ProxyConfig::parse(&original).unwrap();
    invalid.backends[0].name = "replacement".into();
    invalid.backends[0].bearer_token = Some(absent);
    let invalid_body = toml::to_string(&invalid).unwrap();
    for (method, route) in [
        (reqwest::Method::POST, "config/validate"),
        (reqwest::Method::PUT, "config"),
    ] {
        let response = client
            .request(method, format!("{base}/admin/{route}"))
            .bearer_auth("manager")
            .body(invalid_body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("required environment variable is not set"));
        assert!(!text.contains("MCP_PROXY_MISSING"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    // A valid update is checked using resolved values but persists the template.
    config.security.admin_token = Some("${MCP_PROXY_272_ADMIN}".into());
    let template = toml::to_string(&config).unwrap();
    let response = client
        .put(format!("{base}/admin/config"))
        .bearer_auth("manager")
        .body(template.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), template);

    // External edits pass the same validation before removal/addition begins.
    std::fs::write(&path, invalid_body).unwrap();
    wait_for_log(&mut rx, "skipping reload").await;
    let response: serde_json::Value = client
        .get(format!("{base}/admin/backends"))
        .bearer_auth("manager")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        response.to_string().contains("api"),
        "old backend must remain: {response}"
    );
    assert!(!response.to_string().contains("replacement"));
    let mcp = tower_mcp::client::McpClient::connect(tower_mcp::client::HttpClientTransport::new(
        format!("{base}/"),
    ))
    .await
    .unwrap();
    assert_eq!(
        mcp.call_tool_text("api/ping", serde_json::json!({}))
            .await
            .unwrap(),
        "pong",
        "the retained backend must still serve calls, not merely appear in cached health"
    );
    mcp.shutdown().await.unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    log_task.await.unwrap();
    backend_task.abort();
}
