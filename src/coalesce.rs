//! Request coalescing middleware for the proxy.
//!
//! Deduplicates identical in-flight `CallTool` and `ReadResource` requests.
//! When multiple identical requests arrive concurrently, only one is forwarded
//! to the backend; all callers receive the same response.
//!
//! Leader election and follower registration are atomic. Dropping the leader
//! cancels its backend future, removes its own generation and wakes followers
//! with an error; tool calls are never replayed automatically. Active keys,
//! followers per key and whole-group lifetime are bounded by
//! [`CoalesceConfig`]. Completion is retained for already-registered followers.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tower::{Layer, Service, ServiceExt};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{InputResponses, McpRequest};

use crate::config::CoalesceConfig;

type Flights = Arc<Mutex<HashMap<String, Arc<Flight>>>>;

struct Flight {
    // Receivers never retain a sender: leader cancellation closes the channel.
    result: watch::Receiver<Option<RouterResponse>>,
    waiters: AtomicUsize,
    deadline: Instant,
}

struct Leader {
    flights: Flights,
    key: String,
    flight: Arc<Flight>,
    result: watch::Sender<Option<RouterResponse>>,
}

impl Drop for Leader {
    fn drop(&mut self) {
        let mut flights = self.flights.lock().expect("coalescing flights");
        if flights
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.flight))
        {
            flights.remove(&self.key);
        }
        // Dropping the sole sender wakes followers even when no result was sent.
    }
}

struct Follower(Arc<Flight>);
impl Drop for Follower {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

enum Election {
    Leader(Leader),
    Follower(Follower),
    Full,
}

fn error_response(id: tower_mcp::protocol::RequestId, message: &'static str) -> RouterResponse {
    RouterResponse {
        id,
        inner: Err(tower_mcp_types::JsonRpcError::internal_error(message)),
    }
}
const TIMED_OUT: &str = "Coalesced request timed out; execution may already have occurred";
const CANCELLED: &str = "Coalesced request interrupted; execution may already have occurred";
const FULL: &str = "Request coalescing capacity exceeded";

/// Tower layer that produces a [`CoalesceService`].
#[derive(Clone)]
pub struct CoalesceLayer;

impl CoalesceLayer {
    /// Create a new request coalescing layer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CoalesceLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for CoalesceLayer {
    type Service = CoalesceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CoalesceService::new(inner)
    }
}

/// Tower service that coalesces identical in-flight requests.
#[derive(Clone)]
pub struct CoalesceService<S> {
    inner: S,
    in_flight: Flights,
    config: CoalesceConfig,
}

impl<S> CoalesceService<S> {
    /// Create a new request coalescing service wrapping `inner`.
    pub fn new(inner: S) -> Self {
        Self::with_config(inner, &CoalesceConfig::default()).expect("default coalescing bounds")
    }

    /// Wrap `inner` with validated resource bounds and a whole-group deadline.
    ///
    /// Returns an error for zero capacity or a timeout outside 1..=86400 seconds.
    /// Overload, cancellation and timeout return JSON-RPC errors. A timeout or
    /// cancellation does not prove that a side-effecting backend did no work.
    pub fn with_config(inner: S, config: &CoalesceConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            inner,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            config: config.clone(),
        })
    }

    fn elect(&self, key: String) -> Election {
        let mut flights = self.in_flight.lock().expect("coalescing flights");
        if let Some(flight) = flights.get(&key) {
            if flight.waiters.load(Ordering::Relaxed) >= self.config.max_waiters {
                return Election::Full;
            }
            flight.waiters.fetch_add(1, Ordering::Relaxed);
            return Election::Follower(Follower(flight.clone()));
        }
        if flights.len() >= self.config.max_entries {
            return Election::Full;
        }
        let (result, receiver) = watch::channel(None);
        let flight = Arc::new(Flight {
            result: receiver,
            waiters: AtomicUsize::new(0),
            deadline: Instant::now() + Duration::from_secs(self.config.timeout_seconds),
        });
        flights.insert(key.clone(), flight.clone());
        Election::Leader(Leader {
            flights: self.in_flight.clone(),
            key,
            flight,
            result,
        })
    }
}

fn continuation_identity(
    input_responses: &Option<InputResponses>,
    request_state: &Option<String>,
) -> String {
    serde_json::to_string(&(input_responses, request_state)).unwrap_or_default()
}

fn coalesce_key(req: &McpRequest) -> Option<String> {
    match req {
        McpRequest::CallTool(params) => {
            let args = serde_json::to_string(&params.arguments).unwrap_or_default();
            let continuation =
                continuation_identity(&params.input_responses, &params.request_state);
            Some(format!("tool:{}:{args}:{continuation}", params.name))
        }
        McpRequest::ReadResource(params) => {
            let continuation =
                continuation_identity(&params.input_responses, &params.request_state);
            Some(format!("res:{}:{continuation}", params.uri))
        }
        _ => None,
    }
}

impl<S> Service<RouterRequest> for CoalesceService<S>
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

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Each dispatched clone is independently readied by oneshot. Followers
        // need no backend capacity, so do not reserve inner readiness for them.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let Some(key) = coalesce_key(&req.inner) else {
            // Non-coalesceable request, pass through
            return Box::pin(self.inner.clone().oneshot(req));
        };

        // Register on first poll, so an unpolled future occupies no table slot.
        let service = self.clone();
        let request_id = req.id.clone();

        Box::pin(async move {
            match service.elect(key) {
                Election::Full => Ok(error_response(request_id, FULL)),
                Election::Leader(leader) => {
                    let response = match tokio::time::timeout_at(
                        leader.flight.deadline,
                        service.inner.oneshot(req),
                    )
                    .await
                    {
                        Ok(Ok(response)) => response,
                        Err(_) => error_response(request_id, TIMED_OUT),
                    };
                    leader.result.send_replace(Some(response.clone()));
                    // Cleanup is synchronous and generation-aware on every exit.
                    drop(leader);
                    Ok(response)
                }
                Election::Follower(follower) => {
                    let mut result = follower.0.result.clone();
                    let wait = async {
                        loop {
                            if let Some(response) = result.borrow_and_update().clone() {
                                return RouterResponse {
                                    id: request_id.clone(),
                                    inner: response.inner,
                                };
                            }
                            if result.changed().await.is_err() {
                                return error_response(request_id.clone(), CANCELLED);
                            }
                        }
                    };
                    let response = tokio::time::timeout_at(follower.0.deadline, wait)
                        .await
                        .unwrap_or_else(|_| error_response(request_id, TIMED_OUT));
                    drop(follower);
                    Ok(response)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use tower_mcp::protocol::{McpRequest, McpResponse};

    use super::CoalesceService;
    use crate::test_util::{MockService, call_service};

    #[tokio::test]
    async fn test_coalesce_passes_through_single_request() {
        let mock = MockService::with_tools(&["fs/read"]);
        let mut svc = CoalesceService::new(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "fs/read".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        )
        .await;

        match resp.inner.unwrap() {
            McpResponse::CallTool(r) => assert_eq!(r.all_text(), "called: fs/read"),
            other => panic!("expected CallTool, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_coalesce_non_coalesceable_passes_through() {
        let mock = MockService::with_tools(&["tool"]);
        let mut svc = CoalesceService::new(mock);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok(), "list_tools should pass through");
    }

    #[tokio::test]
    async fn test_coalesce_key_includes_arguments() {
        // Different arguments should produce different keys
        let key1 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }));
        let key2 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }));
        assert_ne!(key1, key2, "different args should have different keys");
    }

    #[tokio::test]
    async fn test_coalesce_key_same_arguments_produce_same_key() {
        let key1 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }));
        let key2 =
            super::coalesce_key(&McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }));
        assert_eq!(key1, key2, "same tool+args should have the same key");
    }

    #[tokio::test]
    async fn test_coalesce_key_read_resource() {
        let key = super::coalesce_key(&McpRequest::ReadResource(
            tower_mcp::protocol::ReadResourceParams {
                uri: "file:///tmp/test.txt".to_string(),
                input_responses: None,
                request_state: None,
                meta: None,
            },
        ));
        assert_eq!(
            key,
            Some("res:file:///tmp/test.txt:[null,null]".to_string())
        );
    }

    #[tokio::test]
    async fn test_coalesce_key_includes_continuation_state() {
        let request = |state: &str| {
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"a": 1}),
                input_responses: None,
                request_state: Some(state.to_string()),
                meta: None,
                task: None,
            })
        };

        let key1 = super::coalesce_key(&request("first"));
        let key2 = super::coalesce_key(&request("second"));
        assert_ne!(key1, key2, "continuations must not be coalesced together");
    }

    #[tokio::test]
    async fn test_coalesce_key_non_coalesceable_returns_none() {
        let key = super::coalesce_key(&McpRequest::ListTools(Default::default()));
        assert!(key.is_none(), "ListTools should not be coalesceable");

        let key = super::coalesce_key(&McpRequest::ListResources(Default::default()));
        assert!(key.is_none(), "ListResources should not be coalesceable");
    }

    #[tokio::test]
    async fn test_concurrent_identical_requests_coalesced() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::Service;

        // A mock that counts how many times it's actually called
        #[derive(Clone)]
        struct CountingService {
            call_count: Arc<AtomicUsize>,
        }

        impl Service<tower_mcp::router::RouterRequest> for CountingService {
            type Response = tower_mcp::router::RouterResponse;
            type Error = std::convert::Infallible;
            type Future = std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                tower_mcp::router::RouterResponse,
                                std::convert::Infallible,
                            >,
                        > + Send,
                >,
            >;

            fn poll_ready(
                &mut self,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: tower_mcp::router::RouterRequest) -> Self::Future {
                let count = self.call_count.clone();
                let id = req.id.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    // Small delay to ensure concurrent requests overlap
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(tower_mcp::router::RouterResponse {
                        id,
                        inner: Ok(McpResponse::CallTool(
                            tower_mcp::protocol::CallToolResult::text("result"),
                        )),
                    })
                })
            }
        }

        let call_count = Arc::new(AtomicUsize::new(0));
        let svc = CountingService {
            call_count: call_count.clone(),
        };
        let coalesce = CoalesceService::new(svc);

        let make_request = || {
            let mut c = coalesce.clone();
            let req = tower_mcp::router::RouterRequest {
                id: tower_mcp::protocol::RequestId::Number(1),
                inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                    name: "tool".to_string(),
                    arguments: serde_json::json!({"x": 42}),
                    input_responses: None,
                    request_state: None,
                    meta: None,
                    task: None,
                }),
                extensions: tower_mcp::router::Extensions::new(),
            };
            async move { c.call(req).await }
        };

        // Fire 3 identical requests concurrently
        let (r1, r2, r3) = tokio::join!(make_request(), make_request(), make_request());

        // All should succeed
        assert!(r1.is_ok());
        assert!(r2.is_ok());
        assert!(r3.is_ok());

        // Atomic registration admits exactly one leader for overlapping calls.
        let count = call_count.load(Ordering::SeqCst);
        assert!(
            count == 1,
            "expected exactly one backend call due to coalescing, got {count}"
        );
    }

    #[tokio::test]
    async fn test_different_requests_not_coalesced() {
        let mock = MockService::with_tools(&["tool"]);
        let coalesce = CoalesceService::new(mock);

        // Two requests with different arguments
        let mut c1 = coalesce.clone();
        let req1 = tower_mcp::router::RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(1),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"x": 1}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        let mut c2 = coalesce.clone();
        let req2 = tower_mcp::router::RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(2),
            inner: McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "tool".to_string(),
                arguments: serde_json::json!({"x": 2}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        let (r1, r2) = tokio::join!(
            tower::Service::call(&mut c1, req1),
            tower::Service::call(&mut c2, req2)
        );

        // Both should succeed independently
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    #[tokio::test]
    async fn test_coalesce_with_error_response() {
        use crate::test_util::ErrorMockService;

        let mock = ErrorMockService;
        let mut svc = CoalesceService::new(mock);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
                name: "failing_tool".to_string(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        )
        .await;

        // The error response should pass through correctly
        assert!(
            resp.inner.is_err(),
            "error response should propagate through coalesce"
        );
        let err = resp.inner.unwrap_err();
        assert_eq!(err.code, -32603);
        assert_eq!(err.message, "internal error");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};
    use tower_mcp::protocol::{CallToolParams, CallToolResult, McpResponse, RequestId};
    use tower_mcp::router::Extensions;

    struct PendingCall {
        id: RequestId,
        reply: oneshot::Sender<Result<McpResponse, tower_mcp_types::JsonRpcError>>,
    }
    impl PendingCall {
        fn finish(self, failed: bool) {
            let result = if failed {
                Err(tower_mcp_types::JsonRpcError::internal_error(
                    "backend failure",
                ))
            } else {
                Ok(McpResponse::CallTool(CallToolResult::text("shared result")))
            };
            self.reply.send(result).unwrap();
        }
    }
    struct GateService {
        requests: mpsc::UnboundedSender<PendingCall>,
        calls: Arc<AtomicUsize>,
        ready: bool,
    }
    impl Clone for GateService {
        fn clone(&self) -> Self {
            Self {
                requests: self.requests.clone(),
                calls: self.calls.clone(),
                ready: false,
            }
        }
    }
    impl Service<RouterRequest> for GateService {
        type Response = RouterResponse;
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            self.ready = true;
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: RouterRequest) -> Self::Future {
            assert!(
                std::mem::take(&mut self.ready),
                "dispatched an unreadied clone"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (reply, response) = oneshot::channel();
            self.requests
                .send(PendingCall {
                    id: req.id.clone(),
                    reply,
                })
                .unwrap();
            Box::pin(async move {
                Ok(RouterResponse {
                    id: req.id,
                    inner: response.await.expect("test backend panicked"),
                })
            })
        }
    }
    fn fixture(
        config: CoalesceConfig,
    ) -> (
        CoalesceService<GateService>,
        mpsc::UnboundedReceiver<PendingCall>,
    ) {
        let (requests, receiver) = mpsc::unbounded_channel();
        let service = GateService {
            requests,
            calls: Arc::new(AtomicUsize::new(0)),
            ready: false,
        };
        (
            CoalesceService::with_config(service, &config).unwrap(),
            receiver,
        )
    }
    fn request(id: i64, key: &str) -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(id),
            inner: McpRequest::CallTool(CallToolParams {
                name: key.into(),
                arguments: serde_json::json!({}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        }
    }
    fn poll_pending<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) {
        assert!(
            future
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
    }
    fn entries(service: &CoalesceService<GateService>) -> usize {
        service.in_flight.lock().unwrap().len()
    }
    fn waiters(service: &CoalesceService<GateService>) -> usize {
        service
            .in_flight
            .lock()
            .unwrap()
            .values()
            .map(|flight| flight.waiters.load(Ordering::Relaxed))
            .sum()
    }
    fn assert_error(response: RouterResponse, id: i64, message: &str) {
        assert_eq!(response.id, RequestId::Number(id));
        let error = response.inner.unwrap_err();
        assert_eq!(error.code, -32603);
        assert_eq!(error.message, message);
        assert!(error.data.is_none());
    }

    #[tokio::test]
    async fn leader_abort_wakes_followers_and_allows_fresh_request_without_replay() {
        let (mut service, mut backend) = fixture(CoalesceConfig::default());
        let unpolled = service.call(request(0, "tool"));
        assert_eq!(entries(&service), 0);
        drop(unpolled);
        let mut leader = service.call(request(1, "tool"));
        poll_pending(leader.as_mut());
        let abandoned = backend.recv().await.unwrap();
        let mut follower = service.call(request(2, "tool"));
        poll_pending(follower.as_mut());
        let mut other = service.call(request(3, "tool"));
        poll_pending(other.as_mut());
        assert_eq!(waiters(&service), 2);
        drop(leader);
        assert_eq!(entries(&service), 0);
        assert!(
            abandoned.reply.is_closed(),
            "backend future was not dropped"
        );
        // Old followers may finish after a new generation already owns the key.
        let mut fresh = service.call(request(4, "tool"));
        poll_pending(fresh.as_mut());
        assert_error(
            tokio::time::timeout(Duration::from_secs(1), follower)
                .await
                .unwrap()
                .unwrap(),
            2,
            CANCELLED,
        );
        assert_error(
            tokio::time::timeout(Duration::from_secs(1), other)
                .await
                .unwrap()
                .unwrap(),
            3,
            CANCELLED,
        );
        assert_eq!(
            service.inner.calls.load(Ordering::SeqCst),
            2,
            "followers replayed the tool"
        );
        assert_eq!(
            entries(&service),
            1,
            "old cleanup removed the new generation"
        );
        backend.recv().await.unwrap().finish(false);
        assert_eq!(fresh.await.unwrap().id, RequestId::Number(4));
        assert_eq!(entries(&service), 0);
        assert_eq!(service.inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn follower_abort_releases_waiter_capacity_and_retained_result_survives_leader_exit() {
        let (mut service, mut backend) = fixture(CoalesceConfig {
            max_waiters: 1,
            ..Default::default()
        });
        let mut leader = service.call(request(1, "tool"));
        poll_pending(leader.as_mut());
        let pending = backend.recv().await.unwrap();
        let mut follower = service.call(request(2, "tool"));
        poll_pending(follower.as_mut());
        assert_error(service.call(request(3, "tool")).await.unwrap(), 3, FULL);
        drop(follower);
        assert_eq!(waiters(&service), 0);
        let mut replacement = service.call(request(4, "tool"));
        poll_pending(replacement.as_mut());
        pending.finish(false);
        // Publish and remove the group before the follower polls again.
        assert!(leader.await.unwrap().inner.is_ok());
        assert_eq!(entries(&service), 0);
        let response = replacement.await.unwrap();
        assert_eq!(response.id, RequestId::Number(4));
        let McpResponse::CallTool(result) = response.inner.unwrap() else {
            panic!("expected tool result")
        };
        assert_eq!(result.all_text(), "shared result");
        assert_eq!(service.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_arrivals_elect_exactly_one_leader_and_share_success_and_backend_failure()
    {
        for failed in [false, true] {
            let (service, mut backend) = fixture(CoalesceConfig::default());
            let barrier = Arc::new(tokio::sync::Barrier::new(65));
            let mut tasks = Vec::new();
            for id in 0..64 {
                let mut clone = service.clone();
                let barrier = barrier.clone();
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    clone.call(request(id, "tool")).await.unwrap()
                }));
            }
            barrier.wait().await;
            let pending = tokio::time::timeout(Duration::from_secs(2), backend.recv())
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while waiters(&service) != 63 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(service.inner.calls.load(Ordering::SeqCst), 1);
            pending.finish(failed);
            for (id, task) in tasks.into_iter().enumerate() {
                let response = task.await.unwrap();
                assert_eq!(response.id, RequestId::Number(id as i64));
                if failed {
                    assert_eq!(response.inner.unwrap_err().message, "backend failure");
                } else {
                    assert!(response.inner.is_ok());
                }
            }
            assert_eq!(entries(&service), 0);
        }
    }

    #[tokio::test]
    async fn unique_key_flood_is_bounded_and_cancelled_slots_are_reusable() {
        let (mut service, mut backend) = fixture(CoalesceConfig {
            max_entries: 2,
            ..Default::default()
        });
        let mut first = service.call(request(1, "one"));
        poll_pending(first.as_mut());
        let mut second = service.call(request(2, "two"));
        poll_pending(second.as_mut());
        for id in 3..103 {
            assert_error(
                service
                    .call(request(id, &format!("key-{id}")))
                    .await
                    .unwrap(),
                id,
                FULL,
            );
            assert_eq!(entries(&service), 2);
        }
        assert_eq!(service.inner.calls.load(Ordering::SeqCst), 2);
        drop(first);
        drop(second);
        assert_eq!(entries(&service), 0);
        while backend.try_recv().is_ok() {}
        let mut fresh = service.call(request(104, "fresh"));
        poll_pending(fresh.as_mut());
        backend.recv().await.unwrap().finish(false);
        assert!(fresh.await.unwrap().inner.is_ok());
    }

    #[tokio::test]
    async fn group_deadline_cancels_backend_and_wakes_every_caller() {
        let (mut service, mut backend) = fixture(CoalesceConfig {
            timeout_seconds: 1,
            ..Default::default()
        });
        let mut leader = service.call(request(1, "slow"));
        poll_pending(leader.as_mut());
        let pending = backend.recv().await.unwrap();
        let mut follower = service.call(request(2, "slow"));
        poll_pending(follower.as_mut());
        let (one, two) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(leader, follower)
        })
        .await
        .unwrap();
        assert_error(one.unwrap(), 1, TIMED_OUT);
        assert_error(two.unwrap(), 2, TIMED_OUT);
        assert!(pending.reply.is_closed());
        assert_eq!(entries(&service), 0);
        assert_eq!(service.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone)]
    struct NeverReady;
    impl Service<RouterRequest> for NeverReady {
        type Response = RouterResponse;
        type Error = Infallible;
        type Future = std::future::Ready<Result<RouterResponse, Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Pending
        }
        fn call(&mut self, _: RouterRequest) -> Self::Future {
            panic!("unready backend was called")
        }
    }
    #[tokio::test]
    async fn readiness_wait_is_inside_group_deadline() {
        let mut service = CoalesceService::with_config(
            NeverReady,
            &CoalesceConfig {
                timeout_seconds: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(2), service.call(request(1, "tool")))
                .await
                .unwrap()
                .unwrap();
        assert_error(response, 1, TIMED_OUT);
        assert!(service.in_flight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn backend_panic_cleans_generation_and_wakes_follower() {
        let (mut service, mut backend) = fixture(CoalesceConfig::default());
        let leader = tokio::spawn(service.call(request(1, "tool")));
        let pending = backend.recv().await.unwrap();
        let mut follower = service.call(request(2, "tool"));
        poll_pending(follower.as_mut());
        drop(pending); // The controlled backend panics when its response is lost.
        assert!(leader.await.unwrap_err().is_panic());
        assert_error(
            tokio::time::timeout(Duration::from_secs(1), follower)
                .await
                .unwrap()
                .unwrap(),
            2,
            CANCELLED,
        );
        assert_eq!(entries(&service), 0);
    }

    #[tokio::test]
    async fn non_coalescible_requests_still_readied_when_table_is_full() {
        let (mut service, mut backend) = fixture(CoalesceConfig {
            max_entries: 1,
            ..Default::default()
        });
        let mut leader = service.call(request(1, "tool"));
        poll_pending(leader.as_mut());
        let _pending = backend.recv().await.unwrap();
        let mut list = request(2, "unused");
        list.inner = McpRequest::ListTools(Default::default());
        let mut call = service.call(list);
        poll_pending(call.as_mut());
        let pending = backend.recv().await.unwrap();
        assert_eq!(pending.id, RequestId::Number(2));
        pending.finish(false);
        assert!(call.await.unwrap().inner.is_ok());
        assert_eq!(entries(&service), 1);
        drop(leader);
    }

    #[test]
    fn coalescing_limits_reject_unbounded_or_invalid_values() {
        for config in [
            CoalesceConfig {
                max_entries: 0,
                ..Default::default()
            },
            CoalesceConfig {
                max_waiters: 0,
                ..Default::default()
            },
            CoalesceConfig {
                timeout_seconds: 0,
                ..Default::default()
            },
            CoalesceConfig {
                timeout_seconds: 86401,
                ..Default::default()
            },
        ] {
            assert!(CoalesceService::with_config(NeverReady, &config).is_err());
        }
    }

    #[test]
    fn configured_limits_are_validated_and_old_configs_keep_defaults() {
        let base = "[proxy]\nname='coalescing-test'\n[proxy.listen]\n[[backends]]\nname='api'\ntransport='http'\nurl='http://localhost:1'\n";
        let defaults = crate::ProxyConfig::parse(base).unwrap();
        assert!(!defaults.performance.coalesce_requests);
        assert_eq!(defaults.performance.coalesce.max_entries, 1024);
        assert_eq!(defaults.performance.coalesce.max_waiters, 128);
        assert_eq!(defaults.performance.coalesce.timeout_seconds, 300);
        for invalid in [
            "max_entries=0",
            "max_waiters=0",
            "timeout_seconds=0",
            "timeout_seconds=86401",
        ] {
            assert!(
                crate::ProxyConfig::parse(&format!("{base}\n[performance.coalesce]\n{invalid}"))
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn resource_read_followers_preserve_string_request_ids() {
        let (mut service, mut backend) = fixture(CoalesceConfig::default());
        let resource = |id: &str| RouterRequest {
            id: RequestId::String(id.into()),
            inner: McpRequest::ReadResource(tower_mcp::protocol::ReadResourceParams {
                uri: "fixture://resource".into(),
                input_responses: None,
                request_state: None,
                meta: None,
            }),
            extensions: Extensions::new(),
        };
        let mut leader = service.call(resource("leader"));
        poll_pending(leader.as_mut());
        let mut follower = service.call(resource("follower"));
        poll_pending(follower.as_mut());
        let result = McpResponse::ReadResource(tower_mcp::protocol::ReadResourceResult {
            contents: vec![],
            ttl_ms: None,
            cache_scope: None,
            meta: None,
        });
        backend
            .recv()
            .await
            .unwrap()
            .reply
            .send(Ok(result))
            .unwrap();
        assert_eq!(leader.await.unwrap().id, RequestId::String("leader".into()));
        let response = follower.await.unwrap();
        assert_eq!(response.id, RequestId::String("follower".into()));
        assert!(matches!(response.inner, Ok(McpResponse::ReadResource(_))));
        assert_eq!(service.inner.calls.load(Ordering::SeqCst), 1);
    }
}
