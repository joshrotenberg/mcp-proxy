//! MCP Proxy -- config-driven reverse proxy with auth, rate limiting, and observability.
//!
//! This crate can be used as a library to embed an MCP proxy in your application,
//! or run standalone via the `mcp-proxy` CLI.
//!
//! # Library Usage
//!
//! Build a proxy from a [`ProxyConfig`] and embed it in an existing axum app:
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
//! # Hot Reload
//!
//! Enable `hot_reload = true` in the config to watch the config file for new
//! backends. The proxy will add them dynamically without restart.

pub mod admin;
pub mod admin_tools;
pub mod alias;
pub mod cache;
pub mod canary;
pub mod coalesce;
pub mod config;
pub mod filter;
pub mod inject;
pub mod metrics;
pub mod mirror;
pub mod outlier;
pub mod rbac;
pub mod reload;
pub mod retry;
pub mod token;
pub mod validation;

#[cfg(test)]
mod test_util;

mod proxy;

pub use config::ProxyConfig;
pub use proxy::Proxy;
