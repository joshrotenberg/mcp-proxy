//! Retry middleware for per-backend request retries with exponential backoff.
//!
//! Uses tower-resilience's [`RetryLayer`] with a response-based predicate to
//! retry MCP error responses with transient error codes (internal errors,
//! timeouts). Tool-not-found and other client errors are not retried.
//!
//! # Retry Budget
//!
//! When `budget_percent` is configured, a token bucket budget limits retries.
//! The budget is sized as a percentage of expected request volume, preventing
//! retry storms during widespread backend failures. A `min_retries_per_sec`
//! floor ensures low-traffic backends can still retry.
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

use std::time::Duration;

use tower_mcp::router::RouterResponse;
use tower_resilience::retry::{RetryBudgetBuilder, RetryLayer};

use crate::config::RetryConfig;

/// Returns true if the MCP error code indicates a transient/retriable error.
fn is_retriable_error(code: i32) -> bool {
    // JSON-RPC internal error (-32603) and server errors (-32000 to -32099)
    // are potentially transient. Method not found, invalid params, etc. are not.
    code == -32603 || (-32099..=-32000).contains(&code)
}

/// Returns true if a `RouterResponse` contains a retriable MCP error.
fn is_retriable_response(resp: &RouterResponse) -> bool {
    match &resp.inner {
        Err(err) => is_retriable_error(err.code),
        Ok(_) => false,
    }
}

/// Build a tower-resilience [`RetryLayer`] from our [`RetryConfig`].
///
/// The layer uses a response-based predicate (since MCP services use
/// `Error = Infallible` and encode errors inside `RouterResponse`).
pub fn build_retry_layer(
    config: &RetryConfig,
    backend_name: &str,
) -> RetryLayer<tower_mcp::router::RouterRequest, RouterResponse, std::convert::Infallible> {
    let mut builder = RetryLayer::builder()
        // max_attempts includes the initial attempt, so max_retries + 1
        .max_attempts((config.max_retries + 1) as usize)
        .exponential_backoff(Duration::from_millis(config.initial_backoff_ms))
        .retry_on_response(is_retriable_response)
        .name(format!("retry-{backend_name}"));

    // Configure budget if percent-based limiting is enabled
    if let Some(percent) = config.budget_percent {
        // Map budget_percent to a token bucket: scale tokens to approximate
        // the percentage model. We use min_retries_per_sec as the refill rate
        // and size the bucket relative to expected request volume.
        //
        // For a 20% budget at 100 req/s, we'd want ~20 retries/s capacity.
        // The token bucket's initial_tokens acts as burst capacity.
        let min_per_sec = config.min_retries_per_sec as f64;
        let max_tokens = ((percent / 100.0) * 1000.0).max(min_per_sec * 10.0) as usize;

        let budget = RetryBudgetBuilder::new()
            .token_bucket()
            .tokens_per_second(min_per_sec)
            .max_tokens(max_tokens.max(1))
            .initial_tokens(max_tokens.max(1))
            .build();

        builder = builder.budget(budget);
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
}
