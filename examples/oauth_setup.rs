//! OAuth 2.1 authentication setup -- connect to an identity provider.
//!
//! Demonstrates configuring OAuth 2.1 auth with auto-discovery from an
//! issuer URL. The proxy automatically discovers JWKS and introspection
//! endpoints via RFC 8414 metadata.
//!
//! Run with: `cargo run --example oauth_setup`

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // OAuth config with auto-discovery
    let config = mcp_proxy::ProxyConfig::parse(
        r#"
        [proxy]
        name = "oauth-proxy"
        [proxy.listen]
        host = "0.0.0.0"
        port = 8080

        [[backends]]
        name = "api"
        transport = "stdio"
        command = "echo"

        # OAuth 2.1 with auto-discovery from issuer
        [auth]
        type = "oauth"
        issuer = "https://accounts.google.com"
        audience = "mcp-proxy"
        # token_validation = "jwt"        # default: validate JWTs via JWKS
        # token_validation = "introspection"  # validate opaque tokens
        # token_validation = "both"       # try JWT first, fall back to introspection

        # For introspection mode, provide client credentials:
        # client_id = "my-client-id"
        # client_secret = "${OAUTH_CLIENT_SECRET}"

        # RBAC roles (optional, same as JWT auth)
        # [[auth.roles]]
        # name = "reader"
        # allow_tools = ["api/read_*"]
        #
        # [auth.role_mapping]
        # claim = "scope"
        # mapping = { "mcp:read" = "reader" }
        "#,
    )?;

    println!("OAuth config parsed successfully:");
    println!("  Auth type: OAuth 2.1");
    println!("  Backends: {}", config.backends.len());

    // In production, build and serve:
    // let proxy = mcp_proxy::Proxy::from_config(config).await?;
    // proxy.serve().await?;

    Ok(())
}
