//! MCP Proxy -- config-driven reverse proxy with auth, rate limiting, and observability.
//!
//! This crate can be used as a library to embed an MCP proxy in your application,
//! or run standalone via the `mcp-proxy` CLI.
//!
//! # Library Usage
//!
//! Build a proxy from a TOML config file:
//!
//! ```rust,no_run
//! use mcp_proxy::{Proxy, ProxyConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = ProxyConfig::load("proxy.toml".as_ref())?;
//! let proxy = Proxy::from_config(config).await?;
//!
//! // Embed in an existing axum app
//! let (router, session_handle) = proxy.into_router();
//!
//! // Or serve standalone
//! // proxy.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Or build programmatically with [`ProxyBuilder`]:
//!
//! ```rust,no_run
//! use mcp_proxy::ProxyBuilder;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let proxy = ProxyBuilder::new("my-proxy")
//!     .listen("0.0.0.0", 9090)
//!     .http_backend("api", "http://api:8080")
//!     .stdio_backend("files", "npx", &["-y", "@mcp/server-files"])
//!     .build()
//!     .await?;
//!
//! proxy.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Hot Reload
//!
//! Enable `hot_reload = true` in the config to watch the config file for new
//! backends. The proxy will add them dynamically without restart.

pub mod access_log;
pub mod admin;
pub mod admin_tools;
pub mod alias;
pub mod builder;
pub mod cache;
pub mod canary;
pub mod coalesce;
pub mod config;
pub mod failover;
pub mod filter;
pub mod inject;
pub mod mcp_json;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod mirror;
pub mod outlier;
#[cfg(feature = "oauth")]
pub mod rbac;
pub mod reload;
pub mod retry;
#[cfg(feature = "oauth")]
pub mod token;
pub mod validation;

#[cfg(test)]
mod test_util;

mod proxy;

pub use builder::ProxyBuilder;
pub use config::ProxyConfig;
pub use proxy::Proxy;
