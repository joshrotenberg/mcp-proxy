//! BM25 full-text tool discovery and search.
//!
//! When a proxy aggregates many backends, clients (and the LLMs behind them)
//! need a way to find relevant tools without scanning a long flat list. This
//! module builds a BM25 search index over all registered tools using
//! [`jpx-engine`](jpx_engine) and exposes four discovery tools under the
//! `proxy/` namespace:
//!
//! | Tool | Description |
//! |---|---|
//! | `proxy/search_tools` | Full-text search across tool names, descriptions, parameters, and tags |
//! | `proxy/get_tool` | Retrieve a tool description and complete input schema by search result ID |
//! | `proxy/similar_tools` | Find tools related to a given tool by BM25 term similarity |
//! | `proxy/tool_categories` | Browse tools grouped by backend namespace with counts |
//!
//! # Enabling discovery
//!
//! Set `tool_discovery = true` in the `[proxy]` section:
//!
//! Configuration fragment: append this block after [backend.toml](https://github.com/joshrotenberg/mcp-proxy/blob/main/tests/fixtures/documentation/backend.toml).
//! Required features: `discovery`.
//!
//! <!-- doc-config: fragment=tests/fixtures/documentation/backend.toml features=discovery -->
//! ```toml
//! [proxy]
//! name = "my-proxy"
//! tool_discovery = true
//!
//! [proxy.listen]
//! # Uses the default loopback host and port.
//! ```
//!
//! The four `proxy/` discovery tools are added to the proxy's tool list
//! alongside the backend tools.
//!
//! # Search mode (`tool_exposure = "search"`)
//!
//! For proxies aggregating a large number of tools (100+), listing every tool
//! in `tools/list` responses can overwhelm LLM context windows. Setting
//! `tool_exposure = "search"` hides individual backend tools from listings
//! while keeping them invokable. Only the `proxy/` meta-tools appear in
//! `tools/list`; clients use `proxy/search_tools` to discover and then call
//! backend tools by name. Use `proxy/get_tool` with a search result ID to inspect
//! its input schema before calling it.
//!
//! Configuration fragment: append this block after [backend.toml](https://github.com/joshrotenberg/mcp-proxy/blob/main/tests/fixtures/documentation/backend.toml).
//! Required features: `discovery`.
//!
//! <!-- doc-config: fragment=tests/fixtures/documentation/backend.toml features=discovery -->
//! ```toml
//! [proxy]
//! name = "my-proxy"
//! tool_exposure = "search"
//! # tool_discovery is implied when tool_exposure = "search"
//! [proxy.listen]
//! ```
//!
//! # Indexing architecture
//!
//! [`crate::Proxy`] keeps a [`DiscoveryCatalog`]. The catalog sends a
//! `ListTools` request through the proxy's public middleware stack, below caller
//! policy, so it indexes the tool names and schemas clients see: aliases,
//! hidden tools, hidden and renamed parameters, locked argument keys and
//! composite tools are applied. Tools are grouped by backend namespace (derived
//! from the configured separator) and registered as [`DiscoverySpec`]s in a
//! [`DiscoveryRegistry`] for BM25 search. Tool annotations (destructive,
//! read-only, idempotent, open-world) are extracted as searchable tags. The
//! registry and the tool definitions `proxy/get_tool` returns live in one
//! snapshot that a refresh replaces in a single step.
//!
//! # Caller visibility
//!
//! Discovery calls are answered by middleware just outside caller policy. For
//! each call it lists tools through policy with the caller's identity and keeps
//! only results that listing contains, so RBAC roles and bearer-token scopes
//! limit search results and `proxy/get_tool` the same way they limit
//! `tools/list`. `proxy/get_tool` answers a tool the caller cannot see with the
//! same error as an unknown id. Search ranks up to four times `top_k`
//! candidates (at most 200) before filtering, so a caller who sees part of the
//! catalog can receive fewer than `top_k` results.
//!
//! `proxy/similar_tools` and `proxy/tool_categories` require management
//! authority and see the whole catalog.
//!
//! # Staying current
//!
//! The catalog refreshes whenever the proxy's tool list changes: hot reload,
//! HTTP and MCP backend management, changes made through
//! [`crate::Proxy::mcp_proxy`], and backend `notifications/tools/list_changed`.
//! Bursts of changes are coalesced. When a refresh cannot list tools, the
//! previous snapshot stays in place.
//!
//! The older [`build_index`], [`reindex`] and [`build_discovery_tools`] helpers
//! index the raw backend catalog without caller filtering. They remain for
//! library users who assemble their own [`McpProxy`], and are deprecated.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use jpx_engine::{
    CategorySummary, DiscoveryRegistry, DiscoverySpec, ParamSpec, ServerInfo, ToolQueryResult,
    ToolSpec,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tower::util::BoxCloneService;
use tower::{Service, ServiceExt};
use tower_mcp::protocol::{ListToolsParams, McpRequest, McpResponse, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, NoParams, ToolBuilder, ToolDefinition};
use tower_mcp_types::JsonRpcError;

/// Shared discovery index, wrapped for concurrent access from tool handlers.
pub type SharedDiscoveryIndex = Arc<RwLock<DiscoveryRegistry>>;

/// Build a discovery index from the proxy's current tool list.
///
/// Sends a `ListTools` request through the proxy to collect all registered
/// tools, then indexes them using jpx-engine's BM25 search.
#[deprecated(
    since = "0.8.0",
    note = "use `Proxy::discovery_catalog`; this indexes the raw backend catalog"
)]
pub async fn build_index(proxy: &mut McpProxy, separator: &str) -> SharedDiscoveryIndex {
    #[allow(deprecated)]
    build_index_with_schemas(proxy, separator).await.0
}

/// Build a discovery index and retain tool definitions for `proxy/get_tool`.
#[deprecated(
    since = "0.8.0",
    note = "use `Proxy::discovery_catalog`; this indexes the raw backend catalog"
)]
pub async fn build_index_with_schemas(
    proxy: &mut McpProxy,
    separator: &str,
) -> (SharedDiscoveryIndex, SchemaStore) {
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

    let schemas = SchemaStore::new();
    for tool in &tools {
        schemas
            .insert(discovery_id(tool, separator), tool.clone())
            .await;
    }

    tracing::info!(tools_indexed = tools.len(), "Built tool discovery index");

    (Arc::new(RwLock::new(registry)), schemas)
}

/// Re-index all tools into an existing shared discovery index.
///
/// Called after hot reload adds, removes, or replaces backends to keep
/// the search index in sync with the proxy's current tool set.
#[deprecated(since = "0.8.0", note = "use `DiscoveryCatalog::refresh`")]
pub async fn reindex(index: &SharedDiscoveryIndex, proxy: &mut McpProxy, separator: &str) {
    #[allow(deprecated)]
    reindex_with_schemas(index, &SchemaStore::new(), proxy, separator).await;
}

/// Refresh both discovery search results and retained tool definitions.
#[deprecated(since = "0.8.0", note = "use `DiscoveryCatalog::refresh`")]
pub async fn reindex_with_schemas(
    index: &SharedDiscoveryIndex,
    schemas: &SchemaStore,
    proxy: &mut McpProxy,
    separator: &str,
) {
    let req = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions: Extensions::new(),
    };

    let tools = match proxy.call(req).await {
        Ok(resp) => match resp.inner {
            Ok(McpResponse::ListTools(result)) => result.tools,
            _ => vec![],
        },
        Err(_) => vec![],
    };

    let mut registry = DiscoveryRegistry::new();
    index_tools(&mut registry, &tools, separator);
    {
        let mut store = schemas.0.write().await;
        store.clear();
        for tool in &tools {
            store.insert(discovery_id(tool, separator), tool.clone());
        }
    }

    let mut guard = index.write().await;
    *guard = registry;

    tracing::info!(tools_indexed = tools.len(), "Re-indexed tool discovery");
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
struct GetToolInput {
    /// Tool ID from a search_tools/similar_tools result (e.g. "signoz:signoz_query_metrics")
    tool_id: String,
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

/// Tool definitions keyed by the same `server:tool` ID used by search results.
#[derive(Clone, Default)]
pub struct SchemaStore(Arc<RwLock<std::collections::HashMap<String, ToolDefinition>>>);

impl SchemaStore {
    /// Create an empty tool definition store.
    pub fn new() -> Self {
        Self::default()
    }
    /// Insert or replace the definition associated with a discovery ID.
    pub async fn insert(&self, id: String, tool: ToolDefinition) {
        self.0.write().await.insert(id, tool);
    }
    /// Look up a retained tool definition by its discovery ID.
    pub async fn get(&self, id: &str) -> Option<ToolDefinition> {
        self.0.read().await.get(id).cloned()
    }
}

fn discovery_id(tool: &ToolDefinition, separator: &str) -> String {
    discovery_id_for_name(&tool.name, separator)
}

/// The `namespace:tool` id search results use for a public tool name.
fn discovery_id_for_name(name: &str, separator: &str) -> String {
    let (namespace, local) = name.split_once(separator).unwrap_or(("default", name));
    format!("{namespace}:{local}")
}

const SEARCH_DESCRIPTION: &str = "Search for tools across all backends using BM25 full-text search. \
     Searches tool names, descriptions, parameters, and tags.";
const GET_TOOL_DESCRIPTION: &str = "Get the full input JSON Schema and description for a tool by its id \
     (e.g. \"signoz:signoz_query_metrics\"). Call this after search_tools \
     before invoking an unfamiliar tool through proxy/call_tool.";
const SIMILAR_DESCRIPTION: &str = "Find tools similar to a given tool. Uses BM25 similarity based on \
     shared terms in descriptions, parameters, and tags.";
const CATEGORIES_DESCRIPTION: &str = "List all tool categories (backend namespaces) with tool counts. \
     Useful for browsing available capabilities by domain.";

fn get_tool_output(id: &str, tool: &ToolDefinition) -> CallToolResult {
    json_text(&serde_json::json!({
        "id": id,
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    }))
}

fn unknown_tool_id(id: &str) -> CallToolResult {
    CallToolResult::error(format!(
        "Unknown tool id '{id}': run proxy/search_tools first and use its \"id\" field"
    ))
}

/// Build the three search and category tools without schema retrieval.
#[deprecated(
    since = "0.8.0",
    note = "use `Proxy::discovery_catalog`; these tools do not filter by caller"
)]
pub fn build_discovery_tools(index: SharedDiscoveryIndex) -> Vec<tower_mcp::Tool> {
    #[allow(deprecated)]
    let mut tools = build_discovery_tools_with_schemas(index, SchemaStore::new());
    tools.retain(|tool| tool.name != "get_tool");
    tools
}

/// Build all discovery tools, including retrieval of full tool definitions.
#[deprecated(
    since = "0.8.0",
    note = "use `Proxy::discovery_catalog`; these tools do not filter by caller"
)]
pub fn build_discovery_tools_with_schemas(
    index: SharedDiscoveryIndex,
    schemas: SchemaStore,
) -> Vec<tower_mcp::Tool> {
    let index_for_search = Arc::clone(&index);
    let search_tools = ToolBuilder::new("search_tools")
        .description(SEARCH_DESCRIPTION)
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

    let schemas_for_get = schemas.clone();
    let get_tool = ToolBuilder::new("get_tool")
        .description(GET_TOOL_DESCRIPTION)
        .handler(move |input: GetToolInput| {
            let store = schemas_for_get.clone();
            async move {
                Ok(match store.get(&input.tool_id).await {
                    Some(tool) => get_tool_output(&input.tool_id, &tool),
                    None => unknown_tool_id(&input.tool_id),
                })
            }
        })
        .build();

    let index_for_similar = Arc::clone(&index);
    let similar_tools = ToolBuilder::new("similar_tools")
        .description(SIMILAR_DESCRIPTION)
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
        .description(CATEGORIES_DESCRIPTION)
        .handler(move |_: NoParams| {
            let idx = Arc::clone(&index_for_categories);
            async move {
                let registry = idx.read().await;
                let categories = registry.list_categories();
                let mut cats: Vec<CategorySummary> = categories.into_values().collect();
                cats.sort_by_key(|b| std::cmp::Reverse(b.tool_count));
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

    vec![search_tools, get_tool, similar_tools, tool_categories]
}

// ---------------------------------------------------------------------------
// Public, caller-filtered catalog
// ---------------------------------------------------------------------------

/// Service the catalog lists tools through: the public middleware stack below
/// caller policy.
pub(crate) type CatalogSource = BoxCloneService<RouterRequest, RouterResponse, Infallible>;

/// Discovery tool names, without the `proxy` namespace prefix.
const TOOL_NAMES: [&str; 4] = [
    "search_tools",
    "get_tool",
    "similar_tools",
    "tool_categories",
];

/// Filtered search ranks this many candidates per requested result, so callers
/// who see only part of the catalog still get up to `top_k` results.
const OVERFETCH_FACTOR: usize = 4;
/// Upper bound on ranked candidates for one filtered query.
const OVERFETCH_CAP: usize = 200;

/// One complete, immutable view of the indexed catalog.
struct Snapshot {
    registry: DiscoveryRegistry,
    tools: std::collections::HashMap<String, ToolDefinition>,
    revision: u64,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            registry: DiscoveryRegistry::new(),
            tools: std::collections::HashMap::new(),
            revision: 0,
        }
    }
}

struct CatalogInner {
    separator: String,
    admin_prefix: String,
    snapshot: std::sync::RwLock<Arc<Snapshot>>,
    source: std::sync::Mutex<Option<CatalogSource>>,
    refresh_lock: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    last_ok: AtomicBool,
}

/// The discovery index built from the proxy's public tool catalog.
///
/// [`crate::Proxy`] builds the catalog by listing tools through its public
/// middleware stack, so search results and `proxy/get_tool` schemas use the
/// names and parameters clients see: aliases, hidden tools, hidden and renamed
/// parameters, locked argument keys and composite tools are all applied. Each
/// discovery call is answered for its caller: results are limited to the tools
/// that caller's own `tools/list` would show, after RBAC and bearer-token scopes.
///
/// The catalog refreshes itself whenever the proxy's tool list changes: file
/// reload, HTTP or MCP backend management, and backend `list_changed`
/// notifications. A refresh replaces the whole snapshot in one step. When the
/// listing fails, the previous snapshot stays in place.
///
/// # Example
///
/// ```no_run
/// # async fn demo(proxy: &mcp_proxy::Proxy) {
/// if let Some(catalog) = proxy.discovery_catalog() {
///     // Changes made through `Proxy::mcp_proxy()` are picked up automatically;
///     // `refresh` rebuilds now and reports whether the listing succeeded.
///     let ok = catalog.refresh().await;
///     println!("ok={ok} revision={} tools={}", catalog.revision(), catalog.tool_count());
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct DiscoveryCatalog {
    inner: Arc<CatalogInner>,
}

impl std::fmt::Debug for DiscoveryCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryCatalog")
            .field("revision", &self.revision())
            .field("tools", &self.tool_count())
            .finish()
    }
}

impl DiscoveryCatalog {
    /// Create an empty catalog. It indexes nothing until a source is set.
    pub(crate) fn new(separator: &str) -> Self {
        Self {
            inner: Arc::new(CatalogInner {
                separator: separator.to_string(),
                admin_prefix: format!("proxy{separator}"),
                snapshot: std::sync::RwLock::new(Arc::new(Snapshot::empty())),
                source: std::sync::Mutex::new(None),
                refresh_lock: tokio::sync::Mutex::new(()),
                dirty: AtomicBool::new(false),
                last_ok: AtomicBool::new(false),
            }),
        }
    }

    /// Set the service tools are listed through.
    pub(crate) fn set_source(&self, source: CatalogSource) {
        *self.inner.source.lock().expect("catalog source") = Some(source);
    }

    /// Number of successful refreshes so far. Starts at 0.
    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }

    /// Number of tools in the current snapshot.
    pub fn tool_count(&self) -> usize {
        self.snapshot().tools.len()
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.inner.snapshot.read().expect("catalog snapshot"))
    }

    /// Rebuild the catalog from the current public tool list.
    ///
    /// Returns `true` when the catalog now reflects a successful listing that
    /// started after this call. Concurrent calls are coalesced: a call made
    /// while a refresh is running waits for it and then runs at most one more.
    /// On failure the previous snapshot is kept and `false` is returned.
    pub async fn refresh(&self) -> bool {
        self.inner.dirty.store(true, Ordering::SeqCst);
        let _guard = self.inner.refresh_lock.lock().await;
        if !self.inner.dirty.swap(false, Ordering::SeqCst) {
            // A refresh that started after this request already ran.
            return self.inner.last_ok.load(Ordering::SeqCst);
        }
        let source = self.inner.source.lock().expect("catalog source").clone();
        let Some(source) = source else {
            return false;
        };
        let ok = match list_public(source, Extensions::new()).await {
            Some(tools) => {
                let revision = self.revision() + 1;
                let snapshot = self.build_snapshot(tools, revision);
                let count = snapshot.tools.len();
                *self.inner.snapshot.write().expect("catalog snapshot") = Arc::new(snapshot);
                tracing::info!(
                    tools_indexed = count,
                    revision,
                    "Refreshed tool discovery catalog"
                );
                true
            }
            None => {
                tracing::warn!("Tool listing failed; keeping the previous discovery catalog");
                false
            }
        };
        self.inner.last_ok.store(ok, Ordering::SeqCst);
        #[cfg(feature = "metrics")]
        metrics::counter!(
            "mcp_proxy_discovery_refresh_total",
            "outcome" => if ok { "success" } else { "error" },
        )
        .increment(1);
        ok
    }

    fn build_snapshot(&self, tools: Vec<ToolDefinition>, revision: u64) -> Snapshot {
        let separator = &self.inner.separator;
        let tools: Vec<ToolDefinition> = tools
            .into_iter()
            .filter(|t| !t.name.starts_with(&self.inner.admin_prefix))
            .collect();
        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &tools, separator);
        let tools = tools
            .into_iter()
            .map(|t| (discovery_id(&t, separator), t))
            .collect();
        Snapshot {
            registry,
            tools,
            revision,
        }
    }

    /// Refresh whenever the proxy reports that its tool list changed.
    ///
    /// The task holds only a weak reference, and ends when the proxy that
    /// owns the sender is dropped or the catalog is gone.
    pub(crate) fn listen(&self, mut rx: tower_mcp::context::NotificationReceiver) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            while let Some(note) = rx.recv().await {
                let mut changed = is_tools_changed(&note);
                while let Ok(note) = rx.try_recv() {
                    changed |= is_tools_changed(&note);
                }
                if !changed {
                    continue;
                }
                let Some(inner) = weak.upgrade() else { break };
                DiscoveryCatalog { inner }.refresh().await;
            }
        });
    }

    /// Ids of the catalog tools among `visible` public tool names.
    fn visible_ids(
        &self,
        visible: &std::collections::HashSet<String>,
    ) -> std::collections::HashSet<String> {
        visible
            .iter()
            .filter(|name| !name.starts_with(&self.inner.admin_prefix))
            .map(|name| discovery_id_for_name(name, &self.inner.separator))
            .collect()
    }

    fn search(
        &self,
        input: SearchInput,
        visible: Option<&std::collections::HashSet<String>>,
    ) -> CallToolResult {
        let snapshot = self.snapshot();
        let entries: Vec<SearchResultEntry> = match visible {
            None => snapshot
                .registry
                .query(&input.query, input.top_k)
                .into_iter()
                .map(Into::into)
                .collect(),
            Some(ids) => {
                let fetch = input
                    .top_k
                    .saturating_mul(OVERFETCH_FACTOR)
                    .min(OVERFETCH_CAP)
                    .max(input.top_k);
                snapshot
                    .registry
                    .query(&input.query, fetch)
                    .into_iter()
                    .filter(|r| ids.contains(&r.id))
                    .take(input.top_k)
                    .map(Into::into)
                    .collect()
            }
        };
        json_text(&entries)
    }

    fn similar(
        &self,
        input: SimilarInput,
        visible: Option<&std::collections::HashSet<String>>,
    ) -> CallToolResult {
        let snapshot = self.snapshot();
        let entries: Vec<SearchResultEntry> = match visible {
            None => snapshot
                .registry
                .similar(&input.tool_id, input.top_k)
                .into_iter()
                .map(Into::into)
                .collect(),
            Some(ids) if !ids.contains(&input.tool_id) => Vec::new(),
            Some(ids) => {
                let fetch = input
                    .top_k
                    .saturating_mul(OVERFETCH_FACTOR)
                    .min(OVERFETCH_CAP)
                    .max(input.top_k);
                snapshot
                    .registry
                    .similar(&input.tool_id, fetch)
                    .into_iter()
                    .filter(|r| ids.contains(&r.id))
                    .take(input.top_k)
                    .map(Into::into)
                    .collect()
            }
        };
        json_text(&entries)
    }

    fn get_tool(
        &self,
        input: GetToolInput,
        visible: Option<&std::collections::HashSet<String>>,
    ) -> CallToolResult {
        let snapshot = self.snapshot();
        let allowed = visible.is_none_or(|ids| ids.contains(&input.tool_id));
        match snapshot.tools.get(&input.tool_id).filter(|_| allowed) {
            Some(tool) => get_tool_output(&input.tool_id, tool),
            None => unknown_tool_id(&input.tool_id),
        }
    }

    fn categories(&self, visible: Option<&std::collections::HashSet<String>>) -> CallToolResult {
        let snapshot = self.snapshot();
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for id in snapshot.tools.keys() {
            if visible.is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let namespace = id.split_once(':').map_or("default", |(ns, _)| ns);
            *counts.entry(namespace.to_string()).or_default() += 1;
        }
        let mut categories: Vec<CategorySummary> = counts
            .into_iter()
            .map(|(name, tool_count)| CategorySummary {
                servers: vec![name.clone()],
                name,
                tool_count,
                subcategories: Vec::new(),
            })
            .collect();
        categories.sort_by_key(|c| std::cmp::Reverse(c.tool_count));
        json_text(&CategoriesResult {
            total_categories: categories.len(),
            categories,
        })
    }

    /// Answer a discovery call for a caller who can see `visible` tools.
    fn answer(
        &self,
        tool: &str,
        arguments: serde_json::Value,
        visible: &std::collections::HashSet<String>,
    ) -> Result<CallToolResult, JsonRpcError> {
        let ids = self.visible_ids(visible);
        let arguments = if arguments.is_null() {
            serde_json::json!({})
        } else {
            arguments
        };
        let invalid = |e: serde_json::Error| {
            JsonRpcError::invalid_params(format!("invalid arguments for {tool}: {e}"))
        };
        Ok(match tool {
            "search_tools" => self.search(
                serde_json::from_value(arguments).map_err(invalid)?,
                Some(&ids),
            ),
            "similar_tools" => self.similar(
                serde_json::from_value(arguments).map_err(invalid)?,
                Some(&ids),
            ),
            "get_tool" => self.get_tool(
                serde_json::from_value(arguments).map_err(invalid)?,
                Some(&ids),
            ),
            _ => self.categories(Some(&ids)),
        })
    }

    /// Discovery tool definitions for the `proxy` admin backend.
    ///
    /// Calls that pass through the proxy's middleware are answered by
    /// [`DiscoveryService`] for the caller. These handlers serve only callers
    /// that bypass it through the trusted [`crate::Proxy::mcp_proxy`] handle,
    /// and see the whole catalog. They hold a weak reference so the catalog,
    /// which owns a handle to the proxy, does not keep itself alive.
    pub(crate) fn tools(&self) -> Vec<tower_mcp::Tool> {
        let weak = Arc::downgrade(&self.inner);
        let gone = || CallToolResult::error("discovery catalog is no longer available");
        let w = weak.clone();
        let search_tools = ToolBuilder::new("search_tools")
            .description(SEARCH_DESCRIPTION)
            .handler(move |input: SearchInput| {
                let w = w.clone();
                async move {
                    Ok(w.upgrade()
                        .map_or_else(gone, |inner| DiscoveryCatalog { inner }.search(input, None)))
                }
            })
            .build();
        let w = weak.clone();
        let get_tool = ToolBuilder::new("get_tool")
            .description(GET_TOOL_DESCRIPTION)
            .handler(move |input: GetToolInput| {
                let w = w.clone();
                async move {
                    Ok(w.upgrade().map_or_else(gone, |inner| {
                        DiscoveryCatalog { inner }.get_tool(input, None)
                    }))
                }
            })
            .build();
        let w = weak.clone();
        let similar_tools = ToolBuilder::new("similar_tools")
            .description(SIMILAR_DESCRIPTION)
            .handler(move |input: SimilarInput| {
                let w = w.clone();
                async move {
                    Ok(w.upgrade().map_or_else(gone, |inner| {
                        DiscoveryCatalog { inner }.similar(input, None)
                    }))
                }
            })
            .build();
        let w = weak;
        let tool_categories = ToolBuilder::new("tool_categories")
            .description(CATEGORIES_DESCRIPTION)
            .handler(move |_: NoParams| {
                let w = w.clone();
                async move {
                    Ok(w.upgrade()
                        .map_or_else(gone, |inner| DiscoveryCatalog { inner }.categories(None)))
                }
            })
            .build();
        vec![search_tools, get_tool, similar_tools, tool_categories]
    }
}

fn is_tools_changed(note: &tower_mcp::context::ServerNotification) -> bool {
    matches!(
        note,
        tower_mcp::context::ServerNotification::ToolsListChanged
    )
}

fn json_text<T: Serialize>(value: &T) -> CallToolResult {
    CallToolResult::text(serde_json::to_string_pretty(value).unwrap_or_default())
}

/// List tools through `service` with the given request extensions.
///
/// The [`crate::filter::UnfilteredListing`] marker lets the listing see backend
/// tools in search mode; every other filter still applies. Returns `None` when
/// the listing fails.
async fn list_public<S>(mut service: S, mut extensions: Extensions) -> Option<Vec<ToolDefinition>>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
{
    extensions.insert(crate::filter::UnfilteredListing);
    let request = RouterRequest {
        id: RequestId::Number(0),
        inner: McpRequest::ListTools(ListToolsParams::default()),
        extensions,
    };
    let response = match service.ready().await {
        Ok(ready) => ready.call(request).await,
        Err(never) => match never {},
    };
    match response {
        Ok(RouterResponse {
            inner: Ok(McpResponse::ListTools(result)),
            ..
        }) => Some(result.tools),
        _ => None,
    }
}

/// Middleware that answers the `proxy` discovery tools for each caller.
///
/// It sits directly outside caller policy (RBAC, bearer scopes, OAuth scopes).
/// For a discovery call it lists tools through the policy stack with the
/// caller's request extensions, then answers from the [`DiscoveryCatalog`]
/// using only the tools that listing returned. When the caller cannot see the
/// discovery tool itself, or the listing fails, the request is passed through
/// unchanged so policy produces its usual error.
#[derive(Clone)]
pub(crate) struct DiscoveryService<S> {
    inner: S,
    catalog: DiscoveryCatalog,
    prefix: String,
}

impl<S> DiscoveryService<S> {
    pub(crate) fn new(inner: S, catalog: DiscoveryCatalog, separator: &str) -> Self {
        Self {
            inner,
            catalog,
            prefix: format!("proxy{separator}"),
        }
    }
}

impl<S> Service<RouterRequest> for DiscoveryService<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<RouterResponse, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let tool = match &req.inner {
            McpRequest::CallTool(params) => params
                .name
                .strip_prefix(&self.prefix)
                .filter(|name| TOOL_NAMES.contains(name))
                .map(str::to_owned),
            _ => None,
        };
        let Some(tool) = tool else {
            return Box::pin(self.inner.call(req));
        };
        let probe = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, probe.clone());
        let catalog = self.catalog.clone();
        let full_name = format!("{}{tool}", self.prefix);
        Box::pin(async move {
            let Some(listed) = list_public(probe, req.extensions.clone()).await else {
                return inner.call(req).await;
            };
            let visible: std::collections::HashSet<String> =
                listed.into_iter().map(|t| t.name).collect();
            if !visible.contains(&full_name) {
                return inner.call(req).await;
            }
            let McpRequest::CallTool(params) = req.inner else {
                unreachable!("only tool calls are intercepted")
            };
            let inner = catalog
                .answer(&tool, params.arguments, &visible)
                .map(McpResponse::CallTool);
            Ok(RouterResponse { id: req.id, inner })
        })
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use jpx_engine::DiscoveryRegistry;
    use tower_mcp::ToolDefinition;
    use tower_mcp_types::protocol::ToolAnnotations;

    use super::*;

    fn make_tool(
        name: &str,
        description: Option<&str>,
        annotations: Option<ToolAnnotations>,
    ) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            title: None,
            description: description.map(|d| d.to_string()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icons: None,
            annotations,
            execution: None,
            meta: None,
        }
    }

    // -- index_tools tests ---------------------------------------------------

    #[test]
    fn index_tools_empty_list() {
        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &[], "/");
        let cats = registry.list_categories();
        assert!(cats.is_empty());
    }

    #[test]
    fn index_tools_groups_by_namespace() {
        let tools = vec![
            make_tool("fs/read", Some("Read a file"), None),
            make_tool("fs/write", Some("Write a file"), None),
            make_tool("db/query", Some("Run a query"), None),
        ];

        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &tools, "/");

        let cats = registry.list_categories();
        assert_eq!(cats.len(), 2);
        assert!(cats.contains_key("fs"));
        assert!(cats.contains_key("db"));
    }

    #[test]
    fn index_tools_no_separator_uses_default_namespace() {
        let tools = vec![make_tool("standalone", Some("No namespace"), None)];

        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &tools, "/");

        // The tool is registered under the "default" server; search finds it
        let results = registry.query("namespace", 10);
        assert!(!results.is_empty());
    }

    #[test]
    fn index_tools_without_descriptions() {
        let tools = vec![make_tool("ns/tool", None, None)];

        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &tools, "/");

        let cats = registry.list_categories();
        assert_eq!(cats.len(), 1);
    }

    #[test]
    fn index_tools_with_annotations() {
        let tools = vec![make_tool(
            "ns/dangerous",
            Some("Dangerous tool"),
            Some(ToolAnnotations {
                title: None,
                destructive_hint: true,
                read_only_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
            }),
        )];

        let mut registry = DiscoveryRegistry::new();
        index_tools(&mut registry, &tools, "/");

        // The tool was indexed; search for its annotation tag
        let results = registry.query("destructive", 10);
        assert!(!results.is_empty());
    }

    // -- tool_definition_to_spec tests ----------------------------------------

    #[test]
    fn tool_definition_to_spec_extracts_local_name() {
        let tool = make_tool("backend/read_file", Some("Reads files"), None);
        let spec = tool_definition_to_spec(&tool, "/");
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.category.as_deref(), Some("backend"));
    }

    #[test]
    fn tool_definition_to_spec_no_separator() {
        let tool = make_tool("read_file", Some("Reads files"), None);
        let spec = tool_definition_to_spec(&tool, "/");
        assert_eq!(spec.name, "read_file");
        assert!(spec.category.is_none());
    }

    #[test]
    fn tool_definition_to_spec_annotation_tags() {
        let tool = make_tool(
            "ns/tool",
            Some("desc"),
            Some(ToolAnnotations {
                title: None,
                destructive_hint: true,
                read_only_hint: true,
                idempotent_hint: true,
                open_world_hint: true,
            }),
        );
        let spec = tool_definition_to_spec(&tool, "/");
        assert_eq!(spec.tags.len(), 4);
        assert!(spec.tags.contains(&"destructive".to_string()));
        assert!(spec.tags.contains(&"read-only".to_string()));
        assert!(spec.tags.contains(&"idempotent".to_string()));
        assert!(spec.tags.contains(&"open-world".to_string()));
    }

    #[test]
    fn tool_definition_to_spec_no_annotations_no_tags() {
        let tool = make_tool("ns/tool", Some("desc"), None);
        let spec = tool_definition_to_spec(&tool, "/");
        assert!(spec.tags.is_empty());
    }

    #[test]
    fn tool_definition_to_spec_preserves_description() {
        let tool = make_tool("ns/tool", Some("My description"), None);
        let spec = tool_definition_to_spec(&tool, "/");
        assert_eq!(spec.summary.as_deref(), Some("My description"));
        assert_eq!(spec.description.as_deref(), Some("My description"));
    }

    // -- extract_params tests ------------------------------------------------

    #[test]
    fn extract_params_empty_schema() {
        let schema = serde_json::json!({"type": "object"});
        let params = extract_params(&schema);
        assert!(params.is_empty());
    }

    #[test]
    fn extract_params_with_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path"
                },
                "recursive": {
                    "type": "boolean"
                }
            },
            "required": ["path"]
        });
        let params = extract_params(&schema);
        assert_eq!(params.len(), 2);

        let path_param = params.iter().find(|p| p.name == "path").unwrap();
        assert!(path_param.required);
        assert_eq!(path_param.param_type.as_deref(), Some("string"));
        assert_eq!(path_param.description.as_deref(), Some("File path"));

        let recursive_param = params.iter().find(|p| p.name == "recursive").unwrap();
        assert!(!recursive_param.required);
        assert_eq!(recursive_param.param_type.as_deref(), Some("boolean"));
    }

    #[test]
    fn extract_params_no_required_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let params = extract_params(&schema);
        assert_eq!(params.len(), 1);
        assert!(!params[0].required);
    }

    // -- build_discovery_tools tests -----------------------------------------

    #[test]
    fn build_discovery_tools_returns_three_tools() {
        let index = Arc::new(RwLock::new(DiscoveryRegistry::new()));
        let tools = build_discovery_tools(index);
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "search_tools");
        assert_eq!(tools[1].name, "similar_tools");
        assert_eq!(tools[2].name, "tool_categories");
    }

    // -- DiscoveryCatalog -----------------------------------------------------

    use std::sync::atomic::AtomicUsize;

    /// A catalog source answering `ListTools` from `tools`, or failing while
    /// `fail` is set. Counts every listing in `calls`.
    fn source(
        tools: Arc<std::sync::Mutex<Vec<ToolDefinition>>>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    ) -> CatalogSource {
        BoxCloneService::new(tower::service_fn(move |req: RouterRequest| {
            let tools = tools.lock().unwrap().clone();
            let fail = fail.load(Ordering::SeqCst);
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                assert!(
                    req.extensions
                        .get::<crate::filter::UnfilteredListing>()
                        .is_some()
                );
                let inner = if fail {
                    Err(JsonRpcError::internal_error("listing failed"))
                } else {
                    Ok(McpResponse::ListTools(
                        tower_mcp::protocol::ListToolsResult {
                            tools,
                            next_cursor: None,
                            ttl_ms: None,
                            cache_scope: None,
                            meta: None,
                        },
                    ))
                };
                Ok::<_, Infallible>(RouterResponse { id: req.id, inner })
            }
        }))
    }

    struct CatalogFixture {
        catalog: DiscoveryCatalog,
        tools: Arc<std::sync::Mutex<Vec<ToolDefinition>>>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    fn catalog_fixture(names: &[&str]) -> CatalogFixture {
        let tools = Arc::new(std::sync::Mutex::new(
            names
                .iter()
                .map(|n| make_tool(n, Some("does things"), None))
                .collect(),
        ));
        let fail = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let catalog = DiscoveryCatalog::new("/");
        catalog.set_source(source(tools.clone(), fail.clone(), calls.clone()));
        CatalogFixture {
            catalog,
            tools,
            fail,
            calls,
        }
    }

    #[tokio::test]
    async fn refresh_indexes_public_tools_and_skips_the_admin_namespace() {
        let fx = catalog_fixture(&["api/read", "api/write", "proxy/search_tools"]);
        assert_eq!(fx.catalog.revision(), 0);
        assert!(fx.catalog.refresh().await);
        assert_eq!(fx.catalog.revision(), 1);
        assert_eq!(fx.catalog.tool_count(), 2);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_previous_snapshot() {
        let fx = catalog_fixture(&["api/read"]);
        assert!(fx.catalog.refresh().await);
        fx.fail.store(true, Ordering::SeqCst);
        fx.tools.lock().unwrap().clear();
        assert!(!fx.catalog.refresh().await);
        assert_eq!(fx.catalog.revision(), 1);
        assert_eq!(fx.catalog.tool_count(), 1);
        fx.fail.store(false, Ordering::SeqCst);
        assert!(fx.catalog.refresh().await);
        assert_eq!(fx.catalog.tool_count(), 0);
    }

    #[tokio::test]
    async fn refresh_without_a_source_reports_failure() {
        assert!(!DiscoveryCatalog::new("/").refresh().await);
    }

    #[tokio::test]
    async fn concurrent_refreshes_are_coalesced() {
        let fx = catalog_fixture(&["api/read"]);
        // Hold the refresh lock so every request queues behind one refresh.
        let guard = fx.catalog.inner.refresh_lock.lock().await;
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let catalog = fx.catalog.clone();
                tokio::spawn(async move { catalog.refresh().await })
            })
            .collect();
        tokio::task::yield_now().await;
        drop(guard);
        for waiter in waiters {
            assert!(waiter.await.unwrap());
        }
        assert_eq!(fx.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn filtered_search_and_get_tool_honor_visibility() {
        let catalog = DiscoveryCatalog::new("/");
        let snapshot = catalog.build_snapshot(
            vec![
                make_tool("api/read_file", Some("Read a file"), None),
                make_tool("db/read_rows", Some("Read rows"), None),
            ],
            1,
        );
        *catalog.inner.snapshot.write().unwrap() = Arc::new(snapshot);
        let visible: std::collections::HashSet<String> = [
            "api/read_file".to_string(),
            "proxy/search_tools".to_string(),
        ]
        .into();

        let text = |r: CallToolResult| {
            serde_json::to_value(r).unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let found = text(
            catalog
                .answer(
                    "search_tools",
                    serde_json::json!({"query": "read"}),
                    &visible,
                )
                .unwrap(),
        );
        assert!(found.contains("api:read_file"), "{found}");
        assert!(!found.contains("db:read_rows"), "{found}");

        let hidden = catalog
            .answer(
                "get_tool",
                serde_json::json!({"tool_id": "db:read_rows"}),
                &visible,
            )
            .unwrap();
        assert!(hidden.is_error);
        let unknown = catalog
            .answer(
                "get_tool",
                serde_json::json!({"tool_id": "db:missing"}),
                &visible,
            )
            .unwrap();
        assert_eq!(
            text(hidden),
            text(unknown).replace("db:missing", "db:read_rows")
        );

        let categories = text(
            catalog
                .answer("tool_categories", serde_json::Value::Null, &visible)
                .unwrap(),
        );
        assert!(categories.contains("\"api\""), "{categories}");
        assert!(!categories.contains("\"db\""), "{categories}");

        assert!(
            catalog
                .answer("search_tools", serde_json::json!({"top_k": 1}), &visible)
                .is_err()
        );
    }

    #[tokio::test]
    async fn discovery_service_passes_other_requests_and_hidden_tools_through() {
        use crate::test_util::{MockService, call_service};
        use tower_mcp::protocol::CallToolParams;

        let catalog = DiscoveryCatalog::new("/");
        // The mock lists only an ordinary tool, so the caller cannot see the
        // discovery tool and the call reaches the inner service unchanged.
        let mock = MockService::with_tools(&["api/read"]);
        let mut svc = DiscoveryService::new(mock, catalog, "/");
        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "proxy/search_tools".into(),
                arguments: serde_json::json!({"query": "read"}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
        )
        .await;
        let McpResponse::CallTool(result) = resp.inner.unwrap() else {
            panic!("expected a tool result");
        };
        assert_eq!(
            serde_json::to_value(result).unwrap()["content"][0]["text"],
            "called: proxy/search_tools"
        );
        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(matches!(resp.inner, Ok(McpResponse::ListTools(_))));
    }
}
