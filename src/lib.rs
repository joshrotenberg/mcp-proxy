//! MCP Gateway -- config-driven proxy with auth, rate limiting, and observability.
//!
//! This crate can be used as a library to embed an MCP gateway in your application,
//! or run standalone via the `mcp-gateway` CLI.
//!
//! # Library Usage
//!
//! Build a gateway from a [`GatewayConfig`] and embed it in an existing axum app:
//!
//! ```rust,no_run
//! use mcp_gateway::{Gateway, GatewayConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = GatewayConfig::load("gateway.toml".as_ref())?;
//! let gateway = Gateway::from_config(config).await?;
//!
//! // Embed in an existing axum app
//! let (router, session_handle) = gateway.into_router();
//!
//! // Or serve standalone
//! // gateway.serve().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Hot Reload
//!
//! Enable `hot_reload = true` in the config to watch the config file for new
//! backends. The gateway will add them dynamically without restart.

pub mod admin;
pub mod admin_tools;
pub mod alias;
pub mod cache;
pub mod coalesce;
pub mod config;
pub mod filter;
pub mod metrics;
pub mod outlier;
pub mod rbac;
pub mod reload;
pub mod retry;
pub mod token;
pub mod validation;

#[cfg(test)]
mod test_util;

mod gateway;

pub use config::GatewayConfig;
pub use gateway::Gateway;
