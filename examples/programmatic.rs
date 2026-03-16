//! Programmatic proxy construction -- build a proxy entirely in code.
//!
//! This example uses ProxyBuilder to construct a proxy without any config
//! files. All backends, middleware, auth, and observability are configured
//! via the fluent builder API.
//!
//! Usage:
//!   cargo run --example programmatic
//!
//! The proxy starts on 127.0.0.1:8080 with two in-process backends
//! (math and text) and bearer token auth.

use std::time::Duration;

use anyhow::Result;
use mcp_proxy::ProxyBuilder;
use mcp_proxy::config::TimeoutConfig;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("tower_mcp=info,mcp_proxy=info")
        .init();

    let proxy = ProxyBuilder::new("programmatic-example")
        .version("1.0.0")
        .listen("127.0.0.1", 8080)
        // Add backends
        .http_backend("api", "http://localhost:9090")
        .configure_backend(|b| {
            b.timeout = Some(TimeoutConfig { seconds: 30 });
            b.hide_tools = vec!["dangerous_op".to_string()];
        })
        .stdio_backend("echo", "echo", &["hello"])
        // Global settings
        .bearer_auth(vec!["my-secret-token".to_string()])
        .global_rate_limit(100, Duration::from_secs(1))
        .coalesce_requests(true)
        .max_argument_size(1_048_576)
        // Observability
        .access_logging(true)
        .log_level("info")
        .build()
        .await?;

    tracing::info!("Programmatic proxy ready");
    proxy.serve().await
}
