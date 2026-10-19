//! Resolve search-mode calls without crossing the backend transport boundary.
//!
//! Rewriting the original request preserves identity, cancellation, continuation,
//! task, and progress context. The inner service is the ordinary invocation
//! stack, excluding this resolver, so wrappers cannot recursively invoke it.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::{McpRequest, RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;

#[derive(Clone)]
pub(crate) struct SearchInvocation<S> {
    inner: S,
    prefix: String,
    wrapper: String,
}

impl<S> SearchInvocation<S> {
    pub(crate) fn new(inner: S, separator: &str) -> Self {
        let prefix = format!("proxy{separator}");
        let wrapper = format!("{prefix}call_tool");
        Self {
            inner,
            prefix,
            wrapper,
        }
    }
}

impl<S> Service<RouterRequest> for SearchInvocation<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
    S::Future: Send + 'static,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        if let McpRequest::CallTool(params) = &mut req.inner
            && params.name == self.wrapper
        {
            let input = serde_json::from_value::<crate::admin_tools::CallToolInput>(
                params.arguments.clone(),
            );
            let input = match input {
                Ok(input) if !input.name.is_empty() && !input.name.starts_with(&self.prefix) => {
                    input
                }
                _ => {
                    return Box::pin(async move {
                        Ok(RouterResponse {
                            id: req.id,
                            inner: Err(JsonRpcError::invalid_params(
                                "proxy/call_tool requires an ordinary target name and object arguments",
                            )),
                        })
                    });
                }
            };
            params.name = input.name;
            params.arguments = input.arguments.unwrap_or_default().into();
        }
        Box::pin(self.inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower::ServiceExt;
    use tower_mcp::{Extensions, McpResponse, RequestId};

    #[tokio::test]
    async fn preserves_request_context_and_continuation_fields() {
        let context = tower_mcp::RequestContext::new(RequestId::Number(42));
        let cancellation = context.cancellation_token();
        let expected = json!({
            "name":"api/echo", "arguments":{"value":1},
            "inputResponses":{}, "requestState":"resume-state",
            "_meta":{"progressToken":"progress-42"}, "task":{"ttl":60000}
        });
        let inner = tower::service_fn(move |req: RouterRequest| {
            let expected = expected.clone();
            async move {
                assert_eq!(req.id, RequestId::Number(42));
                assert_eq!(req.extensions.get::<String>().unwrap(), "verified-caller");
                assert!(
                    req.extensions
                        .get::<tower_mcp::RequestContext>()
                        .unwrap()
                        .is_cancelled()
                );
                let McpRequest::CallTool(params) = req.inner else {
                    panic!("expected call")
                };
                assert_eq!(serde_json::to_value(params).unwrap(), expected);
                Ok::<_, Infallible>(RouterResponse {
                    id: req.id,
                    inner: Ok(McpResponse::CallTool(tower_mcp::CallToolResult::text("ok"))),
                })
            }
        });
        let mut extensions = Extensions::new();
        extensions.insert(String::from("verified-caller"));
        extensions.insert(context);
        cancellation.cancel();
        let params = serde_json::from_value(json!({
            "name":"proxy/call_tool", "arguments":{"name":"api/echo","arguments":{"value":1}},
            "inputResponses":{}, "requestState":"resume-state",
            "_meta":{"progressToken":"progress-42"}, "task":{"ttl":60000}
        }))
        .unwrap();
        SearchInvocation::new(inner, "/")
            .oneshot(RouterRequest {
                id: RequestId::Number(42),
                inner: McpRequest::CallTool(params),
                extensions,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_and_recursive_targets_before_dispatch() {
        let inner = tower::service_fn(
            |_: RouterRequest| -> std::future::Ready<Result<RouterResponse, Infallible>> {
                panic!("invalid wrapper must never dispatch")
            },
        );
        for arguments in [
            json!({}),
            json!({"name":""}),
            json!({"name":"api/echo","arguments":[]}),
            json!({"name":"proxy::call_tool"}),
            json!({"name":"proxy::config"}),
            json!({"name":"proxy::search_tools"}),
        ] {
            let params =
                serde_json::from_value(json!({"name":"proxy::call_tool","arguments":arguments}))
                    .unwrap();
            let result = SearchInvocation::new(inner, "::")
                .oneshot(RouterRequest {
                    id: RequestId::Number(1),
                    inner: McpRequest::CallTool(params),
                    extensions: Extensions::new(),
                })
                .await
                .unwrap();
            assert!(result.inner.is_err());
        }
    }
}
