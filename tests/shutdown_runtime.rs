//! Configured process and embedding shutdown, including hostile EOF behavior.
#![cfg(unix)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mcp_proxy::{Proxy, ProxyConfig};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tower::ServiceExt;
use tower_mcp::client::{HttpClientTransport, McpClient};

struct Fixture {
    _dir: tempfile::TempDir,
    pid: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("backend.pid");
        Self { _dir: dir, pid }
    }
    fn backend(&self, mode: &str) -> String {
        format!(
            "transport='stdio'\ncommand='python3'\nargs=[{:?},{:?},{mode:?}]",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/shutdown_backend.py"
            ),
            self.pid.to_str().unwrap()
        )
    }
    fn config(&self, port: u16, mode: &str) -> String {
        format!(
            "[proxy]\nname='shutdown-test'\nshutdown_timeout_seconds=1\n[proxy.listen]\nhost='127.0.0.1'\nport={port}\n[[backends]]\nname='fixture'\n{}",
            self.backend(mode)
        )
    }
    async fn gone(&self) {
        let pid: u32 = std::fs::read_to_string(&self.pid).unwrap().parse().unwrap();
        until(
            || !process_exists(&pid.to_string()),
            "child was not terminated and reaped",
        )
        .await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Failed assertions must not leave the intentionally stubborn fixture.
        if let Ok(pid) = std::fs::read_to_string(&self.pid)
            && let Ok(command) = std::process::Command::new("ps")
                .args(["-p", pid.trim(), "-o", "command="])
                .output()
            && command.status.success()
            && String::from_utf8_lossy(&command.stdout).contains(self.pid.to_str().unwrap())
        {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
    }
}
fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("Unix kill command required")
        .success()
}
async fn until(mut condition: impl FnMut() -> bool, message: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{message}"));
}
struct Process {
    child: tokio::process::Child,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    logger: tokio::task::JoinHandle<()>,
}
impl Drop for Process {
    fn drop(&mut self) {
        self.logger.abort();
    }
}
impl Process {
    async fn wait(&mut self, expected: &str) {
        let mut recent = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = self.lines.recv().await {
                if line.contains(expected) {
                    return;
                }
                recent.push(line);
            }
            panic!("CLI exited before {expected}: {}", recent.join("\n"));
        })
        .await
        .unwrap_or_else(|_| panic!("CLI did not reach {expected}: {}", recent.join("\n")));
    }
    async fn term(&mut self) {
        assert!(
            tokio::process::Command::new("kill")
                .args(["-TERM", &self.child.id().unwrap().to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );
    }
}
async fn cli(fixture: &Fixture) -> (Process, McpClient, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let path = fixture._dir.path().join("proxy.toml");
    std::fs::write(&path, fixture.config(port, "ready")).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
        .args(["--config", path.to_str().unwrap()])
        .kill_on_drop(true)
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let (tx, logs) = tokio::sync::mpsc::unbounded_channel();
    let logger = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut process = Process {
        child,
        lines: logs,
        logger,
    };
    process.wait("Proxy ready").await;
    let base = format!("http://127.0.0.1:{port}/");
    let client = McpClient::connect(HttpClientTransport::new(&base))
        .await
        .unwrap();
    client.initialize("shutdown-test", "1").await.unwrap();
    client.list_tools().await.unwrap();
    (process, client, base)
}

#[tokio::test]
async fn sigterm_bounds_pending_tool_and_open_sse_and_reaps_child() {
    let fixture = Fixture::new();
    let (mut process, client, base) = cli(&fixture).await;
    // A real initialized session's GET stream stays open throughout draining.
    let http = reqwest::Client::new();
    let init: serde_json::Value = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"sse-test","version":"1"}}});
    let initialized = http
        .post(&base)
        .header("accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        .unwrap();
    let session = initialized
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    initialized.bytes().await.unwrap();
    assert!(
        http.post(&base)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let stream = http
        .get(&base)
        .header("accept", "text/event-stream")
        .header("mcp-session-id", &session)
        .send()
        .await
        .unwrap();
    assert!(stream.status().is_success());
    let pending =
        tokio::spawn(async move { client.call_tool("fixture/work", json!({"wait":true})).await });
    until(
        || fixture.pid.with_extension("pid.called").exists(),
        "tool never started",
    )
    .await;
    let start = Instant::now();
    process.term().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), process.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "drain timeout ignored: {:?}",
        start.elapsed()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fixture.gone().await;
    drop(stream);
}

#[tokio::test]
async fn sigterm_allows_a_short_request_to_finish_before_cleanup() {
    let fixture = Fixture::new();
    let (mut process, client, _) = cli(&fixture).await;
    let pending =
        tokio::spawn(async move { client.call_tool("fixture/work", json!({"wait":true})).await });
    until(
        || fixture.pid.with_extension("pid.called").exists(),
        "tool never started",
    )
    .await;
    process.term().await;
    process.wait("Shutdown signal received").await;
    std::fs::write(fixture.pid.with_extension("pid.release"), "go").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.all_text(), "completed");
    assert!(!result.is_error);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), process.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    fixture.gone().await;
}

#[tokio::test]
async fn repeated_proxy_and_embedded_router_drop_stop_their_children() {
    for embed in [false, true, false, true] {
        let fixture = Fixture::new();
        let proxy = Proxy::from_config(ProxyConfig::parse(&fixture.config(0, "ready")).unwrap())
            .await
            .unwrap();
        if embed {
            let (router, _) = proxy.into_router();
            let clone = router.clone();
            drop(router);
            assert!(process_exists(
                std::fs::read_to_string(&fixture.pid).unwrap().trim()
            ));
            drop(clone);
        } else {
            drop(proxy);
        }
        fixture.gone().await;
    }
}

#[tokio::test]
async fn explicit_embedded_shutdown_is_idempotent_closes_sessions_and_rejects_new_work() {
    let fixture = Fixture::new();
    let proxy = Proxy::from_config(ProxyConfig::parse(&fixture.config(0, "ready")).unwrap())
        .await
        .unwrap();
    let shutdown = proxy.shutdown_handle();
    let (router, sessions) = proxy.into_router();
    let request = axum::http::Request::builder().method("POST").uri("/")
        .header("content-type", "application/json").header("accept", "application/json, text/event-stream")
        .body(axum::body::Body::from(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"embed","version":"1"}}}).to_string())).unwrap();
    assert!(
        router
            .clone()
            .oneshot(request)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(sessions.session_count().await, 1);
    let clone = shutdown.clone();
    let (a, b) = tokio::join!(shutdown.shutdown(), clone.shutdown());
    a.unwrap();
    b.unwrap();
    assert_eq!(sessions.session_count().await, 0);
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/livez")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    fixture.gone().await;
}

#[tokio::test]
async fn cancelled_construction_reaps_child_during_initialize() {
    let fixture = Fixture::new();
    let config = ProxyConfig::parse(&fixture.config(0, "hang-init")).unwrap();
    let construction = tokio::spawn(Proxy::from_config(config));
    until(
        || fixture.pid.with_extension("pid.initialize").exists(),
        "initialize never started",
    )
    .await;
    construction.abort();
    assert!(matches!(construction.await, Err(error) if error.is_cancelled()));
    fixture.gone().await;
}

#[tokio::test]
async fn bind_failure_waits_for_owned_worker_and_child_cleanup() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fixture = Fixture::new();
    let proxy = Proxy::from_config(ProxyConfig::parse(&fixture.config(port, "ready")).unwrap())
        .await
        .unwrap();
    assert!(proxy.serve().await.is_err());
    let pid = std::fs::read_to_string(&fixture.pid).unwrap();
    assert!(
        !process_exists(pid.trim()),
        "bind error returned before child reap"
    );
    fixture.gone().await;
}

#[tokio::test]
async fn failed_construction_after_connecting_a_backend_reaps_child() {
    let fixture = Fixture::new();
    // Invalid runtime regex deliberately bypasses parse validation through the
    // public config type; construction must still clean up its connected child.
    let mut config = ProxyConfig::parse(&fixture.config(0, "ready")).unwrap();
    config.backends[0].expose_tools = vec!["re:[".into()];
    assert!(Proxy::from_config(config).await.is_err());
    assert!(fixture.pid.with_extension("pid.initialize").exists());
    let pid = std::fs::read_to_string(&fixture.pid).unwrap();
    assert!(
        !process_exists(pid.trim()),
        "error returned before child reap"
    );
    fixture.gone().await;
}

#[tokio::test]
async fn shutdown_joins_watcher_and_reaps_reloaded_children_even_during_initialize() {
    for mode in ["ready", "hang-init"] {
        let original = Fixture::new();
        let dynamic = Fixture::new();
        let path = original._dir.path().join("reload.toml");
        let config = original.config(0, "ready");
        std::fs::write(&path, &config).unwrap();
        let proxy = Proxy::from_config(ProxyConfig::load(&path).unwrap())
            .await
            .unwrap();
        let backends = proxy.mcp_proxy().clone();
        let shutdown = proxy.shutdown_handle();
        proxy.enable_hot_reload(path.clone());
        // A repeated attempt must retain the first watcher and its runtime.
        proxy.enable_hot_reload(original._dir.path().join("does-not-exist.toml"));
        std::fs::write(
            &path,
            format!(
                "{config}\n[[backends]]\nname='dynamic'\n{}",
                dynamic.backend(mode)
            ),
        )
        .unwrap();
        until(
            || dynamic.pid.with_extension("pid.initialize").exists(),
            "reload never initialized its child",
        )
        .await;
        if mode == "ready" {
            until(
                || {
                    backends
                        .backend_namespaces()
                        .iter()
                        .any(|name| name == "dynamic")
                },
                "reload never admitted its child",
            )
            .await;
        }
        tokio::time::timeout(Duration::from_secs(2), proxy.shutdown())
            .await
            .unwrap()
            .unwrap();
        shutdown.shutdown().await.unwrap();
        assert!(backends.backend_namespaces().is_empty());
        original.gone().await;
        dynamic.gone().await;
    }
}
