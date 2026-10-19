//! Audit records without request arguments or free-form response/error bodies.
//!
//! Backend error messages may echo credentials or arguments. Keep only the
//! error code in diagnostics, leaving the client-visible response untouched.
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tower::Service;
use tower_mcp::protocol::McpRequest;
use tower_mcp::router::{RouterRequest, RouterResponse, ToolAnnotationsMap};

#[derive(Clone)]
pub(crate) struct AuditService<S> {
    inner: S,
}
impl<S> AuditService<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self { inner }
    }
}
impl<S> Service<RouterRequest> for AuditService<S>
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
        let method = req.inner.method_name().to_string();
        let request_id = format!("{:?}", req.id);
        let (tool, resource_uri, prompt) = match &req.inner {
            McpRequest::CallTool(params) => (Some(params.name.clone()), None, None),
            McpRequest::ReadResource(params) => (None, Some(params.uri.clone()), None),
            McpRequest::SubscribeResource(params) => (None, Some(params.uri.clone()), None),
            McpRequest::UnsubscribeResource(params) => (None, Some(params.uri.clone()), None),
            McpRequest::GetPrompt(params) => (None, None, Some(params.name.clone())),
            _ => (None, None, None),
        };
        let annotations = req.extensions.get::<ToolAnnotationsMap>();
        let read_only = tool
            .as_deref()
            .and_then(|name| annotations.map(|map| map.is_read_only(name)));
        let destructive = tool
            .as_deref()
            .and_then(|name| annotations.map(|map| map.is_destructive(name)));
        let start = Instant::now();
        let future = self.inner.call(req);
        Box::pin(async move {
            let response = future.await?;
            let outcome = crate::outcome::Outcome::from_response(&response);
            let status = match (&response.inner, outcome) {
                (_, crate::outcome::Outcome::Success) => "success",
                (Err(error), crate::outcome::Outcome::ClientError)
                    if error.code == -32602 || error.code == -32007 =>
                {
                    "denied"
                }
                _ => "error",
            };
            let error_code = response.inner.as_ref().err().map(|error| error.code);
            let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(
                target: "mcp::audit", %method, %request_id,
                tool = tool.as_deref(), resource_uri = resource_uri.as_deref(),
                prompt = prompt.as_deref(), duration_ms, status, error_code,
                read_only, destructive, outcome = outcome.as_str(), "audit",
            );
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tower_mcp::protocol::RequestId;
    use tower_mcp::router::Extensions;
    use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

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

    #[test]
    fn audit_preserves_metadata_and_client_errors_without_logging_private_bodies() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || Output(writer.clone()))
            .finish();
        let annotations = McpRouter::new()
            .tool(
                ToolBuilder::new("read")
                    .read_only_safe()
                    .handler(|_: Value| async { Ok(CallToolResult::text("unused")) })
                    .build(),
            )
            .tool_annotations_map();
        let private_error = "private-error-echoing-sensitive-argument";
        let inner = tower::service_fn(move |request: RouterRequest| async move {
            Ok::<_, Infallible>(RouterResponse {
                id: request.id,
                inner: Err(tower_mcp_types::JsonRpcError::internal_error(private_error)),
            })
        });
        let mut service = AuditService::new(inner);
        let requests = [
            McpRequest::CallTool(
                serde_json::from_value(
                    json!({"name":"read","arguments":{"secret":"private-argument"}}),
                )
                .unwrap(),
            ),
            McpRequest::ReadResource(
                serde_json::from_value(json!({"uri":"fixture://read"})).unwrap(),
            ),
            McpRequest::GetPrompt(
                serde_json::from_value(
                    json!({"name":"review","arguments":{"secret":"private-argument"}}),
                )
                .unwrap(),
            ),
            McpRequest::SubscribeResource(
                serde_json::from_value(json!({"uri":"fixture://subscribe"})).unwrap(),
            ),
            McpRequest::UnsubscribeResource(
                serde_json::from_value(json!({"uri":"fixture://unsubscribe"})).unwrap(),
            ),
            McpRequest::Ping,
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                for (index, request) in requests.into_iter().enumerate() {
                    let mut extensions = Extensions::new();
                    extensions.insert(annotations.clone());
                    let id = RequestId::String(format!("caller-{index}"));
                    let response = service
                        .call(RouterRequest {
                            id: id.clone(),
                            inner: request,
                            extensions,
                        })
                        .await
                        .unwrap();
                    assert_eq!(response.id, id);
                    assert_eq!(response.inner.unwrap_err().message, private_error);
                }
            })
        });
        let bytes = output.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("private-"));
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 6);
        for (index, (field, value)) in [
            ("tool", "read"),
            ("resource_uri", "fixture://read"),
            ("prompt", "review"),
            ("resource_uri", "fixture://subscribe"),
            ("resource_uri", "fixture://unsubscribe"),
            ("method", "ping"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(records[index]["target"], "mcp::audit");
            let fields = &records[index]["fields"];
            assert_eq!(fields[field], value);
            assert!(
                fields["request_id"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("caller-{index}"))
            );
            assert_eq!(fields["error_code"], -32603);
            assert_eq!(fields["status"], "error");
            assert!(fields.get("error_message").is_none());
        }
        assert_eq!(records[0]["fields"]["read_only"], true);
        assert_eq!(records[0]["fields"]["destructive"], false);
    }
}
