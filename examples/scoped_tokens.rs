//! Per-token tool scoping -- restrict tool access per bearer token.
//!
//! Demonstrates configuring bearer tokens where each token has its own
//! allow/deny list for tools. Bridges the gap between all-or-nothing
//! bearer auth and full JWT/RBAC.
//!
//! Run with: `cargo run --example scoped_tokens`

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = mcp_proxy::ProxyConfig::parse(
        r#"
        [proxy]
        name = "scoped-proxy"
        [proxy.listen]

        [[backends]]
        name = "files"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "db"
        transport = "stdio"
        command = "echo"

        [auth]
        type = "bearer"
        # Admin token -- unrestricted access to all tools
        tokens = ["admin-token-xxx"]

        # Frontend token -- read-only file access
        [[auth.scoped_tokens]]
        token = "frontend-token-xxx"
        allow_tools = ["files/read_file", "files/list_directory"]

        # Analytics token -- database read access, no file access
        [[auth.scoped_tokens]]
        token = "analytics-token-xxx"
        allow_tools = ["db/query"]

        # Operator token -- everything except destructive ops
        [[auth.scoped_tokens]]
        token = "operator-token-xxx"
        deny_tools = ["db/drop_table", "files/delete_file"]
        "#,
    )?;

    println!("Scoped token config parsed:");
    match &config.auth {
        Some(mcp_proxy::config::AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) => {
            println!("  Unrestricted tokens: {}", tokens.len());
            println!("  Scoped tokens: {}", scoped_tokens.len());
            for st in scoped_tokens {
                let scope = if !st.allow_tools.is_empty() {
                    format!("allow: {:?}", st.allow_tools)
                } else if !st.deny_tools.is_empty() {
                    format!("deny: {:?}", st.deny_tools)
                } else {
                    "unrestricted".to_string()
                };
                println!("    - {}", scope);
            }
        }
        _ => println!("  (unexpected auth type)"),
    }

    Ok(())
}
