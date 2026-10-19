//! Deterministic local transports share the same echo/blocked tool contract.
use super::http::{Server, WireTrace};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};

pub struct Backend {
    pub config: String,
    dir: tempfile::TempDir,
    wire: WireTrace,
    _server: Option<Server>,
}
impl Backend {
    pub async fn new(kind: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("calls.jsonl");
        std::fs::write(&journal, "").unwrap();
        let wire = WireTrace::new();
        if kind == "stdio" {
            return Self {
                config: format!(
                    "transport='stdio'\ncommand='python3'\nargs=[{:?},{:?}]",
                    concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/fixtures/session_backend.py"
                    ),
                    journal.to_str().unwrap()
                ),
                dir,
                wire,
                _server: None,
            };
        }
        let mut router = McpRouter::new();
        let writes = Arc::new(Mutex::new(()));
        for name in ["echo", "blocked"] {
            let path = journal.clone();
            let writes = writes.clone();
            router = router.tool(
                ToolBuilder::new(name)
                    .handler(move |arguments: Value| {
                        let _guard = writes.lock().unwrap();
                        let mut file = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .unwrap();
                        writeln!(file, "{}", json!({"name":name,"arguments":arguments})).unwrap();
                        async move { Ok(CallToolResult::json(arguments)) }
                    })
                    .build(),
            );
        }
        let app = match kind {
            "http" => {
                // Reject list/call before initialized; a missing notification is a failure.
                let app = HttpTransport::new(router)
                    .session_config(tower_mcp::transport::http::SessionConfig {
                        strict_initialization: true,
                        ..Default::default()
                    })
                    .into_router();
                wire.layer(app)
            }
            #[cfg(feature = "websocket")]
            "websocket" => {
                tower_mcp::transport::websocket::WebSocketTransport::new(router).into_router()
            }
            other => panic!("unsupported fixture transport: {other}"),
        };
        let server = Server::start(app).await;
        let url = if kind == "websocket" {
            server.url.replacen("http://", "ws://", 1)
        } else {
            server.url.clone()
        };
        Self {
            config: format!("transport='{kind}'\nurl='{url}/'"),
            dir,
            wire,
            _server: Some(server),
        }
    }
    pub fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    pub fn assert_http_handshake(&self) {
        let events = self.wire.snapshot();
        if events.is_empty() {
            return;
        }
        let methods: Vec<_> = events
            .iter()
            .filter(|e| e["phase"] == "received")
            .filter_map(|e| e["method"].as_str())
            .filter(|m| *m != "GET/DELETE")
            .collect();
        assert!(
            methods.starts_with(&["initialize", "notifications/initialized"])
                && methods.contains(&"tools/list"),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["method"] == "notifications/initialized" && e["status"] == 202),
            "{events:?}"
        );
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "backend calls: {:?}; HTTP wire: {:?}",
                self.calls(),
                self.wire.snapshot()
            );
        }
    }
}
