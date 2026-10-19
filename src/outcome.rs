//! Shared MCP outcome policy for middleware decisions.
//!
//! Classification uses typed results and numeric codes, never error text. Known
//! protocol/client errors in the server-code range are terminal. Opaque backend
//! failures already folded to -32603 remain server failures; their original
//! transport type cannot be recovered from the response.
//!
//! | Class | Retry / breaker / outlier failure | Configured hedge continues |
//! | --- | --- | --- |
//! | Success | No | No |
//! | Tool failure (`isError=true`) | No | Yes |
//! | Known client/protocol or unknown application error | No | No |
//! | Server or transport failure | Yes | Yes |
//!
//! Retry and hedging require explicit configuration and may duplicate execution.
//! The hedge adapter preserves the original primary response when all attempts
//! fail. Only completed successful tool/resource results enter caches, including
//! when reading legacy entries. Intermediate task/input-required responses do not.
//!
//! Request counters retain `method`/`status` and add the bounded [`Outcome::as_str`]
//! label; tool failures now have `status="error"`. Access/audit records likewise
//! include outcomes without response bodies. [`crate::admin::admin_router`] now
//! accepts breaker handles using [`McpFailureClassifier`] rather than the default
//! service-error-only classifier.
use std::convert::Infallible;
use tower::{Layer, ServiceExt, util::BoxCloneService};
use tower_mcp::{RouterRequest, RouterResponse, protocol::McpResponse};

/// Bounded outcome classes shared by resilience, caching and telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Successful protocol result (possibly an intermediate continuation/task).
    Success,
    /// A tool result with `isError=true`, not automatically retried.
    ToolFailure,
    /// Standard invalid request/params/method or known MCP client/protocol error.
    ClientError,
    /// Internal error or implementation-defined server error.
    ServerFailure,
    /// Typed service failure or explicit MCP connection-closed/request-timeout.
    TransportFailure,
    /// An unrecognized application code, conservatively terminal.
    OtherError,
}
impl Outcome {
    /// Classify a response without examining error text or application data.
    pub fn from_response(response: &RouterResponse) -> Self {
        match &response.inner {
            Ok(McpResponse::CallTool(result)) if result.is_error => Self::ToolFailure,
            Ok(_) => Self::Success,
            Err(error) => match error.code {
                -32700 | -32602..=-32600 | -32002 | -32009..=-32005 | -32022..=-32020 | -32042 => {
                    Self::ClientError
                }
                -32000 | -32010 => Self::TransportFailure,
                -32603 | -32099..=-32000 => Self::ServerFailure,
                _ => Self::OtherError,
            },
        }
    }
    /// Classify a Tower result; typed service errors count as transport failures.
    pub fn from_result<E>(result: &Result<RouterResponse, E>) -> Self {
        match result {
            Ok(response) => Self::from_response(response),
            Err(_) => Self::TransportFailure,
        }
    }
    /// Stable, bounded telemetry label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ToolFailure => "tool_error",
            Self::ClientError => "client_error",
            Self::ServerFailure => "server_error",
            Self::TransportFailure => "transport_error",
            Self::OtherError => "other_error",
        }
    }
    /// Coarse status retained by access logs and request counters.
    pub fn status(self) -> &'static str {
        if self == Self::Success { "ok" } else { "error" }
    }
    /// Whether retry, breaker and outlier policies treat this as backend failure.
    /// Retrying still requires an explicitly configured retry policy.
    pub fn backend_failure(self) -> bool {
        matches!(self, Self::ServerFailure | Self::TransportFailure)
    }
    /// Whether a configured hedge should keep seeking a better result.
    /// Client/protocol and unknown errors are terminal; tool failures can lose
    /// to successful hedges but do not cause ordinary retries or breaker failures.
    pub fn hedge_failure(self) -> bool {
        self.backend_failure() || self == Self::ToolFailure
    }
}

/// Cache only completed successful tool results and resource reads.
/// Intermediate input-required/task handles and legacy cached errors are excluded.
pub fn cacheable(response: &RouterResponse) -> bool {
    Outcome::from_response(response) == Outcome::Success
        && matches!(
            &response.inner,
            Ok(McpResponse::CallTool(_) | McpResponse::ReadResource(_))
        )
}

/// Response-aware classifier for configured circuit breakers.
#[derive(Debug, Clone, Copy, Default)]
pub struct McpFailureClassifier;
impl<E> tower_resilience::circuitbreaker::classifier::FailureClassifier<RouterResponse, E>
    for McpFailureClassifier
{
    fn classify(&self, result: &Result<RouterResponse, E>) -> bool {
        Outcome::from_result(result).backend_failure()
    }
}

// Keep failed responses in the hedge's Clone error plane, then restore the
// original primary response if every attempt fails. Never stringify the error.
#[derive(Clone)]
struct HedgeFailure(RouterResponse);

pub(crate) fn hedged(
    service: BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    layer: &tower_resilience::hedge::HedgeLayer,
) -> BoxCloneService<RouterRequest, RouterResponse, Infallible> {
    let attempt = tower::service_fn(move |request: RouterRequest| {
        let inner = service.clone();
        async move {
            let response = inner.oneshot(request).await.expect("infallible backend");
            if Outcome::from_response(&response).hedge_failure() {
                Err(HedgeFailure(response))
            } else {
                Ok(response)
            }
        }
    });
    let service = layer.layer(attempt);
    BoxCloneService::new(tower::service_fn(move |request: RouterRequest| {
        let service = service.clone();
        async move {
            Ok::<_, Infallible>(match service.oneshot(request).await {
                Ok(response) => response,
                Err(error) => error.into_inner().0,
            })
        }
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tower_mcp::protocol::{CallToolResult, McpRequest, RequestId};
    use tower_mcp::router::Extensions;
    use tower_mcp_types::JsonRpcError;

    pub(crate) struct Case {
        pub name: String,
        pub inner: Result<McpResponse, JsonRpcError>,
        pub outcome: Outcome,
        pub backend_failure: bool,
        pub hedge_failure: bool,
    }
    pub(crate) fn cases() -> Vec<Case> {
        let mut cases = vec![
            Case {
                name: "success".into(),
                inner: Ok(McpResponse::CallTool(CallToolResult::text("healthy"))),
                outcome: Outcome::Success,
                backend_failure: false,
                hedge_failure: false,
            },
            Case {
                name: "tool_error".into(),
                inner: Ok(McpResponse::CallTool(CallToolResult::error(
                    "private tool failure",
                ))),
                outcome: Outcome::ToolFailure,
                backend_failure: false,
                hedge_failure: true,
            },
        ];
        for code in [
            -32700, -32600, -32601, -32602, -32002, -32005, -32006, -32007, -32008, -32009, -32020,
            -32021, -32022, -32042, -32603, -32001, -32099, -32000, -32010, 1234, -33333,
        ] {
            let (outcome, backend_failure) = match code {
                -32603 | -32001 | -32099 => (Outcome::ServerFailure, true),
                -32000 | -32010 => (Outcome::TransportFailure, true),
                1234 | -33333 => (Outcome::OtherError, false),
                _ => (Outcome::ClientError, false),
            };
            cases.push(Case {
                name: format!("code-{code}"),
                inner: Err(JsonRpcError {
                    code,
                    message: "private failure; do not classify text".into(),
                    data: Some(serde_json::json!({"retryable":true,"private":"payload"})),
                }),
                outcome,
                backend_failure,
                hedge_failure: backend_failure,
            });
        }
        cases
    }
    pub(crate) fn request(id: i64) -> RouterRequest {
        RouterRequest {
            id: RequestId::Number(id),
            inner: McpRequest::CallTool(
                serde_json::from_value(serde_json::json!({"name":"api/work","arguments":{}}))
                    .unwrap(),
            ),
            extensions: Extensions::new(),
        }
    }
    pub(crate) fn service(
        inner: Result<McpResponse, JsonRpcError>,
    ) -> (
        BoxCloneService<RouterRequest, RouterResponse, Infallible>,
        Arc<AtomicUsize>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        (
            BoxCloneService::new(tower::service_fn(move |request: RouterRequest| {
                count.fetch_add(1, Ordering::SeqCst);
                let inner = inner.clone();
                async move {
                    Ok::<_, Infallible>(RouterResponse {
                        id: request.id,
                        inner,
                    })
                }
            })),
            calls,
        )
    }

    #[test]
    fn policy_table_drives_access_audit_and_metrics_without_logging_error_bodies() {
        use std::io::Write;
        use std::sync::Mutex;
        struct Output(Arc<Mutex<Vec<u8>>>);
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || Output(writer.clone()))
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let exercise = || {
            tracing::subscriber::with_default(subscriber, || {
                runtime.block_on(async {
                    for case in cases() {
                        let (base, _) = service(case.inner);
                        #[cfg(feature = "metrics")]
                        let base = BoxCloneService::new(crate::metrics::MetricsService::new(base));
                        let access = crate::access_log::AccessLogService::new(base, "/");
                        let audit = crate::audit::AuditService::new(access);
                        audit.oneshot(request(99)).await.unwrap();
                    }
                })
            })
        };
        #[cfg(feature = "metrics")]
        {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            metrics::with_local_recorder(&recorder, exercise);
            let text = handle.render();
            let mut counts = std::collections::BTreeMap::new();
            for case in cases() {
                *counts
                    .entry((case.outcome.as_str(), case.outcome.status()))
                    .or_insert(0usize) += 1;
            }
            for ((outcome, status), count) in counts {
                let line = text
                    .lines()
                    .find(|line| {
                        line.starts_with("mcp_proxy_requests_total{")
                            && line.contains(&format!("outcome=\"{outcome}\""))
                            && line.contains(&format!("status=\"{status}\""))
                    })
                    .expect("missing outcome counter");
                assert_eq!(line.split_whitespace().last().unwrap(), count.to_string());
            }
        }
        #[cfg(not(feature = "metrics"))]
        exercise();
        let bytes = output.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            !text.contains("private"),
            "error text/data leaked into telemetry"
        );
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let access: Vec<_> = records
            .iter()
            .filter(|r| r["target"] == "mcp::access")
            .collect();
        let audit: Vec<_> = records
            .iter()
            .filter(|r| r["target"] == "mcp::audit")
            .collect();
        assert_eq!(access.len(), cases().len());
        assert_eq!(audit.len(), cases().len());
        for (index, case) in cases().into_iter().enumerate() {
            assert_eq!(access[index]["fields"]["outcome"], case.outcome.as_str());
            assert_eq!(access[index]["fields"]["status"], case.outcome.status());
            assert_eq!(audit[index]["fields"]["outcome"], case.outcome.as_str());
            let denied = case
                .inner
                .as_ref()
                .err()
                .is_some_and(|error| [-32602, -32007].contains(&error.code));
            assert_eq!(
                audit[index]["fields"]["status"],
                if case.outcome == Outcome::Success {
                    "success"
                } else if denied {
                    "denied"
                } else {
                    "error"
                }
            );
        }
    }

    #[test]
    fn outcome_policy_table_excludes_protocol_and_application_failures_from_retry_and_breaker() {
        for case in cases() {
            let response = RouterResponse {
                id: RequestId::Number(1),
                inner: case.inner,
            };
            let outcome = Outcome::from_response(&response);
            assert_eq!(outcome, case.outcome, "{}", case.name);
            assert_eq!(
                outcome.backend_failure(),
                case.backend_failure,
                "{}",
                case.name
            );
            assert_eq!(outcome.hedge_failure(), case.hedge_failure, "{}", case.name);
            assert_eq!(
                cacheable(&response),
                outcome == Outcome::Success,
                "{}",
                case.name
            );
            assert_eq!(
                outcome.status(),
                if outcome == Outcome::Success {
                    "ok"
                } else {
                    "error"
                }
            );
        }
        let failure: Result<RouterResponse, std::io::Error> =
            Err(std::io::Error::other("opaque transport"));
        assert_eq!(Outcome::from_result(&failure), Outcome::TransportFailure);
        assert!(
            tower_resilience::circuitbreaker::classifier::FailureClassifier::classify(
                &McpFailureClassifier,
                &failure
            )
        );
        // Successful non-result envelopes must never enter tool/resource caches.
        let response = RouterResponse {
            id: RequestId::Number(1),
            inner: Ok(McpResponse::Pong(Default::default())),
        };
        assert!(!cacheable(&response));
        for (inner, eligible) in [
            (McpResponse::ReadResource(Default::default()), true),
            (
                McpResponse::InputRequired(tower_mcp::protocol::InputRequiredResult::new()),
                false,
            ),
            (
                McpResponse::Raw(serde_json::json!({"resultType":"task", "taskId":"pending"})),
                false,
            ),
        ] {
            assert_eq!(
                cacheable(&RouterResponse {
                    id: RequestId::Number(2),
                    inner: Ok(inner)
                }),
                eligible
            );
        }
    }

    #[tokio::test]
    async fn policy_table_drives_actual_retry_counts_cache_eligibility_breakers_and_outliers() {
        for case in cases() {
            let (base, calls) = service(case.inner.clone());
            let config = crate::config::RetryConfig {
                max_retries: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 1,
                budget_percent: None,
                min_retries_per_sec: 0,
            };
            let retry = crate::retry::build_retry_layer(&config, "table").layer(base);
            let response = retry.oneshot(request(10)).await.unwrap();
            assert_eq!(response.id, RequestId::Number(10));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if case.backend_failure { 2 } else { 1 },
                "retry {}",
                case.name
            );
            assert_eq!(
                serde_json::to_value(response.inner).unwrap(),
                serde_json::to_value(&case.inner).unwrap()
            );

            let (base, calls) = service(case.inner.clone());
            let config = crate::config::BackendCacheConfig {
                tool_ttl_seconds: 60,
                resource_ttl_seconds: 60,
                max_entries: 10,
            };
            let (cache, handle) = crate::cache::CacheService::new(
                base,
                vec![("api/".into(), &config)],
                &crate::config::CacheBackendConfig::default(),
            );
            for id in [20, 21] {
                let response = cache.clone().oneshot(request(id)).await.unwrap();
                assert_eq!(response.id, RequestId::Number(id));
                assert_eq!(
                    serde_json::to_value(response.inner).unwrap(),
                    serde_json::to_value(&case.inner).unwrap()
                );
            }
            let eligible = case.outcome == Outcome::Success;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if eligible { 1 } else { 2 },
                "cache {}",
                case.name
            );
            assert_eq!(handle.stats().await[0].hits, if eligible { 1 } else { 0 });

            let (base, calls) = service(case.inner.clone());
            let (layer, handle) = tower_resilience::circuitbreaker::CircuitBreakerLayer::builder()
                .failure_classifier_type(McpFailureClassifier)
                .minimum_number_of_calls(2)
                .sliding_window_size(2)
                .failure_rate_threshold(0.5)
                .build_with_handle()
                .unwrap();
            let breaker = layer.layer(base);
            for id in [30, 31] {
                breaker.clone().oneshot(request(id)).await.unwrap();
            }
            assert_eq!(
                handle.is_open(),
                case.backend_failure,
                "breaker {}",
                case.name
            );
            let third = breaker.oneshot(request(32)).await;
            assert_eq!(third.is_err(), case.backend_failure);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if case.backend_failure { 2 } else { 3 }
            );

            let (base, calls) = service(case.inner);
            let detector = crate::outlier::OutlierDetector::new(100);
            let config = crate::config::OutlierDetectionConfig {
                consecutive_errors: 2,
                interval_seconds: 1,
                base_ejection_seconds: 60,
                max_ejection_percent: 100,
            };
            let outlier =
                crate::outlier::OutlierDetectionLayer::new("api".into(), config, detector.clone())
                    .layer(base);
            for id in [40, 41, 42] {
                outlier.clone().oneshot(request(id)).await.unwrap();
            }
            assert_eq!(
                detector.ejected_count(),
                u32::from(case.backend_failure),
                "outlier {}",
                case.name
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if case.backend_failure { 2 } else { 3 }
            );
        }
    }

    #[tokio::test]
    async fn hedge_policy_table_selects_success_or_terminal_client_error_and_preserves_all_failures()
     {
        for case in cases() {
            let calls = Arc::new(AtomicUsize::new(0));
            let count = calls.clone();
            let primary = case.inner.clone();
            let base = BoxCloneService::new(tower::service_fn(move |request: RouterRequest| {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                let primary = primary.clone();
                async move {
                    let inner = if attempt == 0 {
                        primary
                    } else {
                        tokio::time::sleep(Duration::from_millis(3)).await;
                        Ok(McpResponse::CallTool(CallToolResult::text("hedge winner")))
                    };
                    Ok::<_, Infallible>(RouterResponse {
                        id: request.id,
                        inner,
                    })
                }
            }));
            let layer = tower_resilience::hedge::HedgeLayer::builder()
                .delay(Duration::from_millis(1))
                .max_hedged_attempts(2)
                .build();
            let response = hedged(base, &layer).oneshot(request(50)).await.unwrap();
            assert_eq!(response.id, RequestId::Number(50));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if case.hedge_failure { 2 } else { 1 },
                "hedge {}",
                case.name
            );
            if case.hedge_failure {
                let Ok(McpResponse::CallTool(result)) = response.inner else {
                    panic!("hedge failed {}", case.name);
                };
                assert_eq!(result.all_text(), "hedge winner");
            } else {
                assert_eq!(
                    serde_json::to_value(response.inner).unwrap(),
                    serde_json::to_value(&case.inner).unwrap()
                );
            }
            // No successful attempt: retain original code/data/isError/id.
            let primary = case.inner.clone();
            let count = Arc::new(AtomicUsize::new(0));
            let base = BoxCloneService::new(tower::service_fn(move |request: RouterRequest| {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                let primary = primary.clone();
                async move {
                    let inner = if attempt == 0 {
                        primary
                    } else {
                        // A distinct failed hedge must never replace the original failure.
                        Err(JsonRpcError::internal_error("different hedge failure"))
                    };
                    Ok::<_, Infallible>(RouterResponse {
                        id: request.id,
                        inner,
                    })
                }
            }));
            let response = hedged(base, &layer).oneshot(request(51)).await.unwrap();
            assert_eq!(response.id, RequestId::Number(51));
            assert_eq!(
                serde_json::to_value(response.inner).unwrap(),
                serde_json::to_value(case.inner).unwrap()
            );
        }
    }
}
