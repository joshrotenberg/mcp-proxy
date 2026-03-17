//! BM25-based tool discovery and search.
//!
//! When `tool_discovery = true` is set in the proxy config, this module
//! indexes all tools from all backends using jpx-engine's BM25 search and
//! exposes discovery tools under the `proxy/` namespace:
//!
//! - `proxy/search_tools` -- Full-text search across tool names, descriptions, and tags
//! - `proxy/similar_tools` -- Find tools related to a given tool
//! - `proxy/tool_categories` -- Browse tools by backend category

use std::sync::Arc;

use jpx_engine::{
    CategorySummary, DiscoveryRegistry, DiscoverySpec, ParamSpec, ServerInfo, ToolQueryResult,
    ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower_mcp::proxy::McpProxy;
use tower_mcp::{CallToolResult, NoParams, ToolBuilder, ToolDefinition};

/// Shared discovery index, wrapped for concurrent access from tool handlers.
pub type SharedDiscoveryIndex = Arc<RwLock<DiscoveryRegistry>>;

/// Build a discovery index from the proxy's current tool list.
///
/// Sends a `ListTools` request through the proxy to collect all registered
/// tools, then indexes them using jpx-engine's BM25 search.
pub async fn build_index(proxy: &mut McpProxy, separator: &str) -> SharedDiscoveryIndex {
    use tower::Service;
    use tower_mcp::protocol::{ListToolsParams, McpRequest, McpResponse, RequestId};
    use tower_mcp::router::{Extensions, RouterRequest};

    let req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let tools = match proxy.call(req).await {
        Ok(resp) => match resp.inner {
            Ok(McpResponse::ListTools(result)) => result.tools,
            _ => {
                tracing::warn!("Failed to list tools for discovery indexing");
                vec![]
            }
        },
        Err(_) => vec![],
    };

    let mut registry = DiscoveryRegistry::new();
    index_tools(&mut registry, &tools, separator);

    tracing::info!(tools_indexed = tools.len(), "Built tool discovery index");

    Arc::new(RwLock::new(registry))
}

/// Index MCP tool definitions into the discovery registry.
///
/// Groups tools by backend namespace (derived from the separator) and registers
/// each group as a discovery "server" with its tools.
fn index_tools(registry: &mut DiscoveryRegistry, tools: &[ToolDefinition], separator: &str) {
    // Group tools by backend namespace
    let mut by_namespace: std::collections::HashMap<String, Vec<&ToolDefinition>> =
        std::collections::HashMap::new();

    for tool in tools {
        let namespace = tool
            .name
            .split_once(separator)
            .map(|(ns, _)| ns.to_string())
            .unwrap_or_else(|| "default".to_string());
        by_namespace.entry(namespace).or_default().push(tool);
    }

    for (namespace, ns_tools) in &by_namespace {
        let tool_specs: Vec<ToolSpec> = ns_tools
            .iter()
            .map(|t| tool_definition_to_spec(t, separator))
            .collect();

        let spec = DiscoverySpec {
            schema: None,
            server: ServerInfo {
                name: namespace.clone(),
                version: None,
                description: None,
            },
            tools: tool_specs,
            categories: std::collections::HashMap::new(),
        };

        registry.register(spec, true);
    }
}

/// Convert an MCP ToolDefinition to a jpx ToolSpec for indexing.
fn tool_definition_to_spec(tool: &ToolDefinition, separator: &str) -> ToolSpec {
    // Extract the local tool name (without namespace prefix)
    let local_name = tool
        .name
        .split_once(separator)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| tool.name.clone());

    // Extract parameter names from input schema
    let params = extract_params(&tool.input_schema);

    // Extract tags from annotations if available
    let mut tags = Vec::new();
    if let Some(annotations) = &tool.annotations {
        if annotations.destructive_hint {
            tags.push("destructive".to_string());
        }
        if annotations.read_only_hint {
            tags.push("read-only".to_string());
        }
        if annotations.idempotent_hint {
            tags.push("idempotent".to_string());
        }
        if annotations.open_world_hint {
            tags.push("open-world".to_string());
        }
    }

    // Extract category from namespace
    let category = tool
        .name
        .split_once(separator)
        .map(|(ns, _)| ns.to_string());

    ToolSpec {
        name: local_name,
        aliases: vec![],
        category,
        subcategory: None,
        tags,
        summary: tool.description.clone(),
        description: tool.description.clone(),
        params,
        returns: None,
        examples: vec![],
        related: vec![],
        since: None,
        stability: None,
    }
}

/// Extract parameter specs from a JSON Schema input_schema.
fn extract_params(schema: &serde_json::Value) -> Vec<ParamSpec> {
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return vec![];
    };
    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    properties
        .iter()
        .map(|(name, prop)| ParamSpec {
            name: name.clone(),
            param_type: prop.get("type").and_then(|t| t.as_str()).map(String::from),
            required: required.contains(name.as_str()),
            description: prop
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from),
            enum_values: None,
            default: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Discovery tool handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchInput {
    /// Search query (e.g. "read file", "database query", "math operations")
    query: String,
    /// Maximum number of results to return (default: 10)
    #[serde(default = "default_top_k")]
    top_k: usize,
}

fn default_top_k() -> usize {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SimilarInput {
    /// Tool ID to find similar tools for (e.g. "math:add")
    tool_id: String,
    /// Maximum number of results to return (default: 5)
    #[serde(default = "default_similar_k")]
    top_k: usize,
}

fn default_similar_k() -> usize {
    5
}

#[derive(Serialize)]
struct SearchResultEntry {
    id: String,
    server: String,
    name: String,
    description: Option<String>,
    score: f64,
    tags: Vec<String>,
    category: Option<String>,
}

impl From<ToolQueryResult> for SearchResultEntry {
    fn from(r: ToolQueryResult) -> Self {
        Self {
            id: r.id,
            server: r.server,
            name: r.tool.name,
            description: r.tool.description,
            score: r.score,
            tags: r.tool.tags,
            category: r.tool.category,
        }
    }
}

#[derive(Serialize)]
struct CategoriesResult {
    categories: Vec<CategorySummary>,
    total_categories: usize,
}

/// Build the discovery tools and return them for inclusion in the admin router.
pub fn build_discovery_tools(index: SharedDiscoveryIndex) -> Vec<tower_mcp::Tool> {
    let index_for_search = Arc::clone(&index);
    let search_tools = ToolBuilder::new("search_tools")
        .description(
            "Search for tools across all backends using BM25 full-text search. \
             Searches tool names, descriptions, parameters, and tags.",
        )
        .handler(move |input: SearchInput| {
            let idx = Arc::clone(&index_for_search);
            async move {
                let registry = idx.read().await;
                let results = registry.query(&input.query, input.top_k);
                let entries: Vec<SearchResultEntry> =
                    results.into_iter().map(SearchResultEntry::from).collect();
                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&entries).unwrap(),
                ))
            }
        })
        .build();

    let index_for_similar = Arc::clone(&index);
    let similar_tools = ToolBuilder::new("similar_tools")
        .description(
            "Find tools similar to a given tool. Uses BM25 similarity based on \
             shared terms in descriptions, parameters, and tags.",
        )
        .handler(move |input: SimilarInput| {
            let idx = Arc::clone(&index_for_similar);
            async move {
                let registry = idx.read().await;
                let results = registry.similar(&input.tool_id, input.top_k);
                let entries: Vec<SearchResultEntry> =
                    results.into_iter().map(SearchResultEntry::from).collect();
                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&entries).unwrap(),
                ))
            }
        })
        .build();

    let index_for_categories = Arc::clone(&index);
    let tool_categories = ToolBuilder::new("tool_categories")
        .description(
            "List all tool categories (backend namespaces) with tool counts. \
             Useful for browsing available capabilities by domain.",
        )
        .handler(move |_: NoParams| {
            let idx = Arc::clone(&index_for_categories);
            async move {
                let registry = idx.read().await;
                let categories = registry.list_categories();
                let mut cats: Vec<CategorySummary> = categories.into_values().collect();
                cats.sort_by(|a, b| b.tool_count.cmp(&a.tool_count));
                let result = CategoriesResult {
                    total_categories: cats.len(),
                    categories: cats,
                };
                Ok(CallToolResult::text(
                    serde_json::to_string_pretty(&result).unwrap(),
                ))
            }
        })
        .build();

    vec![search_tools, similar_tools, tool_categories]
}
