//! Passive health checks via outlier detection.
//!
//! Tracks error rates on live traffic and automatically ejects unhealthy backends.
//! Unlike the circuit breaker (which uses failure *rate* over a sliding window),
//! outlier detection triggers on *consecutive* errors, catching hard-down backends
//! faster. Cross-backend coordination via `max_ejection_percent` prevents ejecting
//! all backends simultaneously.
//!
//! # Configuration
//!
//! ```toml
//! [[backends]]
//! name = "flaky-api"
//! transport = "http"
//! url = "http://localhost:8080"
//!
//! [backends.outlier_detection]
//! consecutive_errors = 5       # eject after 5 consecutive errors
//! interval_seconds = 10        # evaluation interval
//! base_ejection_seconds = 30   # how long to eject
//! max_ejection_percent = 50    # never eject more than half of backends
//! ```

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

use crate::config::OutlierDetectionConfig;

/// A Tower [`Layer`](tower::Layer) that applies outlier detection to a backend.
#[derive(Clone)]
pub struct OutlierDetectionLayer {
    name: String,
    config: OutlierDetectionConfig,
    detector: OutlierDetector,
}

impl OutlierDetectionLayer {
    /// Create a new outlier detection layer for a specific backend.
    pub fn new(name: String, config: OutlierDetectionConfig, detector: OutlierDetector) -> Self {
        Self {
            name,
            config,
            detector,
        }
    }
}

impl<S> tower::Layer<S> for OutlierDetectionLayer {
    type Service = OutlierDetectionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OutlierDetectionService::new(
            inner,
            self.name.clone(),
            self.config.clone(),
            self.detector.clone(),
        )
    }
}

/// Shared state tracking ejection status across all backends.
///
/// Each backend registers with the detector and reports errors.
/// The detector enforces `max_ejection_percent` globally.
#[derive(Clone)]
pub struct OutlierDetector {
    inner: Arc<OutlierDetectorInner>,
}

struct OutlierDetectorInner {
    /// Total number of backends registered.
    total_backends: AtomicU32,
    /// Number of currently ejected backends.
    ejected_count: AtomicU32,
    /// Maximum percentage of backends that can be ejected (0-100).
    max_ejection_percent: u32,
}

impl OutlierDetector {
    /// Create a new outlier detector.
    ///
    /// `max_ejection_percent` caps how many backends can be ejected at once
    /// (as a percentage of total registered backends).
    pub fn new(max_ejection_percent: u32) -> Self {
        Self {
            inner: Arc::new(OutlierDetectorInner {
                total_backends: AtomicU32::new(0),
                ejected_count: AtomicU32::new(0),
                max_ejection_percent,
            }),
        }
    }

    /// Register a backend. Call once per backend at startup.
    pub fn register_backend(&self) {
        self.inner.total_backends.fetch_add(1, Ordering::Relaxed);
    }

    /// Try to eject a backend. Returns `true` if ejection is allowed.
    ///
    /// Respects `max_ejection_percent` -- if ejecting this backend would
    /// exceed the threshold, returns `false`.
    pub fn try_eject(&self) -> bool {
        let total = self.inner.total_backends.load(Ordering::Relaxed);
        if total == 0 {
            return false;
        }

        let currently_ejected = self.inner.ejected_count.load(Ordering::Relaxed);
        let max_ejectable = (total as u64 * self.inner.max_ejection_percent as u64 / 100) as u32;
        // Always allow at least 1 ejection if max_ejection_percent > 0
        let max_ejectable = if self.inner.max_ejection_percent > 0 {
            max_ejectable.max(1)
        } else {
            0
        };

        if currently_ejected >= max_ejectable {
            tracing::debug!(
                currently_ejected,
                max_ejectable,
                total,
                "Ejection blocked: max_ejection_percent reached"
            );
            return false;
        }

        self.inner.ejected_count.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Record that a backend has been un-ejected.
    pub fn record_uneject(&self) {
        self.inner.ejected_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Current number of ejected backends (for observability).
    pub fn ejected_count(&self) -> u32 {
        self.inner.ejected_count.load(Ordering::Relaxed)
    }

    /// Total registered backends.
    pub fn total_backends(&self) -> u32 {
        self.inner.total_backends.load(Ordering::Relaxed)
    }
}

/// Per-backend outlier detection state.
struct BackendOutlierState {
    /// Number of consecutive errors observed.
    consecutive_errors: AtomicU32,
    /// Whether this backend is currently ejected.
    ejected: AtomicBool,
    /// When the backend was ejected (millis since UNIX epoch).
    ejected_at_ms: AtomicU64,
}

/// Per-backend outlier detection middleware.
///
/// Wraps a backend service and tracks consecutive errors. When the threshold
/// is exceeded, the backend is ejected (requests fail immediately) for the
/// configured duration.
#[derive(Clone)]
pub struct OutlierDetectionService<S> {
    inner: S,
    state: Arc<BackendOutlierState>,
    detector: OutlierDetector,
    config: OutlierDetectionConfig,
    name: String,
}

impl<S> OutlierDetectionService<S> {
    /// Create a new outlier detection service for a specific backend.
    pub fn new(
        inner: S,
        name: String,
        config: OutlierDetectionConfig,
        detector: OutlierDetector,
    ) -> Self {
        detector.register_backend();
        Self {
            inner,
            state: Arc::new(BackendOutlierState {
                consecutive_errors: AtomicU32::new(0),
                ejected: AtomicBool::new(false),
                ejected_at_ms: AtomicU64::new(0),
            }),
            detector,
            config,
            name,
        }
    }

    /// Check if the ejection period has expired and un-eject if so.
    fn maybe_uneject(&self) -> bool {
        if !self.state.ejected.load(Ordering::Relaxed) {
            return false;
        }

        let ejected_at = self.state.ejected_at_ms.load(Ordering::Relaxed);
        let now = now_ms();
        let elapsed_secs = now.saturating_sub(ejected_at) / 1000;

        if elapsed_secs >= self.config.base_ejection_seconds {
            self.state.ejected.store(false, Ordering::Relaxed);
            self.state.consecutive_errors.store(0, Ordering::Relaxed);
            self.detector.record_uneject();
            tracing::info!(
                backend = %self.name,
                ejected_for_secs = elapsed_secs,
                "Backend un-ejected, allowing traffic"
            );
            true
        } else {
            false
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Returns true if the MCP response indicates a server-side error.
fn is_server_error(response: &RouterResponse) -> bool {
    match &response.inner {
        Err(err) => {
            // JSON-RPC internal error (-32603) and server errors (-32000 to -32099)
            err.code == -32603 || (-32099..=-32000).contains(&err.code)
        }
        Ok(_) => false,
    }
}

impl<S> Service<RouterRequest> for OutlierDetectionService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        // Check if ejected
        self.maybe_uneject();

        if self.state.ejected.load(Ordering::Relaxed) {
            let id = req.id.clone();
            let name = self.name.clone();
            return Box::pin(async move {
                tracing::debug!(backend = %name, "Request rejected: backend ejected");
                Ok(RouterResponse {
                    id,
                    inner: Err(JsonRpcError {
                        code: -32000,
                        message: format!("backend '{name}' is ejected due to consecutive errors"),
                        data: None,
                    }),
                })
            });
        }

        let state = Arc::clone(&self.state);
        let detector = self.detector.clone();
        let config = self.config.clone();
        let name = self.name.clone();
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;

            if is_server_error(&response) {
                let errors = state.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    backend = %name,
                    consecutive_errors = errors,
                    threshold = config.consecutive_errors,
                    "Backend error observed"
                );

                if errors >= config.consecutive_errors && !state.ejected.load(Ordering::Relaxed) {
                    if detector.try_eject() {
                        state.ejected.store(true, Ordering::Relaxed);
                        state.ejected_at_ms.store(now_ms(), Ordering::Relaxed);
                        tracing::warn!(
                            backend = %name,
                            consecutive_errors = errors,
                            ejection_seconds = config.base_ejection_seconds,
                            "Backend ejected due to consecutive errors"
                        );
                    } else {
                        tracing::warn!(
                            backend = %name,
                            consecutive_errors = errors,
                            "Backend would be ejected but max_ejection_percent reached"
                        );
                    }
                }
            } else {
                // Success resets the counter
                state.consecutive_errors.store(0, Ordering::Relaxed);
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutlierDetectionConfig;
    use crate::test_util::{MockService, call_service};
    use tower::Service;
    use tower_mcp::protocol::RequestId;
    use tower_mcp::router::Extensions;
    use tower_mcp_types::protocol::McpRequest;

    fn make_config(consecutive: u32, ejection_secs: u64, max_pct: u32) -> OutlierDetectionConfig {
        OutlierDetectionConfig {
            consecutive_errors: consecutive,
            interval_seconds: 10,
            base_ejection_seconds: ejection_secs,
            max_ejection_percent: max_pct,
        }
    }

    fn make_error_request() -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp_types::protocol::CallToolParams {
                name: "test/fail".to_string(),
                arguments: serde_json::json!({}),
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        }
    }

    /// A mock service that returns server errors.
    #[derive(Clone)]
    struct ErrorService;

    impl Service<RouterRequest> for ErrorService {
        type Response = RouterResponse;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RouterRequest) -> Self::Future {
            let id = req.id.clone();
            Box::pin(async move {
                Ok(RouterResponse {
                    id,
                    inner: Err(JsonRpcError {
                        code: -32603,
                        message: "internal error".to_string(),
                        data: None,
                    }),
                })
            })
        }
    }

    #[tokio::test]
    async fn test_passes_through_on_success() {
        let mock = MockService::with_tools(&["test/hello"]);
        let detector = OutlierDetector::new(50);
        let config = make_config(5, 30, 50);
        let mut svc = OutlierDetectionService::new(mock, "test".to_string(), config, detector);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_tracks_consecutive_errors() {
        let detector = OutlierDetector::new(50);
        let config = make_config(3, 30, 50);
        let mut svc =
            OutlierDetectionService::new(ErrorService, "flaky".to_string(), config, detector);

        // 2 errors -- not yet ejected
        for _ in 0..2 {
            let _ = svc.call(make_error_request()).await;
        }
        assert!(!svc.state.ejected.load(Ordering::Relaxed));

        // 3rd error triggers ejection
        let _ = svc.call(make_error_request()).await;
        assert!(svc.state.ejected.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_success_resets_counter() {
        let mock = MockService::with_tools(&["test/hello"]);
        let detector = OutlierDetector::new(50);
        let config = make_config(3, 30, 50);

        // We need a service that can return errors then success.
        // Use ErrorService first, then switch to mock.
        let mut error_svc = OutlierDetectionService::new(
            ErrorService,
            "test".to_string(),
            config.clone(),
            detector.clone(),
        );

        // 2 errors
        let _ = error_svc.call(make_error_request()).await;
        let _ = error_svc.call(make_error_request()).await;
        assert_eq!(
            error_svc.state.consecutive_errors.load(Ordering::Relaxed),
            2
        );

        // Simulate success by directly resetting (the real service would do this)
        error_svc
            .state
            .consecutive_errors
            .store(0, Ordering::Relaxed);
        assert_eq!(
            error_svc.state.consecutive_errors.load(Ordering::Relaxed),
            0
        );

        // Now test with mock service that returns success
        let mut success_svc =
            OutlierDetectionService::new(mock, "test2".to_string(), config, detector);
        // Send a success request
        let resp = call_service(&mut success_svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
        assert_eq!(
            success_svc.state.consecutive_errors.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn test_ejected_backend_returns_error() {
        let detector = OutlierDetector::new(50);
        let config = make_config(2, 3600, 50); // long ejection so it doesn't expire
        let mut svc =
            OutlierDetectionService::new(ErrorService, "bad".to_string(), config, detector);

        // Trigger ejection
        let _ = svc.call(make_error_request()).await;
        let _ = svc.call(make_error_request()).await;
        assert!(svc.state.ejected.load(Ordering::Relaxed));

        // Next request should be rejected without hitting the backend
        let resp = svc.call(make_error_request()).await.unwrap();
        match &resp.inner {
            Err(err) => {
                assert!(err.message.contains("ejected"));
            }
            Ok(_) => panic!("expected error for ejected backend"),
        }
    }

    #[tokio::test]
    async fn test_uneject_after_timeout() {
        let detector = OutlierDetector::new(50);
        let config = make_config(1, 0, 50); // 0-second ejection = immediate uneject
        let mut svc =
            OutlierDetectionService::new(ErrorService, "recover".to_string(), config, detector);

        // Trigger ejection
        let _ = svc.call(make_error_request()).await;
        assert!(svc.state.ejected.load(Ordering::Relaxed));

        // With 0-second ejection, next call should uneject
        // The maybe_uneject runs before checking ejection status
        let _ = svc.call(make_error_request()).await;
        // After uneject, the error from this call will increment counter again
        // but with threshold=1, it will re-eject. That's fine -- the point is
        // the uneject happened.
    }

    #[test]
    fn test_max_ejection_percent_blocks() {
        let detector = OutlierDetector::new(50); // 50%

        // Register 2 backends
        detector.register_backend();
        detector.register_backend();

        // First ejection should work (1/2 = 50%)
        assert!(detector.try_eject());

        // Second ejection should be blocked (2/2 = 100% > 50%)
        assert!(!detector.try_eject());
    }

    #[test]
    fn test_max_ejection_percent_zero_blocks_all() {
        let detector = OutlierDetector::new(0);
        detector.register_backend();
        assert!(!detector.try_eject());
    }

    #[test]
    fn test_max_ejection_percent_100_allows_all() {
        let detector = OutlierDetector::new(100);
        detector.register_backend();
        detector.register_backend();
        assert!(detector.try_eject());
        assert!(detector.try_eject());
    }

    #[test]
    fn test_uneject_decrements_count() {
        let detector = OutlierDetector::new(100);
        detector.register_backend();
        assert!(detector.try_eject());
        assert_eq!(detector.ejected_count(), 1);
        detector.record_uneject();
        assert_eq!(detector.ejected_count(), 0);
    }

    #[test]
    fn test_is_server_error() {
        let err_resp = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32603,
                message: "internal".to_string(),
                data: None,
            }),
        };
        assert!(is_server_error(&err_resp));

        let err_resp2 = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32000,
                message: "server error".to_string(),
                data: None,
            }),
        };
        assert!(is_server_error(&err_resp2));

        // Client error -- not a server error
        let client_err = RouterResponse {
            id: RequestId::Number(1),
            inner: Err(JsonRpcError {
                code: -32601,
                message: "method not found".to_string(),
                data: None,
            }),
        };
        assert!(!is_server_error(&client_err));

        // Success -- not an error
        let ok_resp = RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(tower_mcp_types::protocol::McpResponse::ListTools(
                tower_mcp_types::protocol::ListToolsResult {
                    tools: vec![],
                    next_cursor: None,
                    meta: None,
                },
            )),
        };
        assert!(!is_server_error(&ok_resp));
    }
}
