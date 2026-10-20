//! Real NGINX IP affinity in front of two actual configured CLI processes.
//! Requires Linux Docker/nginx:1.28-alpine and coreutils timeout in every feature
//! mode; no startup skips. Docker CLI commands are bounded, including Drop cleanup.
#[path = "support/app.rs"]
#[allow(dead_code)]
mod app;
#[path = "support/backend.rs"]
#[allow(dead_code)]
mod backend;
#[path = "support/http.rs"]
mod http;

use app::App;
use backend::Backend;
use http::{Session, success};
use serde_json::json;
use std::{process::Command, time::Duration};

fn docker() -> Command {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=1s", "10s", "docker"]);
    command
}

struct Balancer {
    name: String,
    url: String,
    _config: tempfile::TempDir,
}
impl Balancer {
    async fn start(ports: &[u16]) -> Self {
        assert!(!ports.is_empty() && ports.len() <= 2);
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let mut source = include_str!("../examples/nginx-session-affinity.conf").replace(
            "127.0.0.1:8081",
            &format!("host.docker.internal:{}", ports[0]),
        );
        source = if ports.len() == 2 {
            source.replace(
                "127.0.0.1:8082",
                &format!("host.docker.internal:{}", ports[1]),
            )
        } else {
            source.replace("        server 127.0.0.1:8082;", "")
        };
        // The test also disables frontend keepalive to prove affinity across
        // distinct connections, independent of any upstream connection reuse.
        source = source.replace("listen 8080;", "listen 8080;\n        keepalive_timeout 0;");
        let path = dir.path().join("nginx.conf");
        std::fs::write(&path, source).unwrap();
        let balancer = Self {
            name: format!("mcp-replica-test-{}", uuid::Uuid::new_v4()),
            url: format!("http://127.0.0.1:{port}"),
            _config: dir,
        };
        drop(reservation);
        let output = docker()
            .args([
                "run",
                "--detach",
                "--name",
                &balancer.name,
                "--add-host=host.docker.internal:host-gateway",
                "--publish",
                &format!("127.0.0.1:{port}:8080"),
                "--mount",
                &format!(
                    "type=bind,src={},dst=/etc/nginx/nginx.conf,readonly",
                    path.display()
                ),
                "nginx:1.28-alpine",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "NGINX startup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let state = docker()
                    .args(["inspect", "--format", "{{.State.Running}}", &balancer.name])
                    .output()
                    .unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&state.stdout).trim(),
                    "true",
                    "NGINX exited"
                );
                if let Ok(response) = http::client()
                    .get(format!("{}/readyz", balancer.url))
                    .send()
                    .await
                {
                    assert_eq!(
                        response.status(),
                        reqwest::StatusCode::OK,
                        "NGINX cannot reach ready replica"
                    );
                    assert_eq!(response.text().await.unwrap(), "ok");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("NGINX readiness deadline (no MCP handshake retries)");
        balancer
    }
}
impl Drop for Balancer {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let output = docker().args(["logs", &self.name]).output().unwrap();
            eprintln!("NGINX: {}", String::from_utf8_lossy(&output.stderr));
        }
        let _ = docker().args(["rm", "--force", &self.name]).output();
    }
}

async fn replica(backend: &Backend, name: &str) -> (App, u16) {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let config = format!(
        r#"
[proxy]
name = "replica-{name}"
shutdown_timeout_seconds = 1
[proxy.listen]
host = "0.0.0.0"
port = {port}
[auth]
type = "bearer"
tokens = ["data-token"]
[[backends]]
name = "api"
{}
[[backends.inject_args]]
tool = "echo"
args = {{ replica = "{name}" }}
overwrite = true
"#,
        backend.config
    );
    drop(reservation);
    (App::cli(&config, port).await, port)
}

async fn exercise(session: &Session, expected: Option<&str>, marker: u64) -> String {
    let listed = session.rpc("tools/list", json!({})).await;
    let mut names: Vec<_> = success(&listed)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["api/blocked", "api/echo"]);
    let result = session.call("api/echo", json!({"marker":marker})).await;
    let value: serde_json::Value =
        serde_json::from_str(success(&result)["content"][0]["text"].as_str().unwrap()).unwrap();
    let owner = value["replica"].as_str().unwrap();
    assert_eq!(value, json!({"marker":marker,"replica":owner}));
    assert!(["a", "b"].contains(&owner));
    if let Some(expected) = expected {
        assert_eq!(owner, expected);
    }
    owner.into()
}

async fn assert_session_missing(session: &Session, marker: &str) {
    let response = session
        .request(json!({"jsonrpc":"2.0","id":89,"method":"tools/call",
        "params":{"name":"api/echo","arguments":{"marker":marker}}}))
        .await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let reply = http::payload(response).await;
    // Upstream rejects the session before decoding the RPC envelope, so this
    // transport-level error has a null id rather than echoing the request id.
    assert_eq!(reply["id"], serde_json::Value::Null);
    assert_eq!(reply["error"]["code"], -32005);
    assert!(reply.get("result").is_none());
}

async fn routing_contract(crash: bool) {
    // HTTP fixture backends survive process loss without orphaning stdio children.
    let backend_a = Backend::new("http").await;
    let backend_b = Backend::new("http").await;
    let (mut a, port_a) = replica(&backend_a, "a").await;
    let (mut b, port_b) = replica(&backend_b, "b").await;
    let balancer = Balancer::start(&[port_a, port_b]).await;
    let mut session = Session::start(&balancer.url, "data-token").await;
    session.client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let owner = exercise(&session, None, 0).await;
    for marker in 1..4 {
        exercise(&session, Some(&owner), marker).await;
    }
    let events = session
        .client
        .get(&session.url)
        .bearer_auth("data-token")
        .header("accept", "text/event-stream")
        .header("mcp-protocol-version", http::VERSION)
        .header("mcp-session-id", &session.id)
        .send()
        .await
        .unwrap();
    assert_eq!(events.status(), reqwest::StatusCode::OK);
    assert!(
        events.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    drop(events);
    let (owner_app, survivor, survivor_port, owner_backend, survivor_backend, survivor_name) =
        if owner == "a" {
            (&mut a, &mut b, port_b, &backend_a, &backend_b, "b")
        } else {
            (&mut b, &mut a, port_a, &backend_b, &backend_a, "a")
        };
    assert_eq!(owner_backend.calls().len(), 4);
    assert!(survivor_backend.calls().is_empty());
    // Directly moving a live session to the other ready replica fails, even with
    // identical proxy configuration/credentials and healthy upstreams.
    let original = session.url.clone();
    session.url = format!("{}/", survivor.url);
    assert_session_missing(&session, "wrong-replica").await;
    assert!(survivor_backend.calls().is_empty());
    session.url = original;
    exercise(&session, Some(&owner), 4).await;
    if crash {
        owner_app.crash().await;
    } else {
        owner_app.shutdown().await;
    }
    // A request with no surviving owner must fail, without transparently replaying
    // on the healthy peer. NGINX's example explicitly disables upstream retries.
    let unavailable = session
        .request(json!({"jsonrpc":"2.0","id":88,"method":"tools/call",
        "params":{"name":"api/echo","arguments":{"marker":"lost-owner"}}}))
        .await;
    assert_eq!(unavailable.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(survivor_backend.calls().is_empty());
    drop(balancer);
    // Controlled removal of the retired peer represents a controller's endpoint
    // update. This is a real NGINX restart, not a Kubernetes-controller test.
    let remaining = Balancer::start(&[survivor_port]).await;
    session.url = format!("{}/", remaining.url);
    assert_session_missing(&session, "stale-session").await;
    assert!(survivor_backend.calls().is_empty());
    let fresh = Session::start(&remaining.url, "data-token").await;
    assert_ne!(fresh.id, session.id);
    exercise(&fresh, Some(survivor_name), 5).await;
    assert_eq!(owner_backend.calls().len(), 5);
    assert_eq!(survivor_backend.calls().len(), 1);
    fresh.terminate().await;
    survivor.shutdown().await;
}

#[tokio::test]
async fn affinity_survives_connections_and_recovers_after_graceful_owner_retirement() {
    routing_contract(false).await;
}

#[tokio::test]
async fn affinity_does_not_replay_calls_after_abrupt_session_owner_loss() {
    routing_contract(true).await;
}
