//! Support for `.mcp.json` / `mcp.json` config format.
//!
//! The `.mcp.json` format is the standard MCP client config used by Claude
//! Desktop, VS Code, Claude Code, and other MCP clients. This module parses
//! that format and converts entries into [`BackendConfig`] values that the
//! proxy can use.
//!
//! # Format
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "github": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-github"],
//!       "env": { "GITHUB_TOKEN": "..." }
//!     },
//!     "api": {
//!       "url": "http://localhost:8080"
//!     }
//!   }
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{BackendConfig, TransportType};

/// Top-level `.mcp.json` structure.
#[derive(Debug, Deserialize)]
pub struct McpJsonConfig {
    /// Map of server name to server configuration.
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpJsonServer>,
}

/// A single MCP server entry in `.mcp.json`.
#[derive(Debug, Deserialize)]
pub struct McpJsonServer {
    /// Command to run (stdio transport).
    pub command: Option<String>,
    /// Arguments for the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// URL for HTTP transport.
    pub url: Option<String>,
}

impl McpJsonConfig {
    /// Load and parse a `.mcp.json` file.
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse a `.mcp.json` string.
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("parsing .mcp.json")
    }

    /// Convert all server entries into [`BackendConfig`] values.
    ///
    /// Server names become backend names. Entries with `command` become stdio
    /// backends; entries with `url` become HTTP backends.
    pub fn into_backends(self) -> Result<Vec<BackendConfig>> {
        let mut backends = Vec::new();

        for (name, server) in self.mcp_servers {
            let backend = server_to_backend(name, server)?;
            backends.push(backend);
        }

        // Sort by name for deterministic ordering
        backends.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(backends)
    }
}

/// Convert a single `.mcp.json` server entry to a `BackendConfig`.
fn server_to_backend(name: String, server: McpJsonServer) -> Result<BackendConfig> {
    let (transport, command, url) = if let Some(command) = server.command {
        (TransportType::Stdio, Some(command), None)
    } else if let Some(url) = server.url {
        (TransportType::Http, None, Some(url))
    } else {
        anyhow::bail!(
            "server '{}': must have either 'command' (stdio) or 'url' (http)",
            name
        );
    };

    Ok(BackendConfig {
        name,
        transport,
        command,
        args: server.args,
        url,
        env: server.env,
        bearer_token: None,
        forward_auth: false,
        timeout: None,
        circuit_breaker: None,
        rate_limit: None,
        concurrency: None,
        retry: None,
        outlier_detection: None,
        hedging: None,
        cache: None,
        default_args: serde_json::Map::new(),
        inject_args: Vec::new(),
        expose_tools: Vec::new(),
        hide_tools: Vec::new(),
        expose_resources: Vec::new(),
        hide_resources: Vec::new(),
        expose_prompts: Vec::new(),
        hide_prompts: Vec::new(),
        failover_for: None,
        canary_of: None,
        weight: 100,
        aliases: Vec::new(),
        mirror_of: None,
        mirror_percent: 100,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stdio_server() {
        let json = r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "secret" }
                }
            }
        }"#;

        let config = McpJsonConfig::parse(json).unwrap();
        let backends = config.into_backends().unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].name, "github");
        assert!(matches!(backends[0].transport, TransportType::Stdio));
        assert_eq!(backends[0].command.as_deref(), Some("npx"));
        assert_eq!(
            backends[0].args,
            vec!["-y", "@modelcontextprotocol/server-github"]
        );
        assert_eq!(backends[0].env.get("GITHUB_TOKEN").unwrap(), "secret");
    }

    #[test]
    fn test_parse_http_server() {
        let json = r#"{
            "mcpServers": {
                "api": {
                    "url": "http://localhost:8080"
                }
            }
        }"#;

        let config = McpJsonConfig::parse(json).unwrap();
        let backends = config.into_backends().unwrap();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].name, "api");
        assert!(matches!(backends[0].transport, TransportType::Http));
        assert_eq!(backends[0].url.as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn test_parse_multiple_servers() {
        let json = r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"]
                },
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home"]
                },
                "api": {
                    "url": "http://localhost:8080"
                }
            }
        }"#;

        let config = McpJsonConfig::parse(json).unwrap();
        let backends = config.into_backends().unwrap();
        assert_eq!(backends.len(), 3);
        // Sorted by name
        assert_eq!(backends[0].name, "api");
        assert_eq!(backends[1].name, "filesystem");
        assert_eq!(backends[2].name, "github");
    }

    #[test]
    fn test_rejects_server_without_command_or_url() {
        let json = r#"{
            "mcpServers": {
                "bad": {
                    "args": ["--help"]
                }
            }
        }"#;

        let config = McpJsonConfig::parse(json).unwrap();
        let err = config.into_backends().unwrap_err();
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn test_empty_servers() {
        let json = r#"{ "mcpServers": {} }"#;
        let config = McpJsonConfig::parse(json).unwrap();
        let backends = config.into_backends().unwrap();
        assert!(backends.is_empty());
    }

    #[test]
    fn test_default_env_and_args() {
        let json = r#"{
            "mcpServers": {
                "simple": {
                    "command": "echo"
                }
            }
        }"#;

        let config = McpJsonConfig::parse(json).unwrap();
        let backends = config.into_backends().unwrap();
        assert!(backends[0].args.is_empty());
        assert!(backends[0].env.is_empty());
    }
}
