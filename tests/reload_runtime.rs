//! Real watched-file reloads must leave backend transport drivers runnable.
#[path = "support/http.rs"]
mod http;
use http::WireTrace;
use std::time::Duration;

use mcp_proxy::{Proxy, ProxyConfig};
use tokio::io::{AsyncBufReadExt, BufReader};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

struct Server(tokio::task::JoinHandle<()>, Option<WireTrace>);
impl Server {
    fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self(task, None)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(trace) = &self.1
        {
            eprintln!("reload backend wire: {:?}", trace.snapshot());
        }
        self.0.abort();
    }
}
struct Process {
    child: tokio::process::Child,
    logs: tokio::sync::mpsc::UnboundedReceiver<String>,
    _log_task: Server,
}
impl Process {
    async fn wait(&mut self, expected: &str) {
        let mut recent = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(line) = self.logs.recv().await {
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
}
async fn backend(kind: &str, revision: &str) -> (String, Option<Server>) {
    if kind == "stdio" {
        return (
            format!(
                "command = 'python3'\nargs = [{:?}, {:?}]",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/reload_backend.py"
                ),
                revision
            ),
            None,
        );
    }
    let revision = revision.to_string();
    let router = McpRouter::new().tool(
        ToolBuilder::new("ping")
            .handler(move |_: serde_json::Value| {
                let revision = revision.clone();
                async move { Ok(CallToolResult::text(revision)) }
            })
            .build(),
    );
    let trace = WireTrace::new();
    let app = if kind == "http" {
        trace.layer(
            HttpTransport::new(router)
                .session_config(tower_mcp::transport::http::SessionConfig {
                    strict_initialization: true,
                    ..Default::default()
                })
                .into_router(),
        )
    } else {
        tower_mcp::transport::websocket::WebSocketTransport::new(router).into_router()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let scheme = if kind == "http" { "http" } else { "ws" };
    (
        format!("url = '{scheme}://{addr}/'"),
        Some(Server(
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
            Some(trace),
        )),
    )
}
fn config(kind: &str, original: &str, dynamic: Option<&str>, port: u16) -> String {
    let mut config = format!(
        r#"
[proxy]
name = "reload-runtime"
hot_reload = true
[proxy.listen]
host = "127.0.0.1"
port = {port}
[[backends]]
name = "original"
transport = "{kind}"
{original}
"#
    );
    if let Some(dynamic) = dynamic {
        config += &format!("\n[[backends]]\nname = 'dynamic'\ntransport = '{kind}'\n{dynamic}\n");
    }
    config
}
async fn call(client: &tower_mcp::client::McpClient, name: &str, expected: &str) {
    let value = tokio::time::timeout(
        Duration::from_secs(3),
        client.call_tool_text(name, serde_json::json!({})),
    )
    .await
    .unwrap_or_else(|_| panic!("{name} stopped progressing while watcher was idle"))
    .unwrap();
    assert_eq!(value, expected);
}
async fn watched_reload(kind: &str) {
    let (original, _original_server) = backend(kind, "original").await;
    let (added, _added_server) = backend(kind, "added").await;
    let (replacement, _replacement_server) = backend(kind, "replacement").await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, config(kind, &original, None, port)).unwrap();
    drop(reservation);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
        .args(["--config", path.to_str().unwrap()])
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let (tx, logs) = tokio::sync::mpsc::unbounded_channel();
    let log_task = Server::new(tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    }));
    let mut process = Process {
        child,
        logs,
        _log_task: log_task,
    };
    process.wait("Hot reload watching config file").await;
    let url = format!("http://127.0.0.1:{port}/");
    let client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = tower_mcp::client::McpClient::connect(
                tower_mcp::client::HttpClientTransport::new(&url),
            )
            .await
            {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    call(&client, "original/ping", "original").await;
    std::fs::write(&path, config(kind, &original, Some(&added), port)).unwrap();
    process.wait("Backend added via hot reload").await;
    // Discovery is disabled; no await follows this log before the watcher waits.
    tokio::time::sleep(Duration::from_millis(100)).await;
    call(&client, "original/ping", "original").await;
    call(&client, "dynamic/ping", "added").await;
    std::fs::write(&path, config(kind, &original, Some(&replacement), port)).unwrap();
    process.wait("Backend replaced").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    call(&client, "original/ping", "original").await;
    call(&client, "dynamic/ping", "replacement").await;
    client.shutdown().await.unwrap();
    #[cfg(unix)]
    {
        let status = tokio::process::Command::new("kill")
            .args(["-TERM", &process.child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        process.wait("Config watcher stopped").await;
        assert!(process.child.wait().await.unwrap().success());
    }
    #[cfg(not(unix))]
    {
        process.child.kill().await.unwrap();
        process.child.wait().await.unwrap();
    }
}

#[tokio::test]
async fn watched_stdio_addition_and_replacement_keep_serving() {
    watched_reload("stdio").await;
}
#[tokio::test]
async fn watched_http_addition_and_replacement_keep_serving() {
    watched_reload("http").await;
}
#[cfg(feature = "websocket")]
#[tokio::test]
async fn watched_websocket_addition_and_replacement_keep_serving() {
    watched_reload("websocket").await;
}

#[derive(Clone)]
struct LogWriter(tokio::sync::mpsc::UnboundedSender<String>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(String::from_utf8_lossy(bytes).into_owned());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
async fn wait_log(logs: &mut tokio::sync::mpsc::UnboundedReceiver<String>, pattern: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = logs.recv().await {
            if line.contains(pattern) {
                return;
            }
        }
        panic!("watcher log ended before {pattern}");
    })
    .await
    .unwrap_or_else(|_| panic!("watcher never reached {pattern}"));
}

#[tokio::test]
async fn embedded_router_owns_one_watcher_until_its_last_clone_drops() {
    let (tx, mut logs) = tokio::sync::mpsc::unbounded_channel();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(LogWriter(tx))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (original, _original_server) = backend("http", "original").await;
    let (added, _added_server) = backend("http", "added").await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, config("http", &original, None, 0)).unwrap();
    let proxy = Proxy::from_config(ProxyConfig::load(&path).unwrap())
        .await
        .unwrap();
    proxy.enable_hot_reload(path.clone());
    // A second call must not create another writer (or switch watched paths).
    proxy.enable_hot_reload(temp.path().join("not-a-config"));
    let (router, _) = proxy.into_router();
    let retained = router.clone();
    drop(router);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let mut server = Server::new(tokio::spawn(async move {
        axum::serve(listener, retained).await.unwrap();
    }));
    wait_log(&mut logs, "Hot reload watching config file").await;
    let client =
        tower_mcp::client::McpClient::connect(tower_mcp::client::HttpClientTransport::new(&url))
            .await
            .unwrap();
    call(&client, "original/ping", "original").await;
    std::fs::write(&path, config("http", &original, Some(&added), 0)).unwrap();
    wait_log(&mut logs, "Backend added via hot reload").await;
    call(&client, "dynamic/ping", "added").await;
    client.shutdown().await.unwrap();
    server.0.abort();
    // Await task destruction so its last Router/Extension owner is dropped.
    let _ = (&mut server.0).await;
    wait_log(&mut logs, "Config watcher stopped").await;
}

#[tokio::test]
async fn owned_watcher_shutdown_joins_idle_worker_and_proxy_drop_cancels() {
    let (tx, mut logs) = tokio::sync::mpsc::unbounded_channel();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(LogWriter(tx))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (original, _original_server) = backend("http", "original").await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.toml");
    std::fs::write(&path, config("http", &original, None, 0)).unwrap();
    let proxy = Proxy::from_config(ProxyConfig::load(&path).unwrap())
        .await
        .unwrap();
    let watcher = mcp_proxy::reload::spawn_config_watcher(
        path.clone(),
        proxy.mcp_proxy().clone(),
        #[cfg(feature = "discovery")]
        None,
    );
    wait_log(&mut logs, "Hot reload watching config file").await;
    tokio::time::timeout(Duration::from_secs(3), watcher.shutdown())
        .await
        .unwrap()
        .unwrap();
    wait_log(&mut logs, "Config watcher stopped").await;
    proxy.enable_hot_reload(path);
    wait_log(&mut logs, "Hot reload watching config file").await;
    drop(proxy);
    wait_log(&mut logs, "Config watcher stopped").await;
}

struct PolicyFixture {
    _temp: tempfile::TempDir,
    path: std::path::PathBuf,
    base: String,
    _server: Server,
    original: String,
}
fn serialize(config: &ProxyConfig) -> String {
    toml::to_string(config).unwrap()
}
impl PolicyFixture {
    async fn new(body: String, watch: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("proxy.toml");
        let mut config = ProxyConfig::parse(&body).unwrap();
        config.security.admin_token = Some("sentinel-manager".into());
        config.auth = Some(mcp_proxy::config::AuthConfig::Bearer {
            tokens: vec!["sentinel-data".into()],
            scoped_tokens: vec![],
        });
        let original = serialize(&config);
        std::fs::write(&path, &original).unwrap();
        let proxy = Proxy::from_config(ProxyConfig::load(&path).unwrap())
            .await
            .unwrap();
        if watch {
            proxy.enable_hot_reload(path.clone());
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = proxy.into_router().0;
        let server = Server::new(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let fixture = Self {
            _temp: temp,
            path,
            base,
            _server: server,
            original,
        };
        if watch {
            fixture.wait_status("applied").await;
        }
        fixture
    }
    async fn status(&self) -> serde_json::Value {
        let response = reqwest::Client::new()
            .get(format!("{}/admin/config/status", self.base))
            .bearer_auth("sentinel-manager")
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let value: serde_json::Value = response.json().await.unwrap();
        assert!(!value.to_string().contains("sentinel"));
        value
    }
    async fn wait_status(&self, expected: &str) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let value = self.status().await;
                if value["last_result"] == expected {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("reload never reached {expected}"))
    }
    async fn client(&self) -> tower_mcp::client::McpClient {
        tower_mcp::client::McpClient::connect(
            tower_mcp::client::HttpClientTransport::new(format!("{}/", self.base))
                .bearer_token("sentinel-data"),
        )
        .await
        .unwrap()
    }
    async fn request(
        &self,
        method: reqwest::Method,
        route: &str,
        body: String,
    ) -> reqwest::Response {
        reqwest::Client::new()
            .request(method, format!("{}/admin/{route}", self.base))
            .bearer_auth("sentinel-manager")
            .body(body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn watched_policy_and_auth_edits_are_rejected_before_any_transport_mutation() {
    use serde_json::json;
    let (original, _old) = backend("http", "original").await;
    let (replacement, _new) = backend("http", "replacement").await;
    let fixture = PolicyFixture::new(config("http", &original, None, 0), true).await;
    let client = fixture.client().await;
    call(&client, "original/ping", "original").await;
    let unauthorized = reqwest::Client::new()
        .get(format!("{}/admin/config/status", fixture.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let ordinary = reqwest::Client::new()
        .get(format!("{}/admin/config/status", fixture.base))
        .bearer_auth("sentinel-data")
        .send()
        .await
        .unwrap();
    assert_eq!(ordinary.status(), reqwest::StatusCode::UNAUTHORIZED);

    for auth_edit in [false, true] {
        let mut desired = ProxyConfig::parse(&fixture.original).unwrap();
        let new: mcp_proxy::config::BackendConfig =
            toml::from_str(&format!("name='x'\ntransport='http'\n{replacement}")).unwrap();
        desired.backends[0].url = new.url;
        if auth_edit {
            desired.auth = Some(mcp_proxy::config::AuthConfig::Bearer {
                tokens: vec!["sentinel-changed".into()],
                scoped_tokens: vec![],
            });
        } else {
            desired.backends[0].hide_tools = vec!["ping".into()];
        }
        let proposed = serialize(&desired);
        std::fs::write(&fixture.path, &proposed).unwrap();
        let status = fixture.wait_status("restart_required").await;
        assert_eq!(status["applied"], false);
        assert_eq!(status["restart_required"], true);
        call(&client, "original/ping", "original").await;
        for route in ["config/validate", "config"] {
            let method = if route == "config" {
                reqwest::Method::PUT
            } else {
                reqwest::Method::POST
            };
            let response = fixture.request(method, route, proposed.clone()).await;
            assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
            let value: serde_json::Value = response.json().await.unwrap();
            assert_eq!(value["applied"], false);
            assert_eq!(value["restart_required"], true);
            assert!(!value.to_string().contains("sentinel"));
            assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), proposed);
        }
        std::fs::write(&fixture.path, &fixture.original).unwrap();
        fixture.wait_status("applied").await;
    }
    // A connection-only change is supported, saved as pending, then observed applied.
    let mut desired = ProxyConfig::parse(&fixture.original).unwrap();
    desired.backends[0].url = toml::from_str::<mcp_proxy::config::BackendConfig>(&format!(
        "name='x'\ntransport='http'\n{replacement}"
    ))
    .unwrap()
    .url;
    let response = fixture
        .request(reqwest::Method::PUT, "config", serialize(&desired))
        .await;
    assert!(response.status().is_success());
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["applied"], false);
    assert_eq!(value["restart_required"], false);
    fixture.wait_status("applied").await;
    // The old applied status cannot satisfy the first subsequent call indefinitely.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .call_tool_text("original/ping", json!({}))
                .await
                .is_ok_and(|text| text == "replacement")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn saved_config_without_a_watcher_explicitly_requires_restart() {
    let (original, _old) = backend("http", "original").await;
    let fixture = PolicyFixture::new(config("http", &original, None, 0), false).await;
    let mut desired = ProxyConfig::parse(&fixture.original).unwrap();
    desired.backends[0]
        .headers
        .insert("X-Changed".into(), "yes".into());
    let proposed = serialize(&desired);
    let response = fixture
        .request(reqwest::Method::PUT, "config", proposed.clone())
        .await;
    assert!(response.status().is_success());
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["applied"], false);
    assert_eq!(value["restart_required"], true);
    assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), proposed);
    let status = fixture.status().await;
    assert_eq!(status["watcher_active"], false);
    assert_eq!(status["last_result"], "restart_required");
    let client = fixture.client().await;
    call(&client, "original/ping", "original").await;
    client.shutdown().await.unwrap();
}

async fn faulty_backend(
    mode: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> (String, Server) {
    use tower::ServiceExt;
    let router = McpRouter::new().tool(
        ToolBuilder::new("ping")
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("healthy")) })
            .build(),
    );
    let service = tower::service_fn(move |request: tower_mcp::RouterRequest| {
        let router = router.clone();
        let mode = mode.clone();
        let calls = calls.clone();
        async move {
            if matches!(request.inner, tower_mcp::protocol::McpRequest::CallTool(_)) {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match mode.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => tokio::time::sleep(Duration::from_secs(2)).await,
                    1 => {
                        return Ok(tower_mcp::RouterResponse {
                            id: request.id,
                            inner: Err(tower_mcp_types::JsonRpcError::internal_error(
                                "fixture failure",
                            )),
                        });
                    }
                    _ => {}
                }
            }
            router.oneshot(request).await
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fields = format!("url = 'http://{}/'", listener.local_addr().unwrap());
    (
        fields,
        Server::new(tokio::spawn(async move {
            axum::serve(listener, HttpTransport::from_service(service).into_router())
                .await
                .unwrap();
        })),
    )
}

#[tokio::test]
async fn reload_preserves_timeout_breaker_state_and_guards_direct_management_mutation() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mode = Arc::new(AtomicUsize::new(2));
    let calls = Arc::new(AtomicUsize::new(0));
    let (fields, _server) = faulty_backend(mode.clone(), calls.clone()).await;
    let body = config("http", &fields, None, 0)
        + "\n[backends.timeout]\nseconds=1\n[backends.circuit_breaker]\nminimum_calls=1\nwait_duration_seconds=30\n";
    let fixture = PolicyFixture::new(body, true).await;
    let client = fixture.client().await;
    mode.store(0, Ordering::SeqCst);
    assert!(
        client
            .call_tool_text("original/ping", serde_json::json!({}))
            .await
            .is_err()
    );
    let breakers: serde_json::Value = fixture
        .request(reqwest::Method::GET, "circuit-breakers", String::new())
        .await
        .json()
        .await
        .unwrap();
    assert!(
        breakers[0]["state"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("open")
    );
    let manager = tower_mcp::client::McpClient::connect(
        tower_mcp::client::HttpClientTransport::new(format!("{}/", fixture.base))
            .bearer_token("sentinel-manager"),
    )
    .await
    .unwrap();
    let denied = manager
        .call_tool(
            "proxy/add_backend",
            serde_json::json!({"name":"original","url":"http://127.0.0.1:1"}),
        )
        .await
        .unwrap();
    assert!(denied.all_text().contains("Restart required"));
    manager.shutdown().await.unwrap();
    for method in [reqwest::Method::DELETE, reqwest::Method::PUT] {
        let response = reqwest::Client::new()
            .request(method, format!("{}/admin/backends/original", fixture.base))
            .bearer_auth("sentinel-manager")
            .json(&serde_json::json!({"url":"http://127.0.0.1:1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    }
    let (plain, _plain_server) = backend("http", "added").await;
    let mut desired = ProxyConfig::parse(&fixture.original).unwrap();
    desired
        .backends
        .push(toml::from_str(&format!("name='dynamic'\ntransport='http'\n{plain}")).unwrap());
    std::fs::write(&fixture.path, serialize(&desired)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .call_tool_text("dynamic/ping", serde_json::json!({}))
                .await
                .is_ok_and(|s| s == "added")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    mode.store(2, Ordering::SeqCst);
    assert!(
        client
            .call_tool_text("original/ping", serde_json::json!({}))
            .await
            .is_err()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "active breaker must remain attached"
    );
    let mut rejected = ProxyConfig::parse(&serialize(&desired)).unwrap();
    rejected.backends[0].timeout.as_mut().unwrap().seconds = 2;
    std::fs::write(&fixture.path, serialize(&rejected)).unwrap();
    fixture.wait_status("restart_required").await;
    assert!(
        client
            .call_tool_text("original/ping", serde_json::json!({}))
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let current: serde_json::Value = fixture
        .request(reqwest::Method::GET, "circuit-breakers", String::new())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        current, breakers,
        "management handles must still describe the active breaker"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn reload_preserves_shared_fleet_ejection_limits() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let one_mode = Arc::new(AtomicUsize::new(2));
    let two_mode = Arc::new(AtomicUsize::new(2));
    let one_calls = Arc::new(AtomicUsize::new(0));
    let two_calls = Arc::new(AtomicUsize::new(0));
    let (one, _one_server) = faulty_backend(one_mode.clone(), one_calls.clone()).await;
    let (two, _two_server) = faulty_backend(two_mode.clone(), two_calls.clone()).await;
    let body = config("http", &one, Some(&two), 0);
    let mut configured = ProxyConfig::parse(&body).unwrap();
    for backend in &mut configured.backends {
        backend.outlier_detection = Some(
            toml::from_str(
                "consecutive_errors=1\nbase_ejection_seconds=30\nmax_ejection_percent=50",
            )
            .unwrap(),
        );
    }
    let fixture = PolicyFixture::new(serialize(&configured), true).await;
    let client = fixture.client().await;
    one_mode.store(1, Ordering::SeqCst);
    two_mode.store(1, Ordering::SeqCst);
    for name in [
        "original/ping",
        "original/ping",
        "dynamic/ping",
        "dynamic/ping",
    ] {
        assert!(
            client
                .call_tool_text(name, serde_json::json!({}))
                .await
                .is_err()
        );
    }
    assert_eq!(
        one_calls.load(Ordering::SeqCst),
        1,
        "first backend should be ejected"
    );
    assert_eq!(
        two_calls.load(Ordering::SeqCst),
        2,
        "50% fleet cap must keep second backend unejected"
    );
    let (plain, _plain_server) = backend("http", "plain").await;
    let mut desired = ProxyConfig::parse(&fixture.original).unwrap();
    desired
        .backends
        .push(toml::from_str(&format!("name='plain'\ntransport='http'\n{plain}")).unwrap());
    std::fs::write(&fixture.path, serialize(&desired)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if client
                .call_tool_text("plain/ping", serde_json::json!({}))
                .await
                .is_ok_and(|s| s == "plain")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    desired.backends[1]
        .outlier_detection
        .as_mut()
        .unwrap()
        .max_ejection_percent = 100;
    std::fs::write(&fixture.path, serialize(&desired)).unwrap();
    fixture.wait_status("restart_required").await;
    for name in ["original/ping", "dynamic/ping"] {
        assert!(
            client
                .call_tool_text(name, serde_json::json!({}))
                .await
                .is_err()
        );
    }
    assert_eq!(one_calls.load(Ordering::SeqCst), 1);
    assert_eq!(two_calls.load(Ordering::SeqCst), 3);
    client.shutdown().await.unwrap();
}
