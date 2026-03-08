//! Library mode example -- embed mcp-proxy in a custom axum application.
//!
//! This example loads a proxy config, builds the proxy, and merges its
//! router into an existing axum app alongside custom application routes.
//!
//! Usage:
//!   cargo run --example library-mode -- --config proxy.toml
//!
//! The MCP proxy is available at the root path (/) and custom routes
//! are available alongside it:
//!   GET /app/status  -- custom application endpoint
//!   GET /admin/*     -- proxy admin API (built-in)
//!   POST /           -- MCP HTTP transport (built-in)

use anyhow::Result;
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "library-mode-example")]
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

    // Load and resolve config
    let mut config = mcp_proxy::ProxyConfig::load(&cli.config)?;
    config.resolve_env_vars();

    let addr = format!("{}:{}", config.proxy.listen.host, config.proxy.listen.port);

    // Build the proxy
    let proxy = mcp_proxy::Proxy::from_config(config).await?;

    // Extract the router (includes MCP transport + admin API)
    let (proxy_router, _session_handle) = proxy.into_router();

    // Build custom application routes
    let app_routes = Router::new().route("/app/status", get(app_status));

    // Merge proxy router with custom routes
    let app = proxy_router.merge(app_routes);

    tracing::info!(listen = %addr, "Library mode example ready");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn app_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "app": "my-application",
        "status": "running",
    }))
}
