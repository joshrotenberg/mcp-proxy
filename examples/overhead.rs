//! End-to-end HTTP overhead benchmark harness.
//!
//! Measures what putting mcp-proxy in front of a backend costs, over real
//! HTTP on loopback, with protocol-correct MCP clients:
//!
//! 1. `direct`: client -> backend
//! 2. `proxied-minimal`: client -> mcp-proxy (no middleware) -> backend
//! 3. `proxied-loaded`: client -> mcp-proxy (timeout, retry, circuit
//!    breaker, cache, coalescing) -> backend
//!
//! Each scenario runs at several concurrency levels; every request carries
//! distinct arguments so caching and coalescing cannot short-circuit the
//! measurement. Methodology and baseline numbers: docs/benchmarks.md.
//!
//! Run with:
//!
//! ```bash
//! cargo run --release --example overhead
//! ```

use std::time::{Duration, Instant};

use tower_mcp::client::{HttpClientTransport, McpClient};
use tower_mcp::transport::http::HttpTransport;
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_proxy::{Proxy, ProxyConfig};

const WARMUP_PER_WORKER: usize = 20;
const REQUESTS_PER_CELL: usize = 2000;
const CONCURRENCY_LEVELS: &[usize] = &[1, 8, 32];

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoInput {
    v: u64,
}

fn echo_router() -> McpRouter {
    let echo = ToolBuilder::new("echo")
        .description("Echo a number")
        .handler(|input: EchoInput| async move { Ok(CallToolResult::text(input.v.to_string())) })
        .build();
    McpRouter::new()
        .server_info("bench-backend", "1.0.0")
        .tool(echo)
}

/// Serve an axum router on an ephemeral loopback port; returns the base URL.
async fn serve(router: axum::Router) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("server runs");
    });
    Ok(format!("http://{addr}"))
}

async fn start_backend() -> anyhow::Result<String> {
    serve(HttpTransport::new(echo_router()).into_router()).await
}

async fn start_proxy(backend_url: &str, loaded: bool) -> anyhow::Result<String> {
    let middleware = if loaded {
        r#"
[backends.timeout]
seconds = 5

[backends.retry]
max_retries = 2
initial_backoff_ms = 10
max_backoff_ms = 100

[backends.circuit_breaker]
failure_rate_threshold = 0.5
minimum_calls = 100
wait_duration_seconds = 5

[backends.cache]
tool_ttl_seconds = 60
max_entries = 10000
"#
    } else {
        ""
    };
    let toml = format!(
        r#"
[proxy]
name = "bench-proxy"
[proxy.listen]
host = "127.0.0.1"
port = 0

[performance]
coalesce_requests = {coalesce}

[[backends]]
name = "b"
transport = "http"
url = "{backend_url}"
{middleware}
"#,
        coalesce = loaded,
    );
    let config = ProxyConfig::parse(&toml)?;
    let proxy = Proxy::from_config(config).await?;
    let (router, _session_handle) = proxy.into_router();
    serve(router).await
}

/// The tool name differs by target: the backend exposes `echo`, the proxy
/// namespaces it as `b/echo`.
async fn run_cell(
    url: &str,
    tool: &str,
    concurrency: usize,
) -> anyhow::Result<(Duration, Vec<Duration>, usize)> {
    let per_worker = REQUESTS_PER_CELL / concurrency;
    // Workers settle their sessions first and rendezvous at the barrier, so
    // throughput measures steady state, not session-establishment storms
    // (those show up in the resets column instead).
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(concurrency + 1));
    let mut handles = Vec::new();
    for worker in 0..concurrency {
        let url = url.to_string();
        let tool = tool.to_string();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            // Under concurrent session creation the initialized notification
            // can be lost and the session stranded (tower-mcp#1174); connect
            // like a real client would: settle the session or reconnect.
            // Reconnects are counted and reported.
            let mut reconnects = 0usize;
            let client = 'connect: loop {
                let client = McpClient::connect(HttpClientTransport::new(&url))
                    .await
                    .expect("client connects");
                client
                    .initialize("bench-client", "1.0.0")
                    .await
                    .expect("initialize");
                for _ in 0..20 {
                    if client.list_tools().await.is_ok() {
                        break 'connect client;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                reconnects += 1;
                assert!(
                    reconnects <= 5,
                    "worker {worker}: session never settled after 5 connects"
                );
            };
            barrier.wait().await;
            let mut latencies = Vec::with_capacity(per_worker);
            let mut handshake_retries = 0usize;
            for i in 0..(WARMUP_PER_WORKER + per_worker) {
                let arg = (worker * 1_000_000 + i) as u64;
                let t = Instant::now();
                let mut attempts = 0;
                let result = loop {
                    attempts += 1;
                    match client
                        .call_tool(&tool, serde_json::json!({ "v": arg }))
                        .await
                    {
                        Ok(r) => break r,
                        // The initialized-notification race (tower-mcp#1174)
                        // can also surface under load; count and retry it so
                        // the run completes AND quantifies the rate.
                        Err(e)
                            if attempts <= 10
                                && e.to_string().contains("notifications/initialized") =>
                        {
                            handshake_retries += 1;
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(e) => panic!("call failed: {e}"),
                    }
                };
                let elapsed = t.elapsed();
                assert!(!result.is_error, "echo must not fail");
                if i >= WARMUP_PER_WORKER {
                    latencies.push(elapsed);
                }
            }
            (latencies, handshake_retries + reconnects)
        }));
    }
    barrier.wait().await;
    let started = Instant::now();
    let mut all = Vec::with_capacity(REQUESTS_PER_CELL);
    let mut retries = 0usize;
    for h in handles {
        let (latencies, worker_retries) = h.await?;
        all.extend(latencies);
        retries += worker_retries;
    }
    let elapsed = started.elapsed();
    all.sort();
    Ok((elapsed, all, retries))
}

fn pct(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend_url = start_backend().await?;
    let minimal_url = start_proxy(&backend_url, false).await?;
    let loaded_url = start_proxy(&backend_url, true).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let scenarios: &[(&str, &str, &str)] = &[
        ("direct", backend_url.as_str(), "echo"),
        ("proxied-minimal", minimal_url.as_str(), "b/echo"),
        ("proxied-loaded", loaded_url.as_str(), "b/echo"),
    ];

    println!(
        "{:<16} {:>4} {:>10} {:>10} {:>10} {:>10} {:>8}",
        "scenario", "conc", "req/s", "p50", "p95", "p99", "resets"
    );
    for (name, url, tool) in scenarios {
        for &concurrency in CONCURRENCY_LEVELS {
            let (elapsed, latencies, retries) = run_cell(url, tool, concurrency).await?;
            let throughput = latencies.len() as f64 / elapsed.as_secs_f64();
            println!(
                "{:<16} {:>4} {:>10.0} {:>10.2?} {:>10.2?} {:>10.2?} {:>8}",
                name,
                concurrency,
                throughput,
                pct(&latencies, 0.50),
                pct(&latencies, 0.95),
                pct(&latencies, 0.99),
                retries,
            );
        }
    }
    Ok(())
}
