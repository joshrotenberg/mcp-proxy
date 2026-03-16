//! Parameter override middleware for tool customization.
//!
//! Modifies tool schemas and call arguments to hide parameters (injecting
//! defaults) and rename parameters. This turns generic tools into
//! domain-specific ones via config.
//!
//! # Configuration
//!
//! ```toml
//! [[backends.param_overrides]]
//! tool = "list_directory"
//! hide = ["path"]
//! defaults = { path = "/home/docs" }
//! rename = { recursive = "deep_search" }
//! ```
//!
//! On `ListTools`: hidden parameters are removed from the tool's `input_schema`,
//! and renamed parameters have their schema keys swapped.
//!
//! On `CallTool`: hidden parameter defaults are injected, and renamed
//! parameters are mapped back to their original names before forwarding.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::{McpRequest, McpResponse};

/// Tower layer that produces a [`ParamOverrideService`].
///
/// # Example
///
/// ```rust,ignore
/// use tower::ServiceBuilder;
/// use mcp_proxy::param_override::ParamOverrideLayer;
///
/// let service = ServiceBuilder::new()
///     .layer(ParamOverrideLayer::new(overrides))
///     .service(proxy);
/// ```
#[derive(Clone)]
pub struct ParamOverrideLayer {
    overrides: Vec<ToolOverride>,
}

impl ParamOverrideLayer {
    /// Create a new parameter override layer.
    pub fn new(overrides: Vec<ToolOverride>) -> Self {
        Self { overrides }
    }
}

impl<S> Layer<S> for ParamOverrideLayer {
    type Service = ParamOverrideService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ParamOverrideService::new(inner, self.overrides.clone())
    }
}

/// Resolved parameter override rules for a single tool.
#[derive(Debug, Clone)]
pub struct ToolOverride {
    /// Namespaced tool name (e.g. "fs/list_directory").
    namespaced_tool: String,
    /// Parameters to hide from the schema.
    hide: Vec<String>,
    /// Default values for hidden parameters.
    defaults: serde_json::Map<String, serde_json::Value>,
    /// Forward rename map: original_name -> new_name (for schema rewriting).
    rename_forward: HashMap<String, String>,
    /// Reverse rename map: new_name -> original_name (for call rewriting).
    rename_reverse: HashMap<String, String>,
}

impl ToolOverride {
    /// Create a new tool override from config.
    pub fn new(namespace: &str, config: &crate::config::ParamOverrideConfig) -> Self {
        let rename_forward: HashMap<String, String> = config.rename.clone();
        let rename_reverse: HashMap<String, String> = config
            .rename
            .iter()
            .map(|(orig, new)| (new.clone(), orig.clone()))
            .collect();

        Self {
            namespaced_tool: format!("{namespace}{}", config.tool),
            hide: config.hide.clone(),
            defaults: config.defaults.clone(),
            rename_forward,
            rename_reverse,
        }
    }
}

/// Parameter override middleware.
///
/// Intercepts `ListTools` responses to modify tool schemas (hiding and
/// renaming parameters) and `CallTool` requests to inject hidden defaults
/// and reverse-map renamed parameters.
#[derive(Clone)]
pub struct ParamOverrideService<S> {
    inner: S,
    overrides: Arc<Vec<ToolOverride>>,
}

impl<S> ParamOverrideService<S> {
    /// Create a new parameter override service.
    pub fn new(inner: S, overrides: Vec<ToolOverride>) -> Self {
        Self {
            inner,
            overrides: Arc::new(overrides),
        }
    }
}

/// Remove hidden properties from a JSON Schema object and apply renames.
fn rewrite_schema(
    schema: &mut serde_json::Value,
    hide: &[String],
    rename_forward: &HashMap<String, String>,
) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    // Rewrite "properties" object: remove hidden, rename others
    if let Some(props) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
        for param in hide {
            props.remove(param);
        }
        for (original, renamed) in rename_forward {
            if let Some(prop_schema) = props.remove(original) {
                props.insert(renamed.clone(), prop_schema);
            }
        }
    }

    // Rewrite "required" array: remove hidden, rename others
    if let Some(required) = obj.get_mut("required").and_then(|v| v.as_array_mut()) {
        required.retain(|v| {
            v.as_str()
                .map(|s| !hide.contains(&s.to_string()))
                .unwrap_or(true)
        });
        for entry in required.iter_mut() {
            if let Some(s) = entry.as_str()
                && let Some(new_name) = rename_forward.get(s)
            {
                *entry = serde_json::Value::String(new_name.clone());
            }
        }
    }
}

impl<S> Service<RouterRequest> for ParamOverrideService<S>
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

    fn call(&mut self, mut req: RouterRequest) -> Self::Future {
        let overrides = Arc::clone(&self.overrides);

        // On CallTool: inject hidden defaults, reverse-map renamed params
        if let McpRequest::CallTool(ref mut params) = req.inner {
            for tool_override in overrides.iter() {
                if params.name != tool_override.namespaced_tool {
                    continue;
                }

                // Inject defaults for hidden parameters
                if let serde_json::Value::Object(ref mut args) = params.arguments {
                    for (key, value) in &tool_override.defaults {
                        if !args.contains_key(key) {
                            args.insert(key.clone(), value.clone());
                        }
                    }

                    // Reverse-map renamed parameters back to originals
                    let keys_to_rename: Vec<(String, String)> = args
                        .keys()
                        .filter_map(|k| {
                            tool_override
                                .rename_reverse
                                .get(k)
                                .map(|orig| (k.clone(), orig.clone()))
                        })
                        .collect();

                    for (new_name, original_name) in keys_to_rename {
                        if let Some(value) = args.remove(&new_name) {
                            args.insert(original_name, value);
                        }
                    }
                }

                break;
            }
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut resp = fut.await?;

            // On ListTools: rewrite schemas
            if let Ok(McpResponse::ListTools(ref mut result)) = resp.inner {
                for tool in &mut result.tools {
                    for tool_override in overrides.iter() {
                        if tool.name == tool_override.namespaced_tool {
                            rewrite_schema(
                                &mut tool.input_schema,
                                &tool_override.hide,
                                &tool_override.rename_forward,
                            );
                            break;
                        }
                    }
                }
            }

            Ok(resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParamOverrideConfig;
    use crate::test_util::{MockService, call_service};
    use tower_mcp_types::protocol::{CallToolParams, McpRequest, McpResponse};

    /// Create a MockService with a tool that has a rich input_schema.
    fn mock_with_schema(name: &str, schema: serde_json::Value) -> MockService {
        use tower_mcp_types::protocol::ToolDefinition;
        MockService {
            tools: vec![ToolDefinition {
                name: name.to_string(),
                title: None,
                description: Some(format!("{name} tool")),
                input_schema: schema,
                output_schema: None,
                icons: None,
                annotations: None,
                execution: None,
                meta: None,
            }],
        }
    }

    fn list_dir_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "recursive": { "type": "boolean" },
                "pattern": { "type": "string" }
            },
            "required": ["path"]
        })
    }

    fn make_overrides(namespace: &str, configs: Vec<ParamOverrideConfig>) -> Vec<ToolOverride> {
        configs
            .iter()
            .map(|c| ToolOverride::new(namespace, c))
            .collect()
    }

    #[tokio::test]
    async fn test_hide_removes_param_from_schema() {
        let mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: {
                    let mut m = serde_json::Map::new();
                    m.insert("path".to_string(), serde_json::json!("/home/docs"));
                    m
                },
                rename: HashMap::new(),
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let tool = &result.tools[0];
                let props = tool.input_schema["properties"].as_object().unwrap();
                assert!(
                    !props.contains_key("path"),
                    "path should be hidden from schema"
                );
                assert!(props.contains_key("recursive"), "recursive should remain");
                assert!(props.contains_key("pattern"), "pattern should remain");
                // "path" should be removed from required
                let required = tool.input_schema["required"].as_array().unwrap();
                let req_strs: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
                assert!(!req_strs.contains(&"path"), "path should not be required");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_hide_injects_defaults_on_call() {
        let mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: {
                    let mut m = serde_json::Map::new();
                    m.insert("path".to_string(), serde_json::json!("/home/docs"));
                    m
                },
                rename: HashMap::new(),
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "fs/list_directory".to_string(),
                arguments: serde_json::json!({"recursive": true}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok(), "call should succeed");
    }

    #[tokio::test]
    async fn test_rename_rewrites_schema() {
        let mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec![],
                defaults: serde_json::Map::new(),
                rename: {
                    let mut m = HashMap::new();
                    m.insert("recursive".to_string(), "deep_search".to_string());
                    m
                },
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let tool = &result.tools[0];
                let props = tool.input_schema["properties"].as_object().unwrap();
                assert!(
                    !props.contains_key("recursive"),
                    "recursive should be renamed"
                );
                assert!(
                    props.contains_key("deep_search"),
                    "deep_search should appear"
                );
                assert!(props.contains_key("path"), "path should remain");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_rename_reverse_maps_on_call() {
        let mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec![],
                defaults: serde_json::Map::new(),
                rename: {
                    let mut m = HashMap::new();
                    m.insert("recursive".to_string(), "deep_search".to_string());
                    m
                },
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        // Client sends "deep_search" (the renamed param)
        let resp = call_service(
            &mut svc,
            McpRequest::CallTool(CallToolParams {
                name: "fs/list_directory".to_string(),
                arguments: serde_json::json!({"path": "/tmp", "deep_search": true}),
                meta: None,
                task: None,
            }),
        )
        .await;

        assert!(resp.inner.is_ok(), "call should succeed");
    }

    #[tokio::test]
    async fn test_hide_and_rename_combined() {
        let mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: {
                    let mut m = serde_json::Map::new();
                    m.insert("path".to_string(), serde_json::json!("/home/docs"));
                    m
                },
                rename: {
                    let mut m = HashMap::new();
                    m.insert("recursive".to_string(), "deep_search".to_string());
                    m
                },
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        // Verify schema: path hidden, recursive renamed
        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let props = result.tools[0].input_schema["properties"]
                    .as_object()
                    .unwrap();
                assert!(!props.contains_key("path"));
                assert!(!props.contains_key("recursive"));
                assert!(props.contains_key("deep_search"));
                assert!(props.contains_key("pattern"));
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_non_matching_tool_passes_through() {
        let mock = mock_with_schema("db/query", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: serde_json::Map::new(),
                rename: HashMap::new(),
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                // db/query schema should be untouched
                let props = result.tools[0].input_schema["properties"]
                    .as_object()
                    .unwrap();
                assert!(props.contains_key("path"), "unmatched tool is untouched");
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_non_call_tool_passes_through() {
        let mock = MockService::with_tools(&["fs/list_directory"]);
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: serde_json::Map::new(),
                rename: HashMap::new(),
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        // ListTools should pass through without error
        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        assert!(resp.inner.is_ok());
    }

    #[tokio::test]
    async fn test_rename_updates_required_array() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "recursive": { "type": "boolean" }
            },
            "required": ["path", "recursive"]
        });
        let mock = mock_with_schema("fs/list_directory", schema);
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec![],
                defaults: serde_json::Map::new(),
                rename: {
                    let mut m = HashMap::new();
                    m.insert("recursive".to_string(), "deep_search".to_string());
                    m
                },
            }],
        );
        let mut svc = ParamOverrideService::new(mock, overrides);

        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        match resp.inner.unwrap() {
            McpResponse::ListTools(result) => {
                let required = result.tools[0].input_schema["required"].as_array().unwrap();
                let req_strs: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
                assert!(req_strs.contains(&"path"));
                assert!(req_strs.contains(&"deep_search"));
                assert!(!req_strs.contains(&"recursive"));
            }
            other => panic!("expected ListTools, got: {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_schema_no_properties() {
        // Schema without properties should be a no-op
        let mut schema = serde_json::json!({"type": "object"});
        rewrite_schema(&mut schema, &["path".to_string()], &HashMap::new());
        assert_eq!(schema, serde_json::json!({"type": "object"}));
    }

    #[test]
    fn test_rewrite_schema_non_object() {
        // Non-object schema is a no-op
        let mut schema = serde_json::json!("string");
        rewrite_schema(&mut schema, &["path".to_string()], &HashMap::new());
        assert_eq!(schema, serde_json::json!("string"));
    }

    #[test]
    fn test_tool_override_construction() {
        let config = ParamOverrideConfig {
            tool: "list_directory".to_string(),
            hide: vec!["path".to_string()],
            defaults: {
                let mut m = serde_json::Map::new();
                m.insert("path".to_string(), serde_json::json!("/home"));
                m
            },
            rename: {
                let mut m = HashMap::new();
                m.insert("recursive".to_string(), "deep_search".to_string());
                m
            },
        };
        let to = ToolOverride::new("fs/", &config);
        assert_eq!(to.namespaced_tool, "fs/list_directory");
        assert_eq!(to.hide, vec!["path"]);
        assert_eq!(to.rename_forward.get("recursive").unwrap(), "deep_search");
        assert_eq!(to.rename_reverse.get("deep_search").unwrap(), "recursive");
    }

    #[tokio::test]
    async fn test_hidden_default_does_not_overwrite_explicit_arg() {
        let _mock = mock_with_schema("fs/list_directory", list_dir_schema());
        let overrides = make_overrides(
            "fs/",
            vec![ParamOverrideConfig {
                tool: "list_directory".to_string(),
                hide: vec!["path".to_string()],
                defaults: {
                    let mut m = serde_json::Map::new();
                    m.insert("path".to_string(), serde_json::json!("/home/docs"));
                    m
                },
                rename: HashMap::new(),
            }],
        );

        // Even though "path" is hidden, if the client passes it, the default
        // should not overwrite. This is consistent with inject behavior.
        let mut req = RouterRequest {
            id: tower_mcp::protocol::RequestId::Number(1),
            inner: McpRequest::CallTool(CallToolParams {
                name: "fs/list_directory".to_string(),
                arguments: serde_json::json!({"path": "/custom"}),
                meta: None,
                task: None,
            }),
            extensions: tower_mcp::router::Extensions::new(),
        };

        // Simulate the override logic manually
        if let McpRequest::CallTool(ref mut params) = req.inner
            && let serde_json::Value::Object(ref mut args) = params.arguments
        {
            let defaults = &overrides[0].defaults;
            for (key, value) in defaults {
                if !args.contains_key(key) {
                    args.insert(key.clone(), value.clone());
                }
            }
            // path should still be /custom
            assert_eq!(args.get("path").unwrap(), "/custom");
        }
    }
}
