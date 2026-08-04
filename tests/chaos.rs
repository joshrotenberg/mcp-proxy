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

use mcp_proxy::config::OutlierDetectionConfig;
use mcp_proxy::outlier::{OutlierDetectionLayer, OutlierDetector};
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

// ---------------------------------------------------------------------------
// Retry budget (#207)
// ---------------------------------------------------------------------------
//
// Plane note: a tool handler error becomes `CallToolResult { is_error }`, an
// Ok response, per MCP semantics; the retry predicate deliberately does not
// retry those (the tool executed). Retriable failures
// (`RouterResponse.inner = Err` with code -32603 or -32000..-32099) arise
// below the retry layer from transport-level faults. These tests inject at
// that plane with a response-rewriting layer between retry and the backend.

/// Layer that rewrites successful responses into retriable internal errors
/// (-32603) while `failures_remaining` is nonzero. Sits under retry, standing
/// in for a flaky transport.
fn transient_failure_layer(
    failures_remaining: Arc<AtomicUsize>,
) -> impl tower::Layer<
    tower_mcp::proxy::BackendService,
    Service = tower::util::BoxCloneService<RouterRequest, RouterResponse, Infallible>,
> {
    tower::layer::layer_fn(move |inner: tower_mcp::proxy::BackendService| {
        let failures = failures_remaining.clone();
        tower::util::BoxCloneService::new(tower::ServiceExt::map_response(
            inner,
            move |resp: RouterResponse| {
                if failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    RouterResponse {
                        id: resp.id,
                        inner: Err(tower_mcp_types::JsonRpcError::internal_error(
                            "transient transport failure",
                        )),
                    }
                } else {
                    resp
                }
            },
        ))
    })
}

fn retry_config(
    max_retries: u32,
    budget_percent: Option<f64>,
    min_retries_per_sec: u32,
) -> mcp_proxy::config::RetryConfig {
    mcp_proxy::config::RetryConfig {
        max_retries,
        initial_backoff_ms: 1,
        max_backoff_ms: 5,
        budget_percent,
        min_retries_per_sec,
    }
}

async fn build_transient_proxy(
    cfg: &mcp_proxy::config::RetryConfig,
    hits: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
) -> McpProxy {
    // One composed layer: production retry over the response-plane fault
    // injector, which stands in for a flaky transport under the retry seam.
    let stack = ServiceBuilder::new()
        .layer(mcp_proxy::retry::build_retry_layer(cfg, "flaky"))
        .layer(transient_failure_layer(failures));

    McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "flaky",
            ChannelTransport::new(ping_router(hits, Arc::new(AtomicBool::new(false)))),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build")
}

/// A transient failure is retried through the production retry mapping and
/// succeeds within `max_retries`.
#[tokio::test]
async fn retry_absorbs_transient_failures() {
    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(2));
    let cfg = retry_config(3, None, 10);
    let mut proxy = build_transient_proxy(&cfg, hits.clone(), failures).await;

    let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
    assert_eq!(ok_text(&resp), "pong");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "two failed attempts plus the success"
    );
}

/// With the failure persisting past `max_retries`, the caller gets the error
/// back after exactly 1 + max_retries attempts; retries never run away.
#[tokio::test]
async fn retry_gives_up_after_max_retries() {
    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(usize::MAX));
    let cfg = retry_config(2, None, 10);
    let mut proxy = build_transient_proxy(&cfg, hits.clone(), failures).await;

    let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
    let err = resp.inner.as_ref().expect_err("failure surfaces");
    assert_eq!(err.code, -32603, "the backend error is returned as-is");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "initial attempt plus exactly max_retries"
    );
}

/// Under sustained total failure, the retry budget caps attempt volume: the
/// token bucket (budget_percent with min_retries_per_sec = 0, so burst-only)
/// stops retries once spent, and later requests fail fast with a single
/// attempt instead of amplifying the outage.
#[tokio::test]
async fn retry_budget_caps_attempts_under_sustained_failure() {
    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(usize::MAX));
    // budget_percent = 1.0 maps to a 10-token burst with zero refill in the
    // production mapping (see build_retry_layer).
    let cfg = retry_config(3, Some(1.0), 0);
    let mut proxy = build_transient_proxy(&cfg, hits.clone(), failures).await;

    let requests = 30;
    for _ in 0..requests {
        let resp = call(&mut proxy, tool_call("flaky/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err(), "sustained failure surfaces every time");
    }

    let total_attempts = hits.load(Ordering::SeqCst);
    // Unbudgeted this would be requests * (1 + max_retries) = 120 attempts.
    assert!(
        total_attempts >= requests,
        "every request attempts at least once (got {total_attempts})"
    );
    assert!(
        total_attempts > requests,
        "the budget allows some retries before it is spent (got {total_attempts})"
    );
    assert!(
        total_attempts <= requests + 15,
        "attempts stay within the ~10-token budget plus slack, no retry storm \
         (got {total_attempts})"
    );
}

// ---------------------------------------------------------------------------
// Outlier detection (#208)
// ---------------------------------------------------------------------------
//
// Outlier detection observes the response plane (`inner = Err` with internal
// or server error codes), so the same response-rewriting fault layer used
// for retry stands in for an unhealthy backend. Each `OutlierDetectionLayer`
// registers its backend with the shared `OutlierDetector` on construction;
// ejection quota is total * max_ejection_percent / 100 with a floor of one.

fn outlier_config(consecutive_errors: u32, base_ejection_seconds: u64) -> OutlierDetectionConfig {
    OutlierDetectionConfig {
        consecutive_errors,
        interval_seconds: 10,
        base_ejection_seconds,
        max_ejection_percent: 50,
    }
}

/// Reaching `consecutive_errors` ejects the backend: calls fail fast without
/// reaching it. Below the threshold nothing is ejected.
#[tokio::test]
async fn outlier_ejects_after_consecutive_errors_and_fails_fast() {
    let detector = OutlierDetector::new(50);

    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(usize::MAX));

    let stack = ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "sick".to_string(),
            outlier_config(3, 60),
            detector.clone(),
        ))
        .layer(transient_failure_layer(failures));

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "sick",
            ChannelTransport::new(ping_router(hits.clone(), Arc::new(AtomicBool::new(false)))),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build");

    // Three consecutive errors: each reaches the backend, then ejection.
    for i in 1..=3 {
        let resp = call(&mut proxy, tool_call("sick/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err(), "call {i} fails at the response plane");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "all three reach the backend"
    );
    assert_eq!(detector.ejected_count(), 1, "backend is ejected");

    // Ejected: fail fast, backend untouched.
    let resp = call(&mut proxy, tool_call("sick/ping", serde_json::json!({}))).await;
    assert!(resp.inner.is_err(), "ejected backend fails fast");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "ejected call must not reach the backend"
    );
}

/// A success resets the consecutive-error streak: alternating failures never
/// eject.
#[tokio::test]
async fn outlier_success_resets_the_error_streak() {
    let detector = OutlierDetector::new(50);

    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(2));

    let stack = ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "wobbly".to_string(),
            outlier_config(3, 60),
            detector.clone(),
        ))
        .layer(transient_failure_layer(failures.clone()));

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "wobbly",
            ChannelTransport::new(ping_router(hits.clone(), Arc::new(AtomicBool::new(false)))),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build");

    // Two failures, then a success (injector exhausted), then two more
    // failures: the streak never reaches three.
    for _ in 0..2 {
        let resp = call(&mut proxy, tool_call("wobbly/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err());
    }
    let resp = call(&mut proxy, tool_call("wobbly/ping", serde_json::json!({}))).await;
    assert_eq!(ok_text(&resp), "pong", "streak broken by a success");

    failures.store(2, Ordering::SeqCst);
    for _ in 0..2 {
        let resp = call(&mut proxy, tool_call("wobbly/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err());
    }
    assert_eq!(detector.ejected_count(), 0, "never ejected");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        5,
        "every call reached the backend"
    );
}

/// `max_ejection_percent` caps how much of the fleet can be ejected: with a
/// 50 percent cap and one of two backends already out, the second failing
/// backend keeps receiving traffic.
#[tokio::test]
async fn outlier_max_ejection_percent_caps_ejections() {
    let detector = OutlierDetector::new(50);

    let hits_a = Arc::new(AtomicUsize::new(0));
    let hits_b = Arc::new(AtomicUsize::new(0));
    let failures_a = Arc::new(AtomicUsize::new(usize::MAX));
    let failures_b = Arc::new(AtomicUsize::new(usize::MAX));

    let stack_a = ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "a".to_string(),
            outlier_config(3, 60),
            detector.clone(),
        ))
        .layer(transient_failure_layer(failures_a));
    let stack_b = ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "b".to_string(),
            outlier_config(3, 60),
            detector.clone(),
        ))
        .layer(transient_failure_layer(failures_b));

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "a",
            ChannelTransport::new(ping_router(
                hits_a.clone(),
                Arc::new(AtomicBool::new(false)),
            )),
        )
        .await
        .backend_layer(stack_a)
        .backend(
            "b",
            ChannelTransport::new(ping_router(
                hits_b.clone(),
                Arc::new(AtomicBool::new(false)),
            )),
        )
        .await
        .backend_layer(stack_b)
        .build_strict()
        .await
        .expect("proxy should build");

    // Eject A (1 of 2 = 50 percent, at the cap).
    for _ in 0..3 {
        let resp = call(&mut proxy, tool_call("a/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err());
    }
    assert_eq!(detector.ejected_count(), 1, "A is ejected");

    // B fails just as hard, but ejecting it would exceed the cap: it keeps
    // receiving traffic.
    for _ in 0..5 {
        let resp = call(&mut proxy, tool_call("b/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err());
    }
    assert_eq!(detector.ejected_count(), 1, "B is not ejected (cap)");
    assert_eq!(
        hits_b.load(Ordering::SeqCst),
        5,
        "B keeps reaching the backend"
    );
}

/// After `base_ejection_seconds` the backend is unejected and traffic
/// resumes. Ignored by default: the config granularity is whole seconds, so
/// this needs 1s+ of wall clock. Run with --ignored.
#[tokio::test]
#[ignore = "needs 1s+ wall clock (base_ejection_seconds granularity)"]
async fn outlier_ejection_expires_and_traffic_resumes() {
    let detector = OutlierDetector::new(50);

    let hits = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(3));

    let stack = ServiceBuilder::new()
        .layer(OutlierDetectionLayer::new(
            "healing".to_string(),
            outlier_config(3, 1),
            detector.clone(),
        ))
        .layer(transient_failure_layer(failures));

    let mut proxy = McpProxy::builder("chaos-proxy", "1.0.0")
        .separator("/")
        .backend(
            "healing",
            ChannelTransport::new(ping_router(hits.clone(), Arc::new(AtomicBool::new(false)))),
        )
        .await
        .backend_layer(stack)
        .build_strict()
        .await
        .expect("proxy should build");

    for _ in 0..3 {
        let resp = call(&mut proxy, tool_call("healing/ping", serde_json::json!({}))).await;
        assert!(resp.inner.is_err());
    }
    assert_eq!(detector.ejected_count(), 1);

    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Fault injector is exhausted: the unejected backend serves again.
    let resp = call(&mut proxy, tool_call("healing/ping", serde_json::json!({}))).await;
    assert_eq!(
        ok_text(&resp),
        "pong",
        "traffic resumes after ejection expires"
    );
    assert_eq!(detector.ejected_count(), 0, "uneject recorded");
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}
