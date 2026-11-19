//! Per-tool execution effects (F01, #298): which tool calls retry, hedging,
//! mirroring, failover, caching and coalescing may duplicate or suppress.
#[path = "support/http.rs"]
mod http;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tower::ServiceExt;
use tower_mcp::protocol::McpRequest;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, RouterRequest, RouterResponse, ToolBuilder,
};
use tower_mcp_types::JsonRpcError;

const TOOLS: &[&str] = &["counter", "lookup", "create"];

/// How a backend answers a tool call.
#[derive(Clone, Copy)]
enum Reply {
    Ok,
    /// A backend failure (`-32603`).
    Fail,
    /// Success after a delay.
    Slow,
}

type Policy = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

/// An HTTP MCP backend that counts the tool calls it receives by tool name.
struct Backend {
    server: http::Server,
    calls: Arc<Mutex<HashMap<String, usize>>>,
}

impl Backend {
    async fn start(policy: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let policy: Policy = Arc::new(policy);
        let mut router = McpRouter::new();
        for tool in TOOLS {
            router = router.tool(
                ToolBuilder::new(*tool)
                    .handler(|_: Value| async { Ok(CallToolResult::text("unused")) })
                    .build(),
            );
        }
        let calls = Arc::new(Mutex::new(HashMap::new()));
        let counted = calls.clone();
        let service = tower::service_fn(move |request: RouterRequest| {
            let router = router.clone();
            let calls = counted.clone();
            let policy = policy.clone();
            async move {
                let McpRequest::CallTool(params) = &request.inner else {
                    return router.oneshot(request).await;
                };
                *calls
                    .lock()
                    .unwrap()
                    .entry(params.name.clone())
                    .or_insert(0) += 1;
                let inner = match policy(&params.name) {
                    Reply::Ok => Ok(CallToolResult::text("ok")),
                    Reply::Slow => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok(CallToolResult::text("slow"))
                    }
                    Reply::Fail => Err(JsonRpcError::internal_error("backend failure")),
                };
                Ok::<_, std::convert::Infallible>(RouterResponse {
                    id: request.id,
                    inner: inner.map(tower_mcp::protocol::McpResponse::CallTool),
                })
            }
        });
        Self {
            server: http::Server::start(HttpTransport::from_service(service).into_router()).await,
            calls,
        }
    }

    fn url(&self) -> &str {
        &self.server.url
    }

    fn count(&self, tool: &str) -> usize {
        self.calls.lock().unwrap().get(tool).copied().unwrap_or(0)
    }

    /// Wait until `tool` has been received `expected` times (for detached
    /// mirror and hedge requests), then confirm it stays there.
    async fn settles_at(&self, tool: &str, expected: usize) {
        for _ in 0..100 {
            if self.count(tool) >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(self.count(tool), expected, "{tool}");
    }
}

/// Effects shared by every scenario: `lookup` is read, `create` is a write
/// with an idempotency key, `counter` is unlisted (write).
const EFFECTS: &str = r#"
[[backends.tool_effects]]
tools = ["lookup"]
effect = "read"
[[backends.tool_effects]]
tools = ["create"]
effect = "write"
idempotency_key = "request_id"
"#;

async fn proxy(backends: &str, extra: &str) -> (http::Server, http::Session) {
    let config = format!(
        r#"
[proxy]
name = "execution"
[proxy.listen]
{extra}
[auth]
type = "bearer"
tokens = ["t"]
{backends}
"#
    );
    let proxy = mcp_proxy::Proxy::from_config(mcp_proxy::ProxyConfig::parse(&config).unwrap())
        .await
        .unwrap();
    let server = http::Server::start(proxy.into_router().0).await;
    let session = http::Session::start(&server.url, "t").await;
    (server, session)
}

fn api(url: &str, policy: &str) -> String {
    format!(
        "[[backends]]\nname = \"api\"\ntransport = \"http\"\nurl = \"{url}/\"\n{policy}\n{EFFECTS}"
    )
}

#[tokio::test]
async fn retry_resends_read_and_keyed_calls_only() {
    let backend = Backend::start(|_| Reply::Fail).await;
    let (_proxy, session) = proxy(
        &api(
            backend.url(),
            "[backends.retry]\nmax_retries = 2\ninitial_backoff_ms = 0",
        ),
        "",
    )
    .await;

    for (tool, args) in [
        ("counter", json!({})),
        ("lookup", json!({})),
        ("create", json!({"request_id": "r-1"})),
    ] {
        let reply = session.call(&format!("api/{tool}"), args).await;
        assert_eq!(reply["error"]["code"], -32603, "{reply}");
    }
    assert_eq!(backend.count("counter"), 1, "write is sent once");
    assert_eq!(backend.count("lookup"), 3, "read is retried");
    assert_eq!(backend.count("create"), 3, "a keyed write is retried");

    session.call("api/create", json!({"request_id": ""})).await;
    assert_eq!(backend.count("create"), 4, "an empty key does not count");
}

#[tokio::test]
async fn hedging_copies_read_calls_only() {
    let backend = Backend::start(|_| Reply::Slow).await;
    let (_proxy, session) = proxy(
        &api(
            backend.url(),
            "[backends.hedging]\ndelay_ms = 0\nmax_hedges = 1",
        ),
        "",
    )
    .await;

    http::success(&session.call("api/counter", json!({})).await);
    http::success(&session.call("api/lookup", json!({})).await);
    backend.settles_at("counter", 1).await;
    backend.settles_at("lookup", 2).await;
}

#[tokio::test]
async fn cache_and_coalescing_share_read_calls_only() {
    let backend = Backend::start(|_| Reply::Slow).await;
    let (_proxy, session) = proxy(
        &api(backend.url(), "[backends.cache]\ntool_ttl_seconds = 60"),
        "[performance]\ncoalesce_requests = true",
    )
    .await;
    let session = Arc::new(session);

    for tool in ["lookup", "counter"] {
        let concurrent = (0..2).map(|_| {
            let session = session.clone();
            tokio::spawn(async move { session.call(&format!("api/{tool}"), json!({})).await })
        });
        for reply in futures_join(concurrent).await {
            http::success(&reply);
        }
        http::success(&session.call(&format!("api/{tool}"), json!({})).await);
    }
    assert_eq!(backend.count("lookup"), 1, "read is coalesced and cached");
    assert_eq!(backend.count("counter"), 3, "write always executes");
}

#[tokio::test]
async fn mirror_copies_replayable_calls_only() {
    let primary = Backend::start(|_| Reply::Ok).await;
    let shadow = Backend::start(|_| Reply::Ok).await;
    let backends = format!(
        "{}\n[[backends]]\nname = \"shadow\"\ntransport = \"http\"\nurl = \"{}/\"\nmirror_of = \"api\"\nmirror_percent = 100\n",
        api(primary.url(), ""),
        shadow.url()
    );
    let (_proxy, session) = proxy(&backends, "").await;

    http::success(&session.call("api/counter", json!({})).await);
    http::success(&session.call("api/lookup", json!({})).await);
    http::success(
        &session
            .call("api/create", json!({"request_id": "r-1"}))
            .await,
    );
    shadow.settles_at("lookup", 1).await;
    shadow.settles_at("create", 1).await;
    shadow.settles_at("counter", 0).await;
    assert_eq!(primary.count("counter"), 1);
}

#[tokio::test]
async fn failover_resends_backend_failures_and_unreceived_writes_only() {
    let primary = Backend::start(|_| Reply::Fail).await;
    let backup = Backend::start(|_| Reply::Ok).await;
    let breaker = "[backends.circuit_breaker]\nminimum_calls = 2\nfailure_rate_threshold = 0.5\nwait_duration_seconds = 60\n[[backends.locked_args]]\ntools = [\"lookup\"]\nargs = { region = \"us\" }";
    let backends = format!(
        "{}\n[[backends]]\nname = \"backup\"\ntransport = \"http\"\nurl = \"{}/\"\nfailover_for = \"api\"\n",
        api(primary.url(), breaker),
        backup.url()
    );
    let (_proxy, session) = proxy(&backends, "").await;

    // A client error would fail the same way on the backup, even for a read.
    let reply = session.call("api/lookup", json!({"region": "eu"})).await;
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    assert_eq!(backup.count("lookup"), 0);

    // The primary received the write and failed: it may have applied it.
    let reply = session.call("api/counter", json!({})).await;
    assert_eq!(reply["error"]["code"], -32603, "{reply}");
    assert_eq!(backup.count("counter"), 0);

    // Reads fail over; their failures open the primary's breaker.
    for _ in 0..2 {
        http::success(&session.call("api/lookup", json!({})).await);
    }
    assert_eq!(backup.count("lookup"), 2);

    // The open breaker rejects the write before it reaches the primary, so
    // the backup may run it.
    http::success(&session.call("api/counter", json!({})).await);
    assert_eq!(backup.count("counter"), 1);
    assert_eq!(primary.count("counter"), 1, "the primary never saw it");
}

#[tokio::test]
async fn default_effect_read_restores_replay_for_unlisted_tools() {
    let backend = Backend::start(|_| Reply::Fail).await;
    let (_proxy, session) = proxy(
        &api(
            backend.url(),
            "default_effect = \"read\"\n[backends.retry]\nmax_retries = 1\ninitial_backoff_ms = 0",
        ),
        "",
    )
    .await;
    session.call("api/counter", json!({})).await;
    assert_eq!(backend.count("counter"), 2);
}

#[test]
fn invalid_tool_effects_are_rejected() {
    let parse = |effects: &str| {
        mcp_proxy::ProxyConfig::parse(&format!(
            "[proxy]\nname = \"x\"\n[proxy.listen]\n[[backends]]\nname = \"api\"\ntransport = \"http\"\nurl = \"http://localhost:1/\"\n{effects}"
        ))
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
    };
    let cases = [
        ("tools = []\neffect = \"read\"", "must not be empty"),
        ("tools = [\"re:^x$\"]\neffect = \"read\"", "regex entries"),
        ("tools = [\"[x\"]\neffect = \"read\"", "invalid pattern"),
        (
            "tools = [\"x\"]\neffect = \"read\"\nidempotency_key = \"k\"",
            "only to effect = \"write\"",
        ),
    ];
    for (entry, expected) in cases {
        let err = parse(&format!("[[backends.tool_effects]]\n{entry}")).unwrap_err();
        assert!(err.contains(expected), "{entry}: {err}");
    }
    for (first, second) in [
        ("[\"get_*\"]", "[\"get_user\"]"),
        ("[\"x\"]", "[\"x\"]"),
        ("[\"*\"]", "[\"get_?\"]"),
    ] {
        let err = parse(&format!(
            "[[backends.tool_effects]]\ntools = {first}\neffect = \"read\"\n[[backends.tool_effects]]\ntools = {second}\neffect = \"write\""
        ))
        .unwrap_err();
        assert!(err.contains("already matched"), "{first} {second}: {err}");
    }
    // Two distinct wildcards may overlap; order decides.
    parse(
        "[[backends.tool_effects]]\ntools = [\"get_?\"]\neffect = \"read\"\n[[backends.tool_effects]]\ntools = [\"get_*\"]\neffect = \"write\"",
    )
    .unwrap();
}

async fn futures_join<T: Send + 'static>(
    handles: impl Iterator<Item = tokio::task::JoinHandle<T>>,
) -> Vec<T> {
    let mut out = Vec::new();
    for handle in handles.collect::<Vec<_>>() {
        out.push(handle.await.unwrap());
    }
    out
}
