//! Chaos tier: resilience middleware behavior under injected faults.
//!
//! Composes the same `tower_resilience` layers `build_mcp_proxy` applies
//! (identical builder calls and parameter mapping to src/proxy.rs) around
//! in-process `ChannelTransport` backends, with `tower-resilience-chaos`
//! injecting service-level faults.
//!
//! Failure-counting contract exercised here: the circuit breaker counts
//! service-level errors (injected faults, timeouts). An MCP error *result*
//! (`CallToolResult::error`) is an `Ok` response and does not count; backend
//! health and tool-level failure are different planes.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tower::Service;
use tower::timeout::TimeoutLayer;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, McpRequest, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

// ---------------------------------------------------------------------------
// Helpers (same idiom as tests/e2e.rs)
// ---------------------------------------------------------------------------

async fn call<S>(svc: &mut S, request: McpRequest) -> RouterResponse
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
{
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: request,
        extensions: Extensions::new(),
    };
    svc.call(req).await.expect("infallible")
}

fn tool_call(name: &str, args: serde_json::Value) -> McpRequest {
    McpRequest::CallTool(CallToolParams {
        name: name.to_string(),
        arguments: args,
        input_responses: None,
        request_state: None,
        meta: None,
        task: None,
    })
}

/// Backend with a hit counter and a switchable slow mode. When `slow` is set,
/// the handler sleeps long enough to trip a `TimeoutLayer` placed above it.
fn ping_router(hits: Arc<AtomicUsize>, slow: Arc<AtomicBool>) -> McpRouter {
    let ping = ToolBuilder::new("ping")
        .description("Ping; slow when the fault flag is set")
        .handler(move |_: tower_mcp::NoParams| {
            let hits = hits.clone();
            let slow = slow.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                if slow.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(CallToolResult::text("pong"))
            }
        })
        .build();
    McpRouter::new()
        .server_info("ping-server", "1.0.0")
        .tool(ping)
}

/// Backend whose tool always returns an MCP error *result* (an Ok response).
fn mcp_error_router(hits: Arc<AtomicUsize>) -> McpRouter {
    let fail = ToolBuilder::new("fail")
        .description("Always returns an MCP error result")
        .handler(move |_: tower_mcp::NoParams| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Ok(CallToolResult::error("tool-level failure"))
            }
        })
        .build();
    McpRouter::new()
        .server_info("mcp-error-server", "1.0.0")
        .tool(fail)
}

/// The exact breaker construction from src/proxy.rs, with test-sized values.
fn breaker_layer(
    name: &str,
    minimum_calls: usize,
    wait_in_open: Duration,
    permitted_in_half_open: usize,
) -> (
    tower_resilience::circuitbreaker::CircuitBreakerLayer,
    tower_resilience::circuitbreaker::CircuitBreakerHandle,
) {
    tower_resilience::circuitbreaker::CircuitBreakerLayer::builder()
        .failure_rate_threshold(0.5)
        .minimum_number_of_calls(minimum_calls)
        .wait_duration_in_open(wait_in_open)
        .permitted_calls_in_half_open(permitted_in_half_open)
        .name(format!("{name}-cb"))
        .build_with_handle()
}

fn err_text(resp: &RouterResponse) -> String {
    format!("{:?}", resp.inner.as_ref().expect_err("expected an error"))
}

fn ok_text(resp: &RouterResponse) -> String {
    match resp.inner.as_ref().expect("expected success") {
        tower_mcp::protocol::McpResponse::CallTool(result) => result.all_text(),
        other => panic!("expected CallTool, got: {other:?}"),
    }
}

/// TEMPORARY probe 2: do stacked backend_layer calls compose, or does the
/// last call win? chaos(55ms) added first, timeout(10ms) added second: if
/// they stack, the call times out AND injected==1; if last-wins, the call
/// succeeds instantly with injected==0.
#[tokio::test]
async fn probe_backend_layer_stacking() {
    let injected = Arc::new(AtomicUsize::new(0));
    let injected_cb = injected.clone();
    let chaos = tower_resilience_chaos::ChaosLayer::builder()
        .name("probe2-chaos")
        .latency_rate(1.0)
        .min_latency(Duration::from_millis(50))
        .max_latency(Duration::from_millis(60))
        .on_latency_injected(move |_| {
            injected_cb.fetch_add(1, Ordering::SeqCst);
        })
        .seed(7)
        .build();

    let hits = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(false));
    let mut proxy = McpProxy::builder("probe2-proxy", "1.0.0")
        .separator("/")
        .backend("p", ChannelTransport::new(ping_router(hits, slow)))
        .await
        .backend_layer(chaos)
        .backend_layer(TimeoutLayer::new(Duration::from_millis(10)))
        .build_strict()
        .await
        .expect("proxy should build");

    let started = std::time::Instant::now();
    let resp = call(&mut proxy, tool_call("p/ping", serde_json::json!({}))).await;
    eprintln!(
        "probe2: elapsed={:?} injected={} inner_ok={}",
        started.elapsed(),
        injected.load(Ordering::SeqCst),
        resp.inner.is_ok()
    );
}

/// TEMPORARY probe: does backend_layer apply layers at all?
#[tokio::test]
async fn probe_backend_layer_applies() {
    let injected = Arc::new(AtomicUsize::new(0));
    let injected_cb = injected.clone();
    let chaos = tower_resilience_chaos::ChaosLayer::builder()
        .name("probe-chaos")
        .latency_rate(1.0)
        .min_latency(Duration::from_millis(50))
        .max_latency(Duration::from_millis(60))
        .on_latency_injected(move |_| {
            injected_cb.fetch_add(1, Ordering::SeqCst);
        })
        .seed(7)
        .build();

    let hits = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(false));
    let mut proxy = McpProxy::builder("probe-proxy", "1.0.0")
        .separator("/")
        .backend("p", ChannelTransport::new(ping_router(hits, slow)))
        .await
        .backend_layer(chaos)
        .build_strict()
        .await
        .expect("proxy should build");

    let started = std::time::Instant::now();
    let resp = call(&mut proxy, tool_call("p/ping", serde_json::json!({}))).await;
    eprintln!(
        "probe: elapsed={:?} injected={} inner_ok={}",
        started.elapsed(),
        injected.load(Ordering::SeqCst),
        resp.inner.is_ok()
    );
}

// ---------------------------------------------------------------------------
// Circuit breaker lifecycle (#206)
// ---------------------------------------------------------------------------

/// Chaos-injected latency trips the timeout, the timeouts count as breaker
/// failures, and the open breaker rejects calls before they reach the chaos
/// layer.
///
/// Layer note: the backend seam is `Error = Infallible` (failures below it are
/// folded into the response), so chaos error-mode injection cannot compose
/// here at all; latency injection plus a timeout is the way to manufacture
/// service-level failures, and it mirrors the production failure path (slow
/// backend, `[backends.timeout]`, `[backends.circuit_breaker]`).
#[tokio::test]
async fn chaos_latency_timeouts_open_the_breaker() {
    let injected = Arc::new(AtomicUsize::new(0));
    let injected_cb = injected.clone();

    let chaos = tower_resilience_chaos::ChaosLayer::builder()
        .name("flaky-chaos")
        .latency_rate(1.0)
        .min_latency(Duration::from_millis(50))
        .max_latency(Duration::from_millis(60))
        .on_latency_injected(move |_| {
            injected_cb.fetch_add(1, Ordering::SeqCst);
        })
        .seed(42)
        .build();

    let hits = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(false));
    let (breaker, _handle) = breaker_layer("flaky", 5, Duration::from_secs(60), 1);

    // Same relative order as src/proxy.rs: timeout inside, breaker outside,
    // with chaos latency between backend and timeout.
    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "flaky",
            ChannelTransport::new(ping_router(hits.clone(), slow)),
        )
        .await
        .backend_layer(chaos)
        .backend_layer(TimeoutLayer::new(Duration::from_millis(10)))
        .backend_layer(breaker)
        .build_strict()
        .await
        .expect("proxy should build");

    // Calls 1-5: every call is delayed past the timeout and fails; the
    // breaker is still closed, so each one reaches the chaos layer.
    for i in 1..=5 {
        let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err(), "call {i} should time out");
    }
    assert_eq!(injected.load(Ordering::SeqCst), 5, "all 5 calls reach chaos");

    // The window is full at 100% failure rate: the breaker is open and must
    // reject the next call before it reaches the chaos layer.
    let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
    let err = err_text(&resp);
    assert!(resp.inner.is_err(), "open breaker rejects, got: {err}");
    assert_eq!(
        injected.load(Ordering::SeqCst),
        5,
        "rejected call must not reach the chaos layer (rejection was: {err})"
    );
}

/// Full lifecycle with the production timeout->breaker composition: timeouts
/// count as failures and open the breaker; after `wait_duration_in_open` the
/// half-open probes succeed (fault cleared) and the breaker closes.
#[tokio::test]
async fn timeout_breaker_opens_then_recovers_through_half_open() {
    let hits = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(true));
    let (breaker, _handle) = breaker_layer("slowpoke", 4, Duration::from_millis(200), 2);

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "slowpoke",
            ChannelTransport::new(ping_router(hits.clone(), slow.clone())),
        )
        .await
        // Same relative order as src/proxy.rs: timeout inside, breaker outside.
        .backend_layer(TimeoutLayer::new(Duration::from_millis(10)))
        .backend_layer(breaker)
        .build_strict()
        .await
        .expect("proxy should build");

    // Phase 1: four timeouts fill the window; breaker opens.
    for i in 1..=4 {
        let started = std::time::Instant::now();
        let resp = call(
            &mut proxy,
            tool_call("slowpoke/ping", serde_json::json!({})),
        )
        .await;
        eprintln!(
            "call {i}: elapsed={:?} inner={:?}",
            started.elapsed(),
            resp.inner
        );
        assert!(resp.inner.is_err(), "call {i} should time out");
    }
    let hits_after_trip = hits.load(Ordering::SeqCst);
    assert_eq!(hits_after_trip, 4, "all four calls reach the backend");

    // Open: rejected without reaching the backend.
    let resp = call(
        &mut proxy,
        tool_call("slowpoke/ping", serde_json::json!({})),
    )
    .await;
    assert!(resp.inner.is_err(), "open breaker rejects");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        hits_after_trip,
        "rejected call must not reach the backend"
    );

    // Phase 2: wait out the open window, clear the fault, and probe. The
    // permitted half-open probes succeed and close the breaker.
    tokio::time::sleep(Duration::from_millis(300)).await;
    slow.store(false, Ordering::SeqCst);

    for i in 1..=2 {
        let resp = call(
            &mut proxy,
            tool_call("slowpoke/ping", serde_json::json!({})),
        )
        .await;
        assert_eq!(ok_text(&resp), "pong", "half-open probe {i} succeeds");
    }

    // Phase 3: closed again; traffic flows normally.
    let resp = call(
        &mut proxy,
        tool_call("slowpoke/ping", serde_json::json!({})),
    )
    .await;
    assert_eq!(ok_text(&resp), "pong");
    assert_eq!(hits.load(Ordering::SeqCst), hits_after_trip + 3);
}

/// MCP error results are Ok responses: they do not count as breaker failures.
/// A backend can fail every tool call at the MCP level forever without
/// tripping the breaker. Tool-level failure and backend health are
/// deliberately different planes; this test pins that contract.
#[tokio::test]
async fn mcp_error_results_do_not_trip_the_breaker() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (breaker, _handle) = breaker_layer("erroring", 3, Duration::from_secs(60), 1);

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "erroring",
            ChannelTransport::new(mcp_error_router(hits.clone())),
        )
        .await
        .backend_layer(breaker)
        .build_strict()
        .await
        .expect("proxy should build");

    // Twice the minimum window of MCP-level failures.
    for _ in 0..6 {
        let resp = call(&mut proxy, tool_call("erroring/fail", serde_json::json!({}))).await;
        match resp.inner.as_ref().expect("MCP error result is an Ok response") {
            tower_mcp::protocol::McpResponse::CallTool(result) => {
                assert!(result.is_error, "tool reports an MCP-level error");
            }
            other => panic!("expected CallTool, got: {other:?}"),
        }
    }

    // Still closed: the next call reaches the backend.
    let before = hits.load(Ordering::SeqCst);
    let resp = call(&mut proxy, tool_call("erroring/fail", serde_json::json!({}))).await;
    assert!(resp.inner.is_ok(), "breaker must not have opened");
    assert_eq!(hits.load(Ordering::SeqCst), before + 1);
}
