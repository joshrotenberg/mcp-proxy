//! Dynamic backend management -- add and remove backends at runtime.
//!
//! This example starts a proxy with one backend, then dynamically adds
//! and removes backends using the McpProxy API. This is the same API
//! used by hot reload and the REST management endpoints.
//!
//! Usage:
//!   cargo run --example dynamic_backends -- --config proxy.toml

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(name = "dynamic-backends-example")]
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

    let proxy = mcp_proxy::Proxy::from_config(config).await?;

    // Get a handle to the inner McpProxy for dynamic operations
    let mcp_proxy = proxy.mcp_proxy().clone();

    // Spawn a task that demonstrates dynamic backend management
    tokio::spawn(async move {
        // Wait for the server to start
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // List current backends
        let namespaces = mcp_proxy.backend_namespaces();
        tracing::info!(backends = ?namespaces, "Current backends");

        // Add an HTTP backend dynamically
        let transport = tower_mcp::client::HttpClientTransport::new("http://localhost:9999");
        match mcp_proxy.add_backend("dynamic-api", transport).await {
            Ok(()) => tracing::info!("Added 'dynamic-api' backend"),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to add backend (expected if no server at :9999)")
            }
        }

        // List backends again
        let namespaces = mcp_proxy.backend_namespaces();
        tracing::info!(backends = ?namespaces, "Backends after add");

        // Remove a backend
        if mcp_proxy.remove_backend("dynamic-api").await {
            tracing::info!("Removed 'dynamic-api' backend");
        }

        // Final state
        let namespaces = mcp_proxy.backend_namespaces();
        tracing::info!(backends = ?namespaces, "Backends after remove");
    });

    tracing::info!(listen = %addr, "Dynamic backends example ready");
    proxy.serve().await
}
