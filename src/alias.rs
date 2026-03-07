//! Tool aliasing middleware for the gateway.
//!
//! Rewrites tool names in list responses and call requests based on
//! per-backend alias configuration.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::Service;
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{McpRequest, McpResponse};

/// Resolved alias mappings for all backends.
#[derive(Clone)]
pub struct AliasMap {
    /// Maps "namespace/original" -> "namespace/alias" (for list responses)
    pub forward: HashMap<String, String>,
    /// Maps "namespace/alias" -> "namespace/original" (for call requests)
    reverse: HashMap<String, String>,
}

impl AliasMap {
    pub fn new(mappings: Vec<(String, String, String)>) -> Option<Self> {
        if mappings.is_empty() {
            return None;
        }
        let mut forward = HashMap::new();
        let mut reverse = HashMap::new();
        for (namespace, from, to) in mappings {
            let original = format!("{}{}", namespace, from);
            let aliased = format!("{}{}", namespace, to);
            forward.insert(original.clone(), aliased.clone());
            reverse.insert(aliased, original);
        }
        Some(Self { forward, reverse })
    }
}

/// Tower service that rewrites tool names based on alias configuration.
#[derive(Clone)]
pub struct AliasService<S> {
    inner: S,
    aliases: Arc<AliasMap>,
}

impl<S> AliasService<S> {
    pub fn new(inner: S, aliases: AliasMap) -> Self {
        Self {
            inner,
            aliases: Arc::new(aliases),
        }
    }
}

impl<S> Service<RouterRequest> for AliasService<S>
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

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        let aliases = Arc::clone(&self.aliases);

        // Reverse-map aliased names back to originals in requests
        match &mut req.inner {
            McpRequest::CallTool(params) => {
                if let Some(original) = aliases.reverse.get(&params.name) {
                    params.name = original.clone();
                }
            }
            McpRequest::ReadResource(params) => {
                if let Some(original) = aliases.reverse.get(&params.uri) {
                    params.uri = original.clone();
                }
            }
            McpRequest::GetPrompt(params) => {
                if let Some(original) = aliases.reverse.get(&params.name) {
                    params.name = original.clone();
                }
            }
            _ => {}
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut result = fut.await;

            // Forward-map original names to aliases in responses
            let Ok(ref mut resp) = result;
            if let Ok(mcp_resp) = &mut resp.inner {
                match mcp_resp {
                    McpResponse::ListTools(r) => {
                        for tool in &mut r.tools {
                            if let Some(aliased) = aliases.forward.get(&tool.name) {
                                tool.name = aliased.clone();
                            }
                        }
                    }
                    McpResponse::ListResources(r) => {
                        for res in &mut r.resources {
                            if let Some(aliased) = aliases.forward.get(&res.uri) {
                                res.uri = aliased.clone();
                            }
                        }
                    }
                    McpResponse::ListPrompts(r) => {
                        for prompt in &mut r.prompts {
                            if let Some(aliased) = aliases.forward.get(&prompt.name) {
                                prompt.name = aliased.clone();
                            }
                        }
                    }
                    _ => {}
                }
            }

            result
        })
    }
}
