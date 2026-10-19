//! Real CLI subscribers must emit enabled MCP request logs without private bodies.
#![cfg(unix)]

use std::convert::Infallible;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tower::ServiceExt;
use tower_mcp::client::{HttpClientTransport, McpClient};
use tower_mcp::protocol::McpRequest;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, RouterRequest, RouterResponse, ToolBuilder,
};

const DATA_TOKEN: &str = "private-data-token-sentinel";
const ADMIN_TOKEN: &str = "private-admin-token-sentinel";
const BACKEND_TOKEN: &str = "private-backend-token-sentinel";
const HEADER: &str = "private-header-sentinel";
const ARGUMENT: &str = "private-argument-sentinel";
const RESULT: &str = "private-result-sentinel";
const ERROR: &str = "private-error-sentinel";

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn serve(app: axum::Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (
        format!("http://{addr}"),
        Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })),
    )
}
fn tool(name: &str) -> tower_mcp::Tool {
    ToolBuilder::new(name)
        .handler(|_: Value| async move { Ok(CallToolResult::text(RESULT)) })
        .build()
}
async fn exercise_cli(json_logs: bool, audit: bool, access: bool, tracing: bool) -> Vec<String> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let backend_seen = seen.clone();
    let router = McpRouter::new()
        .tool(tool("work"))
        .tool(tool("blocked"))
        .tool(tool("fail"));
    let service = tower::service_fn(move |req: RouterRequest| {
        let inner = router.clone();
        let seen = backend_seen.clone();
        async move {
            if let McpRequest::CallTool(params) = &req.inner {
                seen.lock().unwrap().push(params.name.clone());
                assert_eq!(params.arguments["secret"], ARGUMENT);
                if params.name == "fail" {
                    return Ok::<_, Infallible>(RouterResponse {
                        id: req.id,
                        inner: Err(tower_mcp_types::JsonRpcError::internal_error(format!(
                            "{ERROR}: {ARGUMENT}"
                        ))),
                    });
                }
            }
            inner.oneshot(req).await
        }
    });
    let app = HttpTransport::from_service(service)
        .into_router()
        .layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                assert_eq!(
                    request.headers()["authorization"],
                    format!("Bearer {BACKEND_TOKEN}")
                );
                assert_eq!(request.headers()["x-api-key"], HEADER);
                next.run(request).await
            },
        ));
    let (backend, _backend) = serve(app).await;
    let (collector, _collector) =
        serve(axum::Router::new().fallback(|| async { axum::Json(json!({})) })).await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    let tracing_config = if tracing {
        format!(
            "[observability.tracing]\nenabled=true\nendpoint='{collector}'\nservice_name='logging-test'\n"
        )
    } else {
        String::new()
    };
    std::fs::write(
        &path,
        format!(
            r#"
[proxy]
name = "logging-test"
[proxy.listen]
host = "127.0.0.1"
port = {port}
[auth]
type = "bearer"
tokens = ["{DATA_TOKEN}"]
[security]
admin_token = "{ADMIN_TOKEN}"
[observability]
audit = {audit}
json_logs = {json_logs}
log_level = "{}"
[observability.access_log]
enabled = {access}
{tracing_config}
[[backends]]
name = "api"
transport = "http"
url = "{backend}/"
bearer_token = "{BACKEND_TOKEN}"
hide_tools = ["blocked"]
[backends.headers]
X-API-Key = "{HEADER}"
"#,
            if audit && access { "warn" } else { "debug" }
        ),
    )
    .unwrap();
    drop(reservation);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
        .arg("--config")
        .arg(&path)
        // The CLI is configured by observability, not an inherited RUST_LOG.
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let collected = Arc::new(Mutex::new(Vec::new()));
    let lines = collected.clone();
    let reader = tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr).lines();
        while let Some(line) = stderr.next_line().await.unwrap() {
            lines.lock().unwrap().push(line);
        }
    });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "CLI exited before readiness"
            );
            if let Ok(response) = http.get(format!("{base}/livez")).send().await
                && response.status().is_success()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("CLI never became ready");
    let client =
        McpClient::connect(HttpClientTransport::new(format!("{base}/")).bearer_token(DATA_TOKEN))
            .await
            .unwrap();
    client.initialize("logging-regression", "1").await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert!(!tools.tools.iter().any(|tool| tool.name == "api/blocked"));
    assert_eq!(
        client
            .call_tool_text("api/work", json!({"secret":ARGUMENT}))
            .await
            .unwrap(),
        RESULT
    );
    let denied = client
        .call_tool_text("api/blocked", json!({"secret":ARGUMENT}))
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("-32602"), "{denied}");
    let failed = client
        .call_tool_text("api/fail", json!({"secret":ARGUMENT}))
        .await
        .unwrap_err();
    // Redact only diagnostics, preserving the original client-visible failure.
    assert!(failed.to_string().contains(ERROR));
    assert!(failed.to_string().contains(ARGUMENT));
    assert_eq!(*seen.lock().unwrap(), ["work", "fail"]);
    client.shutdown().await.unwrap();

    assert!(
        tokio::process::Command::new("kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(2), reader)
        .await
        .unwrap()
        .unwrap();
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .await
        .unwrap();
    assert!(stdout.is_empty(), "request logs must stay on stderr");
    let logs = collected.lock().unwrap().clone();
    let text = logs.join("\n");
    for secret in [
        DATA_TOKEN,
        ADMIN_TOKEN,
        BACKEND_TOKEN,
        HEADER,
        ARGUMENT,
        RESULT,
        ERROR,
    ] {
        assert!(!text.contains(secret), "private value appeared in CLI logs");
    }
    logs
}

fn assert_json_records(lines: &[String], audit: bool, access: bool) {
    let records: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("stderr record is not JSON"))
        .collect();
    for (target, enabled) in [("mcp::audit", audit), ("mcp::access", access)] {
        let records: Vec<_> = records
            .iter()
            .filter(|record| {
                record["target"] == target && record["fields"]["method"] == "tools/call"
            })
            .collect();
        assert_eq!(
            records.len(),
            if enabled { 3 } else { 0 },
            "{target} tools/call count"
        );
        for record in &records {
            assert_eq!(record["level"], "INFO");
            assert!(record["fields"]["duration_ms"].is_number());
            assert!(record["fields"].get("error_message").is_none());
            assert!(record["fields"].get("arguments").is_none());
        }
        if !enabled {
            continue;
        }
        let target_field = if target == "mcp::audit" {
            "tool"
        } else {
            "target"
        };
        for (name, access_status, audit_status, code) in [
            ("api/work", "ok", "success", None),
            ("api/blocked", "error", "denied", Some(-32602)),
            ("api/fail", "error", "error", Some(-32603)),
        ] {
            let record = records
                .iter()
                .find(|record| record["fields"][target_field] == name)
                .unwrap();
            let fields = &record["fields"];
            if target == "mcp::audit" {
                assert_eq!(fields["status"], audit_status);
                assert_eq!(fields["error_code"].as_i64(), code);
                assert!(
                    fields["request_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
                );
            } else {
                assert_eq!(fields["backend"], "api");
                assert_eq!(fields["status"], access_status);
            }
        }
    }
}

#[tokio::test]
async fn cli_json_emits_success_denial_and_error_without_private_bodies() {
    assert_json_records(&exercise_cli(true, true, true, false).await, true, true);
}

#[tokio::test]
async fn cli_json_request_log_flags_are_independent() {
    for (audit, access) in [(true, false), (false, true), (false, false)] {
        assert_json_records(
            &exercise_cli(true, audit, access, false).await,
            audit,
            access,
        );
    }
}

#[tokio::test]
async fn cli_text_emits_enabled_request_targets_at_info() {
    let lines = exercise_cli(false, true, true, false).await;
    for target in ["mcp::audit", "mcp::access"] {
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains(target) && line.contains("tools/call"))
                .count(),
            3
        );
    }
}

#[cfg(feature = "otel")]
#[tokio::test]
async fn cli_otel_json_emits_the_same_request_records() {
    assert_json_records(&exercise_cli(true, true, true, true).await, true, true);
}
