//! Retry middleware for per-backend request retries with exponential backoff.
//!
//! Uses tower-resilience's [`RetryLayer`] with a response-based predicate to
//! retry MCP error responses with transient error codes (internal errors,
//! timeouts), using the shared [`crate::outcome`] policy. Tool `isError` results,
//! unknown application codes and known client/protocol errors are not retried.
//!
//! # Retry Budget
//!
//! When `budget_percent` is configured, each layer shares a budget across its
//! service clones. In each non-overlapping ten-second window, retry reservations
//! may not exceed `max(min_retries_per_sec * 10, floor(originals * percent / 100))`.
//! Every original invocation counts once, whether successful or failed; retries
//! and successful responses do not add original volume. A zero floor disables
//! time-only credit, so new original calls are needed to fund retries.
//!
//! Windows reset lazily after ten seconds, discard unused credit, and can allow
//! bursts on either side of a boundary; this is not a rolling window or an RPS
//! limiter. Reservations are charged before backoff and are not refunded on
//! cancellation. This replaces the old unrelated fixed token bucket. The floor
//! is a ten-second allowance (including at startup), not continuous refill.
//!
//! Backoff is `min(initial_backoff_ms * 2^attempt, max_backoff_ms)`, including
//! the first delay. Configuration permits at most 1000 retries and five minutes
//! of cumulative planned backoff. Set `[backends.timeout].seconds` to bound the
//! dispatched operation, including execution, retry readiness and retry sleeps;
//! limiting sleep alone does not bound a pending backend. Cancellation stops
//! further attempts but cannot undo remote side effects.
//! Initial admission/readiness before dispatch is outside that timeout.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "flaky-api"
//! transport = "http"
//! url = "http://localhost:8080"
//!
//! [backends.retry]
//! max_retries = 3
//! initial_backoff_ms = 100
//! max_backoff_ms = 5000
//! budget_percent = 20.0    # retries up to 20% of original calls per window
//! min_retries_per_sec = 10 # floor: 100 retry reservations per 10-second window
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tower_mcp::router::RouterResponse;
use tower_resilience::retry::{FnInterval, RetryBudget, RetryLayer};

use crate::config::RetryConfig;

pub(crate) const MAX_RETRIES: u32 = 1000;
pub(crate) const MAX_PLANNED_BACKOFF_MS: u64 = 300_000;

fn delay_ms(initial: u64, maximum: u64, attempt: usize) -> u64 {
    let multiplier = u32::try_from(attempt)
        .ok()
        .and_then(|attempt| 2_u64.checked_pow(attempt))
        .unwrap_or(u64::MAX);
    initial.saturating_mul(multiplier).min(maximum)
}

fn bounded_retries(config: &RetryConfig) -> u32 {
    let mut total = 0_u64;
    let mut retries = 0;
    for attempt in 0..config.max_retries.min(MAX_RETRIES) {
        let delay = delay_ms(
            config.initial_backoff_ms,
            config.max_backoff_ms,
            attempt as usize,
        );
        let Some(next) = total
            .checked_add(delay)
            .filter(|total| *total <= MAX_PLANNED_BACKOFF_MS)
        else {
            break;
        };
        total = next;
        retries += 1;
    }
    retries
}

pub(crate) fn validate_config(config: &RetryConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.max_retries <= MAX_RETRIES,
        "retry.max_retries must be <= 1000"
    );
    if let Some(percent) = config.budget_percent {
        anyhow::ensure!(
            percent.is_finite() && (0.0..=100.0).contains(&percent),
            "retry.budget_percent must be finite and between 0 and 100"
        );
    }
    anyhow::ensure!(
        bounded_retries(config) == config.max_retries,
        "retry cumulative planned backoff must be <= 300000 ms"
    );
    Ok(())
}

struct VolumeBudget {
    percent: f64,
    floor: u64,
    state: Mutex<Window>,
}
struct Window {
    start: tokio::time::Instant,
    originals: u64,
    reserved: u64,
}
impl Window {
    fn refresh(&mut self) {
        let now = tokio::time::Instant::now();
        if now.duration_since(self.start) >= Duration::from_secs(10) {
            self.start = now;
            self.originals = 0;
            self.reserved = 0;
        }
    }
}
impl VolumeBudget {
    fn new(percent: f64, floor: u32) -> Self {
        Self {
            // Low-level callers can bypass ProxyConfig validation. Invalid
            // percentages earn no volume credit rather than panicking.
            percent: if percent.is_finite() && (0.0..=100.0).contains(&percent) {
                percent
            } else {
                0.0
            },
            floor: u64::from(floor) * 10,
            state: Mutex::new(Window {
                start: tokio::time::Instant::now(),
                originals: 0,
                reserved: 0,
            }),
        }
    }
    fn original(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.refresh();
        state.originals = state.originals.saturating_add(1);
    }
    fn allowance(&self, state: &Window) -> u64 {
        ((state.originals as f64 * self.percent / 100.0).floor() as u64).max(self.floor)
    }
}
impl RetryBudget for VolumeBudget {
    fn try_withdraw(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.refresh();
        if state.reserved >= self.allowance(&state) {
            return false;
        }
        state.reserved += 1;
        true
    }
    // Upstream calls this after a successful response. Originals were already
    // recorded at invocation, so depositing here would count them twice.
    fn deposit(&self) {}
    fn balance(&self) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.refresh();
        usize::try_from(self.allowance(&state).saturating_sub(state.reserved)).unwrap_or(usize::MAX)
    }
}

/// Returns true if a `RouterResponse` contains a retriable MCP error.
fn is_retriable_response(resp: &RouterResponse) -> bool {
    crate::outcome::Outcome::from_response(resp).backend_failure()
}

/// Build a tower-resilience [`RetryLayer`] from our [`RetryConfig`].
///
/// The layer uses a response-based predicate (since MCP services use
/// `Error = Infallible` and encode errors inside `RouterResponse`).
/// Parsed configurations reject excess attempts/planned wait; direct callers
/// bypassing validation are conservatively limited to the same bounds.
pub fn build_retry_layer(
    config: &RetryConfig,
    backend_name: &str,
) -> RetryLayer<tower_mcp::router::RouterRequest, RouterResponse, std::convert::Infallible> {
    let attempts = bounded_retries(config) as usize + 1;
    let initial = config.initial_backoff_ms;
    let maximum = config.max_backoff_ms;
    let mut builder = RetryLayer::builder()
        // max_attempts includes the initial attempt, so max_retries + 1
        .max_attempts(attempts)
        .backoff(FnInterval::new(move |attempt| {
            Duration::from_millis(delay_ms(initial, maximum, attempt))
        }))
        .retry_on_response(is_retriable_response)
        .name(format!("retry-{backend_name}"));

    // Configure budget if percent-based limiting is enabled
    if let Some(percent) = config.budget_percent {
        let budget = Arc::new(VolumeBudget::new(percent, config.min_retries_per_sec));
        let originals = budget.clone();
        builder = builder
            .max_attempts_fn(move |_| {
                originals.original();
                attempts
            })
            .budget(budget);
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetryConfig;
    use crate::test_util::{ErrorMockService, MockService, call_service};
    use tower::Layer;
    use tower_mcp_types::protocol::McpRequest;

    fn make_config(max_retries: u32) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_backoff_ms: 1, // fast for tests
            max_backoff_ms: 10,
            budget_percent: None,
            min_retries_per_sec: 10,
        }
    }

    fn make_config_with_budget(max_retries: u32, budget_percent: f64) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            budget_percent: Some(budget_percent),
            min_retries_per_sec: 0,
        }
    }

    #[tokio::test]
    async fn test_retries_internal_error() {
        // ErrorMockService always returns a -32603 error
        let svc = ErrorMockService;
        let layer = build_retry_layer(&make_config(3), "test");
        let mut svc = layer.layer(svc);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        // Should still get an error (all attempts fail), but it should have retried
        assert!(resp.inner.is_err());
    }

    #[tokio::test]
    async fn test_does_not_retry_success() {
        let svc = MockService::with_tools(&["tool1"]);
        let layer = build_retry_layer(&make_config(3), "test");
        let mut svc = layer.layer(svc);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_response_predicate_matches_transient_errors() {
        // Verify the predicate function directly
        use tower_mcp::protocol::RequestId;
        use tower_mcp_types::JsonRpcError;

        let transient = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32603,
                message: "internal error".to_string(),
                data: None,
            }),
        };
        assert!(is_retriable_response(&transient));

        let server_err = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32000,
                message: "server error".to_string(),
                data: None,
            }),
        };
        assert!(is_retriable_response(&server_err));

        // Client error -- should NOT retry
        let client_err = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32601,
                message: "method not found".to_string(),
                data: None,
            }),
        };
        assert!(!is_retriable_response(&client_err));

        // Success -- should NOT retry
        let success = RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(tower_mcp_types::protocol::McpResponse::ListTools(
                tower_mcp_types::protocol::ListToolsResult {
                    tools: vec![],
                    next_cursor: None,
                    ttl_ms: None,
                    cache_scope: None,
                    meta: None,
                },
            )),
        };
        assert!(!is_retriable_response(&success));
    }

    #[tokio::test]
    async fn test_budget_limits_retries() {
        // With a very small budget, retries should be limited
        let config = make_config_with_budget(10, 1.0); // tiny budget
        let layer = build_retry_layer(&config, "test");

        let svc = ErrorMockService;
        let mut svc = layer.layer(svc);

        // Should still eventually return (budget exhaustion returns the response)
        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_err());
    }

    #[tokio::test]
    async fn test_no_budget_allows_all_retries() {
        let config = make_config(2); // No budget, 2 retries
        let _layer = build_retry_layer(&config, "test");
        // Just verify it builds without a budget
        assert!(config.budget_percent.is_none());
    }

    fn recorded() -> (
        impl tower::Service<
            tower_mcp::router::RouterRequest,
            Response = RouterResponse,
            Error = std::convert::Infallible,
            Future: Send,
        > + Clone,
        Arc<Mutex<Vec<tokio::time::Instant>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        (
            tower::service_fn(move |request: tower_mcp::router::RouterRequest| {
                captured.lock().unwrap().push(tokio::time::Instant::now());
                async move {
                    Ok(RouterResponse {
                        id: request.id,
                        inner: Err(tower_mcp_types::JsonRpcError::internal_error("transient")),
                    })
                }
            }),
            calls,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_progression_and_first_delay_obey_the_cap() {
        for (initial, maximum, expected) in [
            (2, 5, vec![2, 4, 5, 5]),
            (5, 5, vec![5, 5, 5]),
            (100, 5, vec![5, 5]),
        ] {
            let mut config = make_config(expected.len() as u32);
            config.initial_backoff_ms = initial;
            config.max_backoff_ms = maximum;
            let (base, calls) = recorded();
            let mut service = build_retry_layer(&config, "capped").layer(base);
            call_service(&mut service, McpRequest::ListTools(Default::default())).await;
            let actual: Vec<_> = calls
                .lock()
                .unwrap()
                .windows(2)
                .map(|pair| pair[1].duration_since(pair[0]).as_millis())
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn budget_tracks_low_and_high_original_failure_volume() {
        for originals in [20, 100, 1000] {
            let mut config = make_config_with_budget(3, 20.0);
            config.initial_backoff_ms = 0;
            let (base, calls) = recorded();
            let mut service = build_retry_layer(&config, "volume").layer(base);
            for _ in 0..originals {
                call_service(&mut service, McpRequest::ListTools(Default::default())).await;
            }
            assert_eq!(calls.lock().unwrap().len(), originals + originals / 5);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn zero_floor_requires_new_volume_and_windows_discard_credit() {
        let mut config = make_config_with_budget(3, 50.0);
        config.initial_backoff_ms = 0;
        let (base, calls) = recorded();
        let mut service = build_retry_layer(&config, "zero-floor").layer(base);
        for expected in [1, 3] {
            call_service(&mut service, McpRequest::ListTools(Default::default())).await;
            assert_eq!(calls.lock().unwrap().len(), expected);
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        call_service(&mut service, McpRequest::ListTools(Default::default())).await;
        assert_eq!(calls.lock().unwrap().len(), 4, "time alone must not refill");
        tokio::time::advance(Duration::from_secs(10)).await;
        call_service(&mut service, McpRequest::ListTools(Default::default())).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            5,
            "new window starts with one original and zero credit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn floor_is_shared_exhaustible_and_resets_without_continuous_refill() {
        let mut config = make_config_with_budget(3, 0.0);
        config.min_retries_per_sec = 1;
        config.initial_backoff_ms = 0;
        let (base, calls) = recorded();
        let layer = build_retry_layer(&config, "floor");
        let mut service = layer.layer(base.clone());
        let mut other = layer.layer(base);
        for _ in 0..4 {
            call_service(&mut service, McpRequest::ListTools(Default::default())).await;
        }
        assert_eq!(calls.lock().unwrap().len(), 14);
        tokio::time::advance(Duration::from_secs(9)).await;
        call_service(&mut other, McpRequest::ListTools(Default::default())).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            15,
            "cloned layers must not get fresh floor credit"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        call_service(&mut other, McpRequest::ListTools(Default::default())).await;
        assert_eq!(calls.lock().unwrap().len(), 19);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_requests_fund_volume_once_and_not_one_token_each() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let failed = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let flag = failed.clone();
        let count = calls.clone();
        let base = tower::service_fn(move |request: tower_mcp::router::RouterRequest| {
            count.fetch_add(1, Ordering::SeqCst);
            let failure = flag.load(Ordering::SeqCst);
            async move {
                Ok::<_, std::convert::Infallible>(RouterResponse {
                    id: request.id,
                    inner: if failure {
                        Err(tower_mcp_types::JsonRpcError::internal_error("transient"))
                    } else {
                        Ok(tower_mcp_types::protocol::McpResponse::Pong(
                            Default::default(),
                        ))
                    },
                })
            }
        });
        let mut config = make_config_with_budget(50, 20.0);
        config.initial_backoff_ms = 0;
        let mut service = build_retry_layer(&config, "success-volume").layer(base);
        for _ in 0..100 {
            call_service(&mut service, McpRequest::Ping).await;
        }
        failed.store(true, Ordering::SeqCst);
        call_service(&mut service, McpRequest::Ping).await;
        call_service(&mut service, McpRequest::Ping).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            122,
            "102 originals earn exactly 20 retry reservations"
        );
        failed.store(false, Ordering::SeqCst);
        for _ in 0..100 {
            call_service(&mut service, McpRequest::Ping).await;
        }
        failed.store(true, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(10)).await;
        call_service(&mut service, McpRequest::Ping).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            223,
            "unused original-volume credit must expire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_service_clones_share_one_atomic_volume_budget() {
        let mut config = make_config_with_budget(3, 20.0);
        config.initial_backoff_ms = 0;
        let (base, calls) = recorded();
        let service = build_retry_layer(&config, "concurrent").layer(base);
        let mut jobs = Vec::new();
        for _ in 0..100 {
            let mut service = service.clone();
            jobs.push(tokio::spawn(async move {
                call_service(&mut service, McpRequest::Ping).await
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        assert_eq!(calls.lock().unwrap().len(), 120);
    }

    #[tokio::test(start_paused = true)]
    async fn outer_deadline_cancels_backoff_without_later_attempts() {
        let mut config = make_config(10);
        config.initial_backoff_ms = 50;
        config.max_backoff_ms = 50;
        let (base, calls) = recorded();
        let retry = build_retry_layer(&config, "deadline").layer(base);
        let mut service = tower::timeout::Timeout::new(retry, Duration::from_millis(120));
        let request = tower_mcp::router::RouterRequest {
            id: tower_mcp_types::protocol::RequestId::Number(1),
            inner: McpRequest::Ping,
            extensions: Default::default(),
        };
        assert!(
            tower::ServiceExt::oneshot(&mut service, request)
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 3);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(calls.lock().unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn unchecked_extreme_configurations_are_bounded_without_overflow() {
        let mut config = make_config(u32::MAX);
        config.initial_backoff_ms = 0;
        config.max_backoff_ms = u64::MAX;
        let (base, calls) = recorded();
        let mut service = build_retry_layer(&config, "extreme-zero").layer(base);
        call_service(&mut service, McpRequest::Ping).await;
        assert_eq!(calls.lock().unwrap().len(), 1001);
        config.initial_backoff_ms = u64::MAX;
        let (base, calls) = recorded();
        let mut service = build_retry_layer(&config, "extreme-delay").layer(base);
        call_service(&mut service, McpRequest::Ping).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_retry_sleep_does_not_refund_a_reservation() {
        let mut config = make_config_with_budget(20, 0.0);
        config.min_retries_per_sec = 1;
        let (base, calls) = recorded();
        let service = build_retry_layer(&config, "cancelled").layer(base);
        let mut first = service.clone();
        let task = tokio::spawn(async move { call_service(&mut first, McpRequest::Ping).await });
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut next = service;
        call_service(&mut next, McpRequest::Ping).await;
        assert_eq!(
            calls.lock().unwrap().len(),
            11,
            "two originals and nine dispatched retries; one reservation was cancelled"
        );
    }
}
