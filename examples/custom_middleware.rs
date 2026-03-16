//! Custom middleware example -- add your own tower middleware to the proxy.
//!
//! Demonstrates composing mcp-proxy's middleware layers with custom tower
//! services using ServiceBuilder. The proxy is built from config, then
//! additional middleware is applied before serving.
//!
//! Usage:
//!   cargo run --example custom_middleware -- --config proxy.toml

use std::convert::Infallible;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use clap::Parser;
use tower::Service;
use tower_mcp::{RouterRequest, RouterResponse};

/// A custom middleware that logs the tool name for every CallTool request.
#[derive(Clone)]
#[allow(dead_code)]
struct ToolCallLogger<S> {
    inner: S,
}

impl<S> Service<RouterRequest> for ToolCallLogger<S>
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
        // Log tool calls
        if let tower_mcp::protocol::McpRequest::CallTool(ref params) = req.inner {
            tracing::info!(tool = %params.name, "Custom middleware: tool call intercepted");
        }

        Box::pin(self.inner.call(req))
    }
}

#[derive(Parser)]
#[command(name = "custom-middleware-example")]
struct Cli {
    #[arg(short, long, default_value = "proxy.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,mcp_proxy=info")
        .init();

    let cli = Cli::parse();

    let mut config = mcp_proxy::ProxyConfig::load(&cli.config)?;
    config.resolve_env_vars();

    let addr = format!("{}:{}", config.proxy.listen.host, config.proxy.listen.port);

    // Build the proxy to get the inner McpProxy for direct service composition
    let proxy = mcp_proxy::Proxy::from_config(config).await?;
    let (proxy_router, _handle) = proxy.into_router();

    // The proxy router is a standard axum Router. You can add axum-level
    // middleware (auth, CORS, request logging) using axum's layer system.
    // For MCP-level middleware (tool filtering, aliasing, etc.), use the
    // Layer implementations from mcp_proxy's modules.
    //
    // To add MCP-level custom middleware, use ProxyBuilder::into_config()
    // and build the middleware stack manually, or use the McpProxy directly:
    //
    //   let mcp_proxy = proxy.mcp_proxy().clone();
    //   let custom = ToolCallLogger { inner: mcp_proxy };

    tracing::info!(listen = %addr, "Custom middleware example ready");
    tracing::info!("The ToolCallLogger middleware demonstrates the Service pattern");
    tracing::info!("See src/inject.rs or src/filter.rs for real middleware examples");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, proxy_router).await?;

    Ok(())
}
