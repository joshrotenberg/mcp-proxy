//! Configured embedding and real executable fixtures with bounded readiness/logs.
use super::http::{Server, client};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct App {
    pub url: String,
    owner: Owner,
}
enum Owner {
    Embedded {
        shutdown: mcp_proxy::ShutdownHandle,
        server: Server,
    },
    Cli {
        child: tokio::process::Child,
        logs: Arc<Mutex<VecDeque<String>>>,
        reader: tokio::task::JoinHandle<()>,
        _dir: tempfile::TempDir,
    },
}
impl App {
    pub async fn embedded(config: &str) -> Self {
        let proxy = tokio::time::timeout(
            Duration::from_secs(10),
            mcp_proxy::Proxy::from_config(mcp_proxy::ProxyConfig::parse(config).unwrap()),
        )
        .await
        .expect("configured construction deadline")
        .unwrap();
        let shutdown = proxy.shutdown_handle();
        let server = Server::start(proxy.into_router().0).await;
        Self {
            url: server.url.clone(),
            owner: Owner::Embedded { shutdown, server },
        }
    }
    pub async fn cli(config: &str, port: u16) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.toml");
        std::fs::write(&path, config).unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"))
            .args(["--config", path.to_str().unwrap()])
            .env("RUST_LOG", "info")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let captured = logs.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let mut logs = captured.lock().unwrap();
                if logs.len() == 100 {
                    logs.pop_front();
                }
                logs.push_back(line);
            }
        });
        let mut app = Self {
            url: format!("http://127.0.0.1:{port}"),
            owner: Owner::Cli {
                child,
                logs,
                reader,
                _dir: dir,
            },
        };
        let http = client();
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Owner::Cli { child, reader, .. } = &mut app.owner
                    && let Some(status) = child.try_wait().unwrap()
                {
                    // Drain stderr before reporting early exit; no scheduling race may hide it.
                    let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;
                    panic!("CLI exited ({status}): {}", app.diagnostics());
                }
                // Only process readiness is polled. Session failures are never retried.
                if let Ok(response) = http.get(format!("{}/readyz", app.url)).send().await {
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    assert_eq!(response.text().await.unwrap(), "ok");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            ready.is_ok(),
            "CLI readiness deadline: {}",
            app.diagnostics()
        );
        app
    }
    pub fn diagnostics(&self) -> String {
        match &self.owner {
            Owner::Embedded { .. } => "embedded configured application".into(),
            Owner::Cli { logs, .. } => logs
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
    pub async fn shutdown(&mut self) {
        match &mut self.owner {
            Owner::Embedded { shutdown, server } => {
                tokio::time::timeout(Duration::from_secs(3), shutdown.shutdown())
                    .await
                    .expect("embedded shutdown deadline")
                    .unwrap();
                server.stop().await;
            }
            Owner::Cli { child, reader, .. } => {
                #[cfg(unix)]
                {
                    let status = tokio::process::Command::new("kill")
                        .args(["-TERM", &child.id().unwrap().to_string()])
                        .status()
                        .await
                        .unwrap();
                    assert!(status.success());
                }
                #[cfg(not(unix))]
                child.start_kill().unwrap();
                let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
                    .await
                    .expect("CLI shutdown deadline")
                    .unwrap();
                #[cfg(unix)]
                assert!(status.success(), "{status}");
                tokio::time::timeout(Duration::from_secs(1), reader)
                    .await
                    .expect("stderr reader deadline")
                    .unwrap();
            }
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("application: {}", self.diagnostics());
        }
        if let Owner::Cli { reader, .. } = &self.owner {
            reader.abort();
        }
    }
}
