//! Chaos tier: resilience middleware behavior under injected faults.
//!
//! Drives in-process `ChannelTransport` backends through `McpProxy` with the
//! same `tower_resilience` builders and parameter mapping `build_mcp_proxy`
//! uses, and `tower-resilience-chaos` injecting faults.
//!
//! Two structural constraints shape these tests, both discovered while
//! writing them (#218):
//!
//! - The backend seam is `Error = Infallible`; failures below it are folded
//!   into the response. Chaos error-mode injection cannot compose there, so
//!   service-level failures are manufactured with chaos *latency* injection
//!   plus a timeout, mirroring the production failure path (slow backend,
//!   `[backends.timeout]`, `[backends.circuit_breaker]`).
//! - tower-mcp's `backend_layer` replaces rather than stacks
//!   (joshrotenberg/tower-mcp#1173), so each test hands it ONE composed
//!   layer, exactly as the fixed `build_mcp_proxy` does.
//!
//! Failure-counting contract pinned here: the circuit breaker counts
//! service-level errors (injected latency tripping the timeout). An MCP
//! error *result* (`CallToolResult::error`) is an `Ok` response and does not
//! count; backend health and tool-level failure are different planes.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tower::Service;
use tower::ServiceBuilder;
use tower::timeout::TimeoutLayer;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
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
/// the handler sleeps long enough to trip a `TimeoutLayer` above it.
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

/// The breaker construction from src/proxy.rs with test-sized values,
/// including the `sliding_window_size` alignment from #220.
fn breaker_layer(
    name: &str,
    minimum_calls: usize,
    wait_in_open: Duration,
    permitted_in_half_open: usize,
) -> tower_resilience::circuitbreaker::CircuitBreakerLayer {
    let (layer, _handle) = tower_resilience::circuitbreaker::CircuitBreakerLayer::builder()
        .failure_rate_threshold(0.5)
        .minimum_number_of_calls(minimum_calls)
        .sliding_window_size(minimum_calls)
        .wait_duration_in_open(wait_in_open)
        .permitted_calls_in_half_open(permitted_in_half_open)
        .name(format!("{name}-cb"))
        .build_with_handle();
    layer
}

fn ok_text(resp: &RouterResponse) -> String {
    match resp.inner.as_ref().expect("expected success") {
        McpResponse::CallTool(result) => result.all_text(),
        other => panic!("expected CallTool, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker lifecycle (#206)
// ---------------------------------------------------------------------------

/// Chaos-injected latency trips the timeout, the timeouts count as breaker
/// failures, and the open breaker rejects calls before they reach the chaos
/// layer. The `on_latency_injected` counter proves which calls traversed to
/// the chaos layer and which were rejected above it.
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

    // One composed layer, outermost first: breaker, then timeout, then chaos
    // latency directly over the backend (the production relative order).
    let stack = ServiceBuilder::new()
        .layer(breaker_layer("flaky", 5, Duration::from_secs(60), 1))
        .layer(TimeoutLayer::new(Duration::from_millis(10)))
        .layer(chaos);

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "flaky",
            ChannelTransport::new(ping_router(hits.clone(), slow)),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build");

    // Calls 1-5: every call is delayed past the timeout and fails; the
    // breaker is still closed, so each one reaches the chaos layer.
    for i in 1..=5 {
        let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err(), "call {i} should time out");
    }
    assert_eq!(
        injected.load(Ordering::SeqCst),
        5,
        "all 5 calls reach chaos"
    );

    // Window full at 100% failure rate: the breaker is open and must reject
    // the next call before it reaches the chaos layer.
    let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
    assert!(resp.inner.is_err(), "open breaker rejects");
    assert_eq!(
        injected.load(Ordering::SeqCst),
        5,
        "rejected call must not reach the chaos layer"
    );
}

/// Full lifecycle with the production timeout->breaker composition: timeouts
/// count as failures and open the breaker; after `wait_duration_in_open` the
/// half-open probes succeed (fault cleared) and the breaker closes.
#[tokio::test]
async fn timeout_breaker_opens_then_recovers_through_half_open() {
    let hits = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicBool::new(true));

    let stack = ServiceBuilder::new()
        .layer(breaker_layer("slowpoke", 4, Duration::from_millis(200), 2))
        .layer(TimeoutLayer::new(Duration::from_millis(10)));

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "slowpoke",
            ChannelTransport::new(ping_router(hits.clone(), slow.clone())),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build");

    // Phase 1: four timeouts fill the window; breaker opens.
    for i in 1..=4 {
        let resp = call(
            &mut proxy,
            tool_call("slowpoke/ping", serde_json::json!({})),
        )
        .await;
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

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "erroring",
            ChannelTransport::new(mcp_error_router(hits.clone())),
        )
        .await
        .backend_layer(breaker_layer("erroring", 3, Duration::from_secs(60), 1))
        .build_strict()
        .await
        .expect("proxy should build");

    // Twice the minimum window of MCP-level failures.
    for _ in 0..6 {
        let resp = call(
            &mut proxy,
            tool_call("erroring/fail", serde_json::json!({})),
        )
        .await;
        match resp
            .inner
            .as_ref()
            .expect("MCP error result is an Ok response")
        {
            McpResponse::CallTool(result) => {
                assert!(result.is_error, "tool reports an MCP-level error");
            }
            other => panic!("expected CallTool, got: {other:?}"),
        }
    }

    // Still closed: the next call reaches the backend.
    let before = hits.load(Ordering::SeqCst);
    let resp = call(
        &mut proxy,
        tool_call("erroring/fail", serde_json::json!({})),
    )
    .await;
    assert!(resp.inner.is_ok(), "breaker must not have opened");
    assert_eq!(hits.load(Ordering::SeqCst), before + 1);
}
