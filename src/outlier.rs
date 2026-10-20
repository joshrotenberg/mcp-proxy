//! Passive health checks via outlier detection.
//!
//! Tracks error rates on live traffic and automatically ejects unhealthy backends.
//! Unlike the circuit breaker (which uses failure *rate* over a sliding window),
//! outlier detection triggers on *consecutive* errors, catching hard-down backends
//! faster. The shared [`crate::outcome`] policy counts server/transport failures;
//! tool failures and known client/protocol errors do not trigger ejection.
//! Cross-backend coordination via `max_ejection_percent` bounds new ejections
//! across registered backends.
//!
//! # Concurrency and lifetime
//!
//! A short synchronous lock serializes fleet admission and per-backend state;
//! no lock is held over backend I/O, readiness, tracing or an await. Every new
//! service registers one owner. Service clones and dispatched futures share its
//! lifetime; final drop unregisters it and releases any outstanding ejection.
//! Reusing a namespace creates independent state. Pending calls retain the old
//! owner until completion/cancellation; removing an entry cannot cancel remote
//! side effects or make its in-flight work disappear from accounting.
//!
//! Recovery uses monotonic time and occurs on the next call at/after the ejection
//! duration. One caller releases the reservation and resets the error streak.
//! Responses admitted before that recovery cannot alter the new generation.
//! Already-admitted responses while ejected are ignored for health accounting;
//! their original results still reach their callers. There is no background
//! evaluation/probe timer; `interval_seconds` is a compatibility setting.
//!
//! The quota is `floor(registered * percent / 100)`, with one slot minimum for
//! any nonzero percentage and a nonempty fleet. Zero disables ejection; direct
//! detector percentages above 100 are clamped. Removing a healthy owner can
//! shrink the quota below existing ejections: those retain their deadlines and
//! new admissions are blocked until capacity is available. Quotas are per
//! detector, not coordinated across replicas. Configured proxies register only
//! outlier-enabled backends and use their largest configured percentage.
//!
//! Low-level `register_backend`/`try_eject`/`record_uneject` remain anonymous
//! manual accounting APIs. Manual registrations persist for the detector's
//! lifetime; do not double-register middleware. Extra manual releases are safe
//! and cannot steal middleware-owned reservations. Existing service/layer public
//! signatures and configuration keys are unchanged.
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
//! interval_seconds = 10        # compatibility setting; no background evaluator
//! base_ejection_seconds = 30   # how long to eject
//! max_ejection_percent = 50    # new-ejection quota; nonzero allows one slot minimum
//! ```

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Instant;

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
    state: Mutex<FleetState>,
    /// Maximum percentage of backends that can be ejected (0-100).
    max_ejection_percent: u32,
}

#[derive(Default)]
struct FleetState {
    next_id: u64,
    backends: HashMap<u64, BackendStatus>,
    ejected: usize,
    // Compatibility APIs manage anonymous registrations separately: an extra
    // manual release must never consume a middleware-owned reservation.
    manual_backends: u32,
    manual_ejected: u32,
}

impl FleetState {
    fn total(&self) -> u128 {
        self.backends.len() as u128 + u128::from(self.manual_backends)
    }

    fn ejected(&self) -> u128 {
        self.ejected as u128 + u128::from(self.manual_ejected)
    }

    fn allows_ejection(&self, percent: u32) -> bool {
        let quota = (self.total() * u128::from(percent) / 100)
            .max(u128::from(percent > 0 && self.total() > 0));
        self.ejected() < quota
    }
}

struct BackendStatus {
    consecutive_errors: u32,
    ejected_at: Option<Instant>,
    // Pointer identity cannot wrap or be reused while an old call retains it.
    generation: Arc<()>,
}

impl OutlierDetector {
    /// Create a new outlier detector.
    ///
    /// `max_ejection_percent` bounds new ejection admissions relative to
    /// registered backends. Values above 100
    /// are clamped; nonzero percentages allow at least one ejection.
    pub fn new(max_ejection_percent: u32) -> Self {
        Self {
            inner: Arc::new(OutlierDetectorInner {
                state: Mutex::new(FleetState::default()),
                max_ejection_percent: max_ejection_percent.min(100),
            }),
        }
    }

    /// Register an anonymous backend for the manual accounting API.
    ///
    /// Manual registrations persist for this detector's lifetime. Middleware
    /// registers its own owned lifetime automatically; do not also register it
    /// here. Pair manual [`Self::try_eject`] calls with [`Self::record_uneject`].
    /// Panics if more than `u32::MAX` manual registrations are requested.
    pub fn register_backend(&self) {
        let mut fleet = self.lock();
        fleet.manual_backends = fleet
            .manual_backends
            .checked_add(1)
            .expect("too many manual outlier registrations");
    }

    /// Reserve one anonymous manual ejection. Returns whether it is allowed.
    ///
    /// Respects `max_ejection_percent` -- if ejecting this backend would
    /// exceed the threshold, returns `false`.
    pub fn try_eject(&self) -> bool {
        let mut fleet = self.lock();
        if fleet.manual_ejected >= fleet.manual_backends
            || !fleet.allows_ejection(self.inner.max_ejection_percent)
        {
            return false;
        }
        fleet.manual_ejected += 1;
        true
    }

    /// Release one manual ejection. Extra releases are harmless and cannot
    /// consume middleware-owned reservations.
    pub fn record_uneject(&self) {
        let mut fleet = self.lock();
        fleet.manual_ejected = fleet.manual_ejected.saturating_sub(1);
    }

    /// Current manual plus owned ejections (saturates at `u32::MAX`).
    pub fn ejected_count(&self) -> u32 {
        u32::try_from(self.lock().ejected()).unwrap_or(u32::MAX)
    }

    /// Current owned plus lifetime manual registrations (saturates at `u32::MAX`).
    pub fn total_backends(&self) -> u32 {
        u32::try_from(self.lock().total()).unwrap_or(u32::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, FleetState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register_owned(&self) -> u64 {
        let mut fleet = self.lock();
        let id = fleet.next_id;
        fleet.next_id = id
            .checked_add(1)
            .expect("outlier registration IDs exhausted");
        fleet.backends.insert(
            id,
            BackendStatus {
                consecutive_errors: 0,
                ejected_at: None,
                generation: Arc::new(()),
            },
        );
        id
    }
}

/// Shared registration lease, retained by service clones and dispatched calls.
struct BackendOutlierState {
    id: u64,
    detector: OutlierDetector,
    config: OutlierDetectionConfig,
    name: String,
}

impl Drop for BackendOutlierState {
    fn drop(&mut self) {
        let mut fleet = self.detector.lock();
        if let Some(backend) = fleet.backends.remove(&self.id)
            && backend.ejected_at.is_some()
        {
            fleet.ejected -= 1;
        }
    }
}

impl BackendOutlierState {
    fn admit(&self) -> Option<Arc<()>> {
        let (generation, recovered) = {
            let mut fleet = self.detector.lock();
            let mut recovered = None;
            let generation = {
                let backend = fleet.backends.get_mut(&self.id).expect("live registration");
                if let Some(since) = backend.ejected_at {
                    let elapsed = Instant::now().saturating_duration_since(since);
                    if elapsed < Duration::from_secs(self.config.base_ejection_seconds) {
                        return None;
                    }
                    backend.ejected_at = None;
                    backend.consecutive_errors = 0;
                    backend.generation = Arc::new(());
                    recovered = Some(elapsed);
                }
                backend.generation.clone()
            };
            if recovered.is_some() {
                fleet.ejected -= 1;
            }
            (generation, recovered)
        };
        if let Some(elapsed) = recovered {
            tracing::info!(backend = %self.name, ejected_for_secs = elapsed.as_secs(),
                "Backend un-ejected, allowing traffic");
        }
        Some(generation)
    }

    fn record(&self, generation: &Arc<()>, failed: bool) {
        let (errors, ejected) = {
            let mut fleet = self.detector.lock();
            let errors = {
                let backend = fleet.backends.get_mut(&self.id).expect("live registration");
                if backend.ejected_at.is_some() || !Arc::ptr_eq(&backend.generation, generation) {
                    return; // A response admitted before ejection/recovery is stale.
                }
                if !failed {
                    backend.consecutive_errors = 0;
                    return;
                }
                backend.consecutive_errors = backend.consecutive_errors.saturating_add(1);
                backend.consecutive_errors
            };
            let ejected = errors >= self.config.consecutive_errors
                && fleet.allows_ejection(self.detector.inner.max_ejection_percent);
            if ejected {
                fleet.backends.get_mut(&self.id).unwrap().ejected_at = Some(Instant::now());
                fleet.ejected += 1;
            }
            (errors, ejected)
        };
        tracing::debug!(backend = %self.name, consecutive_errors = errors,
            threshold = self.config.consecutive_errors, "Backend error observed");
        if ejected {
            tracing::warn!(backend = %self.name, consecutive_errors = errors,
                ejection_seconds = self.config.base_ejection_seconds,
                "Backend ejected due to consecutive errors");
        } else if errors >= self.config.consecutive_errors {
            tracing::warn!(backend = %self.name, consecutive_errors = errors,
                "Backend would be ejected but max_ejection_percent reached");
        }
    }
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
}

impl<S> OutlierDetectionService<S> {
    /// Create a new outlier detection service for a specific backend.
    pub fn new(
        inner: S,
        name: String,
        config: OutlierDetectionConfig,
        detector: OutlierDetector,
    ) -> Self {
        let id = detector.register_owned();
        Self {
            inner,
            state: Arc::new(BackendOutlierState {
                id,
                detector,
                config,
                name,
            }),
        }
    }
}

/// Returns true if the MCP response indicates a server-side error.
fn is_server_error(response: &RouterResponse) -> bool {
    crate::outcome::Outcome::from_response(response).backend_failure()
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
        let Some(generation) = self.state.admit() else {
            let id = req.id.clone();
            let name = self.state.name.clone();
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
        };

        let state = Arc::clone(&self.state);
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;

            state.record(&generation, is_server_error(&response));

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutlierDetectionConfig;
    use crate::test_util::{MockService, call_service};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::Service;
    use tower_mcp::protocol::RequestId;
    use tower_mcp::router::Extensions;
    use tower_mcp_types::protocol::McpRequest;

    fn switchable() -> (
        impl Service<RouterRequest, Response = RouterResponse, Error = Infallible, Future: Send> + Clone,
        Arc<AtomicBool>,
    ) {
        let failed = Arc::new(AtomicBool::new(true));
        let flag = failed.clone();
        let backend = tower::service_fn(move |request: RouterRequest| {
            std::future::ready(Ok::<_, Infallible>(RouterResponse {
                id: request.id,
                inner: if flag.load(Ordering::SeqCst) {
                    Err(JsonRpcError::internal_error("transient"))
                } else {
                    Ok(tower_mcp::McpResponse::Pong(Default::default()))
                },
            }))
        });
        (backend, failed)
    }

    fn status<S>(service: &OutlierDetectionService<S>) -> (u32, bool) {
        let fleet = service.state.detector.lock();
        let backend = &fleet.backends[&service.state.id];
        (backend.consecutive_errors, backend.ejected_at.is_some())
    }

    fn assert_counts(detector: &OutlierDetector, total: u32, ejected: u32) {
        let fleet = detector.lock();
        assert_eq!(fleet.total(), u128::from(total));
        assert_eq!(fleet.ejected(), u128::from(ejected));
        assert!(fleet.manual_ejected <= fleet.manual_backends);
        assert_eq!(
            fleet.ejected,
            fleet
                .backends
                .values()
                .filter(|b| b.ejected_at.is_some())
                .count()
        );
        assert!(fleet.ejected() <= fleet.total());
    }

    fn finish(
        mut future: <OutlierDetectionService<ErrorService> as Service<RouterRequest>>::Future,
    ) -> RouterResponse {
        let mut context = Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(response)) => response,
            _ => panic!("controlled immediate backend must finish in one poll"),
        }
    }

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
                input_responses: None,
                request_state: None,
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
        assert!(!status(&svc).1);

        // 3rd error triggers ejection
        let _ = svc.call(make_error_request()).await;
        assert!(status(&svc).1);
    }

    #[tokio::test]
    async fn test_success_resets_counter() {
        let (backend, failed) = switchable();
        let detector = OutlierDetector::new(50);
        let mut service =
            OutlierDetectionService::new(backend, "test".into(), make_config(3, 30, 50), detector);
        service.call(make_error_request()).await.unwrap();
        service.call(make_error_request()).await.unwrap();
        assert_eq!(status(&service), (2, false));
        failed.store(false, Ordering::SeqCst);
        let resp = service.call(make_error_request()).await.unwrap();
        assert!(resp.inner.is_ok());
        assert_eq!(status(&service), (0, false));
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
        assert!(status(&svc).1);

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
        assert!(status(&svc).1);

        // With 0-second ejection, next call should uneject
        // Admission performs recovery before checking ejection status.
        let _ = svc.call(make_error_request()).await;
        assert!(status(&svc).1);
        assert_eq!(svc.state.detector.ejected_count(), 1);
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
    fn competing_fleet_ejections_share_one_atomic_reservation() {
        let detector = OutlierDetector::new(1);
        let mut services: Vec<_> = (0..100)
            .map(|index| {
                OutlierDetectionService::new(
                    ErrorService,
                    index.to_string(),
                    make_config(1, 3600, 1),
                    detector.clone(),
                )
            })
            .collect();
        // Admit every request before releasing any backend response. No winner
        // or thread execution order is assumed.
        let pending: Vec<_> = services
            .iter_mut()
            .map(|s| s.call(make_error_request()))
            .collect();
        let barrier = Arc::new(std::sync::Barrier::new(pending.len()));
        std::thread::scope(|scope| {
            for future in pending {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    assert!(finish(future).inner.is_err());
                });
            }
        });
        assert_counts(&detector, 100, 1);
        assert_eq!(services.iter().filter(|s| status(s).1).count(), 1);
        drop(services);
        assert_counts(&detector, 0, 0);
    }

    #[test]
    fn competing_responses_eject_one_backend_only_once() {
        let detector = OutlierDetector::new(100);
        for _ in 0..99 {
            detector.register_backend();
        }
        let mut service = OutlierDetectionService::new(
            ErrorService,
            "same".into(),
            make_config(1, 3600, 100),
            detector.clone(),
        );
        let pending: Vec<_> = (0..32)
            .map(|_| service.call(make_error_request()))
            .collect();
        let barrier = Arc::new(std::sync::Barrier::new(pending.len()));
        std::thread::scope(|scope| {
            for future in pending {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    finish(future);
                });
            }
        });
        assert_counts(&detector, 100, 1);
        assert_eq!(status(&service), (1, true));
        detector.record_uneject(); // Cannot steal a managed reservation.
        assert_counts(&detector, 100, 1);
        drop(service);
        assert_counts(&detector, 99, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_recovery_releases_once_at_the_monotonic_deadline() {
        let detector = OutlierDetector::new(100);
        let service = OutlierDetectionService::new(
            ErrorService,
            "recovery".into(),
            make_config(1, 10, 100),
            detector.clone(),
        );
        let mut original = service.clone();
        original.call(make_error_request()).await.unwrap();
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(service.state.admit().is_none());
        assert_counts(&detector, 1, 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        let handle = tokio::runtime::Handle::current();
        let barrier = Arc::new(std::sync::Barrier::new(32));
        let generations = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..32)
                .map(|_| {
                    let handle = handle.clone();
                    let barrier = barrier.clone();
                    let state = service.state.clone();
                    scope.spawn(move || {
                        let _runtime = handle.enter();
                        barrier.wait();
                        state.admit().expect("deadline expired")
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(generations.iter().all(|g| Arc::ptr_eq(g, &generations[0])));
        assert_eq!(status(&service), (0, false));
        assert_counts(&detector, 1, 0);
        original.call(make_error_request()).await.unwrap();
        assert_counts(&detector, 1, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_successes_and_failures_cannot_change_a_recovered_generation() {
        let detector = OutlierDetector::new(100);
        let (backend, failed) = switchable();
        let mut service = OutlierDetectionService::new(
            backend,
            "stale".into(),
            make_config(2, 10, 100),
            detector.clone(),
        );
        failed.store(false, Ordering::SeqCst);
        let stale_success = service.call(make_error_request());
        failed.store(true, Ordering::SeqCst);
        let stale_failure = service.call(make_error_request());
        service.call(make_error_request()).await.unwrap();
        service.call(make_error_request()).await.unwrap();
        assert_counts(&detector, 1, 1);
        tokio::time::advance(Duration::from_secs(10)).await;
        service.call(make_error_request()).await.unwrap();
        assert_eq!(status(&service), (1, false));
        assert!(stale_success.await.unwrap().inner.is_ok());
        assert_eq!(
            status(&service),
            (1, false),
            "stale success reset the new streak"
        );
        assert!(stale_failure.await.unwrap().inner.is_err());
        assert_eq!(status(&service), (1, false));
        assert_counts(&detector, 1, 0);
        service.call(make_error_request()).await.unwrap();
        assert_counts(&detector, 1, 1);
    }

    #[tokio::test]
    async fn clones_and_pending_calls_retain_then_release_replaced_registrations() {
        let detector = OutlierDetector::new(100);
        let config = make_config(1, u64::MAX, 100);
        let mut old = OutlierDetectionService::new(
            ErrorService,
            "api".into(),
            config.clone(),
            detector.clone(),
        );
        let clone = old.clone();
        let pending = old.call(make_error_request());
        old.call(make_error_request()).await.unwrap();
        drop(old);
        drop(clone);
        assert_counts(&detector, 1, 1);
        let mut replacement =
            OutlierDetectionService::new(ErrorService, "api".into(), config, detector.clone());
        replacement.call(make_error_request()).await.unwrap();
        assert_counts(&detector, 2, 2);
        pending.await.unwrap();
        assert_counts(&detector, 1, 1);
        drop(replacement);
        assert_counts(&detector, 0, 0);

        let mut cancelled = OutlierDetectionService::new(
            ErrorService,
            "cancelled".into(),
            make_config(1, 60, 100),
            detector.clone(),
        );
        let pending = cancelled.call(make_error_request());
        cancelled.call(make_error_request()).await.unwrap();
        drop(cancelled);
        assert_counts(&detector, 1, 1);
        drop(pending);
        assert_counts(&detector, 0, 0);
    }

    #[tokio::test]
    async fn shrinking_fleet_preserves_existing_ejections_and_blocks_new_admissions() {
        let detector = OutlierDetector::new(50);
        let mut services: Vec<_> = (0..4)
            .map(|index| {
                OutlierDetectionService::new(
                    ErrorService,
                    index.to_string(),
                    make_config(1, 3600, 50),
                    detector.clone(),
                )
            })
            .collect();
        services[0].call(make_error_request()).await.unwrap();
        services[1].call(make_error_request()).await.unwrap();
        assert_counts(&detector, 4, 2);
        services.pop(); // Healthy removal reduces the new-admission quota.
        services[2].call(make_error_request()).await.unwrap();
        assert_counts(&detector, 3, 2);
        assert!(!status(&services[2]).1);
        services.remove(0); // Ejected removal releases its reservation.
        assert_counts(&detector, 2, 1);
        drop(services);
        assert_counts(&detector, 0, 0);
    }

    #[test]
    fn manual_accounting_bounds_and_extra_releases_are_safe() {
        let detector = OutlierDetector::new(u32::MAX);
        assert!(!detector.try_eject());
        detector.record_uneject();
        detector.register_backend();
        assert!(detector.try_eject());
        assert!(!detector.try_eject());
        for _ in 0..3 {
            detector.record_uneject();
        }
        assert_counts(&detector, 1, 0);

        let detector = OutlierDetector::new(1);
        for _ in 0..100 {
            detector.register_backend();
        }
        let barrier = std::sync::Barrier::new(32);
        let admitted = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..32)
                .map(|_| {
                    let detector = &detector;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        detector.try_eject()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| u32::from(job.join().unwrap()))
                .sum::<u32>()
        });
        assert_eq!(admitted, 1);
        assert_counts(&detector, 100, 1);
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let detector = &detector;
                scope.spawn(move || detector.record_uneject());
            }
        });
        assert_counts(&detector, 100, 0);
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
                    ttl_ms: None,
                    cache_scope: None,
                    meta: None,
                },
            )),
        };
        assert!(!is_server_error(&ok_resp));
    }
}
