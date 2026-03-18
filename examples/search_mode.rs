//! Search mode -- expose meta-tools instead of individual tools.
//!
//! When aggregating many backends with hundreds of tools, listing them all
//! overwhelms LLM context. Search mode replaces N tool registrations with
//! meta-tools that agents use to discover and invoke tools on demand.
//!
//! Run with: `cargo run --example search_mode --features discovery`

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = mcp_proxy::ProxyConfig::parse(
        r#"
        [proxy]
        name = "search-proxy"
        # Search mode: hide individual tools, expose meta-tools only
        tool_exposure = "search"
        # tool_discovery = true   # implied by search mode
        [proxy.listen]

        # Imagine 10+ backends with dozens of tools each...
        [[backends]]
        name = "github"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "filesystem"
        transport = "stdio"
        command = "echo"

        [[backends]]
        name = "database"
        transport = "stdio"
        command = "echo"
        "#,
    )?;

    println!("Search mode config:");
    println!("  Tool exposure: {:?}", config.proxy.tool_exposure);
    println!("  Backends: {}", config.backends.len());
    println!();
    println!("In search mode, ListTools returns only:");
    println!("  - proxy/search_tools    (BM25 full-text search)");
    println!("  - proxy/similar_tools   (find related tools)");
    println!("  - proxy/tool_categories (browse by backend)");
    println!("  - proxy/call_tool       (invoke any tool by name)");
    println!();
    println!("Agents discover tools via search, then invoke via call_tool.");
    println!("Backend tools still work -- they're just hidden from listing.");

    Ok(())
}
