//! Full tool-schema retrieval and refresh through the discovery meta-tools.
#![cfg(feature = "discovery")]

use mcp_proxy::discovery::{
    build_discovery_tools_with_schemas, build_index_with_schemas, reindex_with_schemas,
};
use tower::{Service, ServiceExt};
use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, McpRequest, McpResponse, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

fn backend(name: &str, schema: serde_json::Value) -> McpRouter {
    McpRouter::new().tool(
        ToolBuilder::new(name)
            .description("Retrieve nested query results")
            .input_schema(schema)
            .handler(|_: serde_json::Value| async { Ok(CallToolResult::text("ok")) })
            .build(),
    )
}

async fn get_tool(router: &mut McpProxy, id: &str) -> CallToolResult {
    let response = router
        .ready()
        .await
        .unwrap()
        .call(RouterRequest {
            id: RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "discovery/get_tool".into(),
                arguments: serde_json::json!({"tool_id": id}),
                input_responses: None,
                request_state: None,
                meta: None,
                task: None,
            }),
            extensions: Extensions::new(),
        })
        .await
        .unwrap();
    match response.inner.unwrap() {
        McpResponse::CallTool(result) => result,
        _ => panic!("expected tool result"),
    }
}

#[tokio::test]
async fn get_tool_retains_nested_schema_and_uses_search_result_ids() {
    let schema = serde_json::json!({"type": "object", "properties": {
        "query": {"type": "object", "properties": {"mode": {"enum": ["sum", "count"]}}, "required": ["mode"]}
    }, "required": ["query"]});
    let mut proxy = McpProxy::builder("test", "1")
        .separator("/")
        .backend(
            "api",
            ChannelTransport::new(backend("query/nested", schema.clone())),
        )
        .await
        .build_strict()
        .await
        .unwrap();
    let (index, schemas) = build_index_with_schemas(&mut proxy, "/").await;
    let results = index.read().await.query("nested", 10);
    let id = &results[0].id;
    assert_eq!(id, "api:query/nested");
    let mut router = McpRouter::new();
    for tool in build_discovery_tools_with_schemas(index, schemas) {
        router = router.tool(tool);
    }
    proxy
        .add_backend("discovery", ChannelTransport::new(router))
        .await
        .unwrap();
    let result = get_tool(&mut proxy, id).await;
    let content = serde_json::to_value(result).unwrap();
    let data: serde_json::Value =
        serde_json::from_str(content["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["input_schema"], schema);
    assert_eq!(data["name"], "api/query/nested");
    assert_eq!(data["description"], "Retrieve nested query results");
    let missing = serde_json::to_value(get_tool(&mut proxy, "api:missing").await).unwrap();
    assert_eq!(missing["isError"], true);
}

#[tokio::test]
async fn schema_refresh_replaces_and_removes_stale_entries() {
    let mut proxy = McpProxy::builder("test", "1")
        .separator(".")
        .backend(
            "api",
            ChannelTransport::new(backend(
                "query.nested",
                serde_json::json!({"type":"object"}),
            )),
        )
        .await
        .build_strict()
        .await
        .unwrap();
    let (index, schemas) = build_index_with_schemas(&mut proxy, ".").await;
    assert!(schemas.get("api:query.nested").await.is_some());
    assert!(proxy.remove_backend("api").await);
    let updated = serde_json::json!({"type":"object", "properties":{"new":{"type":"string"}}});
    proxy
        .add_backend(
            "api",
            ChannelTransport::new(backend("query.nested", updated.clone())),
        )
        .await
        .unwrap();
    reindex_with_schemas(&index, &schemas, &mut proxy, ".").await;
    assert_eq!(
        schemas.get("api:query.nested").await.unwrap().input_schema,
        updated
    );
    assert!(proxy.remove_backend("api").await);
    reindex_with_schemas(&index, &schemas, &mut proxy, ".").await;
    assert!(schemas.get("api:query.nested").await.is_none());
    assert!(index.read().await.query("nested", 10).is_empty());
}
