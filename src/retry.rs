//! Retry middleware for per-backend request retries with exponential backoff.
//!
//! Only retries requests that result in MCP error responses with transient
//! error codes (internal errors, timeouts). Tool-not-found and other client
//! errors are not retried.
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
//! ```

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tower::retry::Policy;
use tower_mcp::router::{RouterRequest, RouterResponse};

use crate::config::RetryConfig;

/// MCP-aware retry policy with exponential backoff.
#[derive(Clone)]
pub struct McpRetryPolicy {
    max_retries: u32,
    attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl McpRetryPolicy {
    /// Create a new retry policy from config.
    pub fn from_config(config: &RetryConfig) -> Self {
        Self {
            max_retries: config.max_retries,
            attempts: 0,
            initial_backoff: Duration::from_millis(config.initial_backoff_ms),
            max_backoff: Duration::from_millis(config.max_backoff_ms),
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
        if self.attempts >= self.max_retries {
            return None;
        }

        let resp = result.as_ref().unwrap_or_else(|e| match *e {});
        match &resp.inner {
            Err(err) if is_retriable_error(err.code) => {
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
}
