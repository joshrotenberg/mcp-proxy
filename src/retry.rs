//! Retry middleware for per-backend request retries with exponential backoff.
//!
//! Only retries requests that result in MCP error responses with transient
//! error codes (internal errors, timeouts). Tool-not-found and other client
//! errors are not retried.
//!
//! # Retry Budget
//!
//! When `budget_percent` is configured, retries are capped as a percentage of
//! total request volume over a 10-second rolling window. This prevents retry
//! storms where many failing requests simultaneously retry and overwhelm the
//! backend. A `min_retries_per_sec` floor (default: 10) ensures low-traffic
//! backends can still retry.
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
//! budget_percent = 20.0    # max 20% of requests can be retries
//! min_retries_per_sec = 10 # floor for low-traffic backends
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tower::retry::Policy;
use tower_mcp::router::{RouterRequest, RouterResponse};

use crate::config::RetryConfig;

/// Shared retry budget that tracks requests and retries across a rolling window.
///
/// Used to cap retries as a percentage of total request volume, preventing
/// retry storms during widespread backend failures.
pub struct RetryBudget {
    budget_percent: f64,
    min_retries_per_sec: u32,
    /// Total requests seen in the current window.
    requests: AtomicU64,
    /// Retries issued in the current window.
    retries: AtomicU64,
    /// Window start time as milliseconds since an arbitrary epoch.
    window_start_ms: AtomicU64,
    /// Window duration in milliseconds.
    window_ms: u64,
}

impl RetryBudget {
    /// Create a new retry budget.
    fn new(budget_percent: f64, min_retries_per_sec: u32) -> Self {
        Self {
            budget_percent,
            min_retries_per_sec,
            requests: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            window_start_ms: AtomicU64::new(Self::now_ms()),
            window_ms: 10_000, // 10 second window
        }
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Record that a request was seen. Called for every request attempt.
    pub fn record_request(&self) {
        self.maybe_rotate_window();
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if a retry is allowed and, if so, record it.
    /// Returns `true` if the retry should proceed.
    pub fn allow_retry(&self) -> bool {
        self.maybe_rotate_window();

        let requests = self.requests.load(Ordering::Relaxed);
        let retries = self.retries.load(Ordering::Relaxed);

        // Floor: always allow min_retries_per_sec scaled to window
        let min_retries = (self.min_retries_per_sec as u64 * self.window_ms) / 1000;
        if retries < min_retries {
            self.retries.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        // Budget check: (retries + 1) / (requests + retries + 1) < budget_percent / 100
        // We check what the ratio would be *after* adding this retry.
        let new_retries = retries + 1;
        let new_total = requests + new_retries;
        if new_total == 0 {
            self.retries.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        let retry_ratio = new_retries as f64 / new_total as f64;
        if retry_ratio < self.budget_percent / 100.0 {
            self.retries.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            tracing::debug!(
                requests,
                retries,
                budget_percent = self.budget_percent,
                "Retry budget exhausted"
            );
            false
        }
    }

    /// Rotate the window if it has expired, resetting counters.
    fn maybe_rotate_window(&self) {
        let now = Self::now_ms();
        let start = self.window_start_ms.load(Ordering::Relaxed);
        if now.saturating_sub(start) >= self.window_ms {
            // Try to rotate -- if another thread already did, that's fine
            if self
                .window_start_ms
                .compare_exchange(start, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.requests.store(0, Ordering::Relaxed);
                self.retries.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Get current stats for testing/observability.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.retries.load(Ordering::Relaxed),
        )
    }
}

/// MCP-aware retry policy with exponential backoff and optional retry budget.
#[derive(Clone)]
pub struct McpRetryPolicy {
    max_retries: u32,
    attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    budget: Option<Arc<RetryBudget>>,
}

impl McpRetryPolicy {
    /// Create a new retry policy from config.
    pub fn from_config(config: &RetryConfig) -> Self {
        let budget = config
            .budget_percent
            .map(|percent| Arc::new(RetryBudget::new(percent, config.min_retries_per_sec)));
        Self {
            max_retries: config.max_retries,
            attempts: 0,
            initial_backoff: Duration::from_millis(config.initial_backoff_ms),
            max_backoff: Duration::from_millis(config.max_backoff_ms),
            budget,
        }
    }

    fn backoff_duration(&self) -> Duration {
        let backoff = self.initial_backoff * 2u32.saturating_pow(self.attempts);
        backoff.min(self.max_backoff)
    }
}

/// Returns true if the MCP error code indicates a transient/retriable error.
fn is_retriable_error(code: i32) -> bool {
    // JSON-RPC internal error (-32603) and server errors (-32000 to -32099)
    // are potentially transient. Method not found, invalid params, etc. are not.
    code == -32603 || (-32099..=-32000).contains(&code)
}

impl Policy<RouterRequest, RouterResponse, std::convert::Infallible> for McpRetryPolicy {
    type Future = Pin<Box<dyn Future<Output = ()> + Send>>;

    fn retry(
        &mut self,
        _req: &mut RouterRequest,
        result: &mut Result<RouterResponse, std::convert::Infallible>,
    ) -> Option<Self::Future> {
        // Record the request in the budget (every attempt counts)
        if let Some(ref budget) = self.budget {
            budget.record_request();
        }

        if self.attempts >= self.max_retries {
            return None;
        }

        let resp = result.as_ref().unwrap_or_else(|e| match *e {});
        match &resp.inner {
            Err(err) if is_retriable_error(err.code) => {
                // Check budget before allowing retry
                if let Some(ref budget) = self.budget
                    && !budget.allow_retry()
                {
                    return None;
                }

                self.attempts += 1;
                let delay = self.backoff_duration();
                tracing::debug!(
                    attempt = self.attempts,
                    max = self.max_retries,
                    delay_ms = delay.as_millis(),
                    error_code = err.code,
                    "Retrying MCP request"
                );
                Some(Box::pin(tokio::time::sleep(delay)))
            }
            _ => None,
        }
    }

    fn clone_request(&mut self, req: &RouterRequest) -> Option<RouterRequest> {
        Some(req.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetryConfig;
    use tower_mcp::protocol::RequestId;
    use tower_mcp::router::Extensions;
    use tower_mcp_types::JsonRpcError;
    use tower_mcp_types::protocol::McpRequest;

    fn make_config(max_retries: u32) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_backoff_ms: 10,
            max_backoff_ms: 100,
            budget_percent: None,
            min_retries_per_sec: 10,
        }
    }

    fn make_config_with_budget(max_retries: u32, budget_percent: f64) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_backoff_ms: 10,
            max_backoff_ms: 100,
            budget_percent: Some(budget_percent),
            min_retries_per_sec: 0, // disable floor for deterministic tests
        }
    }

    fn make_request() -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::ListTools(Default::default()),
            extensions: Extensions::new(),
        }
    }

    fn make_error_response(code: i32) -> RouterResponse {
        RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code,
                message: "test error".to_string(),
                data: None,
            }),
        }
    }

    fn make_success_response() -> RouterResponse {
        RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(tower_mcp_types::protocol::McpResponse::ListTools(
                tower_mcp_types::protocol::ListToolsResult {
                    tools: vec![],
                    next_cursor: None,
                    meta: None,
                },
            )),
        }
    }

    #[tokio::test]
    async fn test_retries_internal_error() {
        let mut policy = McpRetryPolicy::from_config(&make_config(3));
        let mut req = make_request();
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));

        let retry = policy.retry(&mut req, &mut result);
        assert!(retry.is_some(), "should retry internal error");
        assert_eq!(policy.attempts, 1);
    }

    #[test]
    fn test_does_not_retry_success() {
        let mut policy = McpRetryPolicy::from_config(&make_config(3));
        let mut req = make_request();
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_success_response());

        let retry = policy.retry(&mut req, &mut result);
        assert!(retry.is_none(), "should not retry success");
    }

    #[test]
    fn test_does_not_retry_client_error() {
        let mut policy = McpRetryPolicy::from_config(&make_config(3));
        let mut req = make_request();
        // Method not found
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32601));

        let retry = policy.retry(&mut req, &mut result);
        assert!(retry.is_none(), "should not retry client errors");
    }

    #[tokio::test]
    async fn test_stops_after_max_retries() {
        let mut policy = McpRetryPolicy::from_config(&make_config(2));
        let mut req = make_request();

        // First retry
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));
        assert!(policy.retry(&mut req, &mut result).is_some());

        // Second retry
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));
        assert!(policy.retry(&mut req, &mut result).is_some());

        // Should stop
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));
        assert!(policy.retry(&mut req, &mut result).is_none());
    }

    #[test]
    fn test_backoff_increases() {
        let policy = McpRetryPolicy {
            max_retries: 5,
            attempts: 0,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(5000),
            budget: None,
        };

        // attempt 0: 100ms
        assert_eq!(policy.backoff_duration(), Duration::from_millis(100));

        let policy2 = McpRetryPolicy {
            attempts: 1,
            ..policy.clone()
        };
        // attempt 1: 200ms
        assert_eq!(policy2.backoff_duration(), Duration::from_millis(200));

        let policy3 = McpRetryPolicy {
            attempts: 3,
            ..policy.clone()
        };
        // attempt 3: 800ms
        assert_eq!(policy3.backoff_duration(), Duration::from_millis(800));

        let policy4 = McpRetryPolicy {
            attempts: 10,
            ..policy
        };
        // attempt 10: would be 102400ms but capped at 5000ms
        assert_eq!(policy4.backoff_duration(), Duration::from_millis(5000));
    }

    #[tokio::test]
    async fn test_retries_server_error_range() {
        let mut policy = McpRetryPolicy::from_config(&make_config(3));
        let mut req = make_request();
        // Server error in -32000 to -32099 range
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32000));

        assert!(
            policy.retry(&mut req, &mut result).is_some(),
            "should retry server errors"
        );
    }

    // ========================================================================
    // Retry budget tests
    // ========================================================================

    #[test]
    fn test_budget_allows_retries_under_limit() {
        let budget = RetryBudget::new(50.0, 0); // 50% budget, no floor

        // Simulate 10 requests
        for _ in 0..10 {
            budget.record_request();
        }

        // Should allow retries while under 50%
        assert!(budget.allow_retry(), "should allow first retry");
        assert!(budget.allow_retry(), "should allow second retry");
        assert!(budget.allow_retry(), "should allow third retry");

        let (requests, retries) = budget.stats();
        assert_eq!(requests, 10);
        assert_eq!(retries, 3);
    }

    #[test]
    fn test_budget_blocks_retries_over_limit() {
        let budget = RetryBudget::new(10.0, 0); // 10% budget, no floor

        // Simulate 10 requests
        for _ in 0..10 {
            budget.record_request();
        }

        // First retry: 1/(10+1) = 9% < 10% -- allowed
        assert!(budget.allow_retry(), "first retry should be allowed");

        // Second retry: 2/(10+2) = 16.7% > 10% -- blocked
        assert!(!budget.allow_retry(), "second retry should be blocked");
    }

    #[test]
    fn test_budget_min_retries_floor() {
        let budget = RetryBudget::new(1.0, 100); // 1% budget, but 100 retries/sec floor

        // No requests at all
        // Floor allows 100/sec * 10sec window = 1000 retries
        assert!(budget.allow_retry(), "floor should allow retry");
        assert!(budget.allow_retry(), "floor should allow retry");
    }

    #[tokio::test]
    async fn test_budget_integrated_with_policy() {
        let config = make_config_with_budget(10, 10.0); // 10% budget
        let mut policy = McpRetryPolicy::from_config(&config);
        let mut req = make_request();

        // Record some requests to establish a baseline
        if let Some(ref budget) = policy.budget {
            for _ in 0..10 {
                budget.record_request();
            }
        }

        // First retry should work (under budget)
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));
        assert!(
            policy.retry(&mut req, &mut result).is_some(),
            "first retry under budget"
        );

        // Second retry -- budget now exceeded (retries/total > 10%)
        let mut result: Result<RouterResponse, std::convert::Infallible> =
            Ok(make_error_response(-32603));
        assert!(
            policy.retry(&mut req, &mut result).is_none(),
            "second retry should be blocked by budget"
        );
    }

    #[test]
    fn test_budget_no_budget_allows_all() {
        let config = make_config(3); // No budget
        let policy = McpRetryPolicy::from_config(&config);
        assert!(policy.budget.is_none());
    }
}
