//! Real watched-file reloads must leave backend transport drivers runnable.
use std::time::Duration;

use mcp_proxy::{Proxy, ProxyConfig};
use tokio::io::{AsyncBufReadExt, BufReader};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
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
    let app = if kind == "http" {
        HttpTransport::new(router).into_router()
    } else {
        tower_mcp::transport::websocket::WebSocketTransport::new(router).into_router()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let scheme = if kind == "http" { "http" } else { "ws" };
    (
        format!("url = '{scheme}://{addr}/'"),
        Some(Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }))),
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
    let log_task = Server(tokio::spawn(async move {
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
    let mut server = Server(tokio::spawn(async move {
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
