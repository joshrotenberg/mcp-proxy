//! Locked argument values for tool calls.
//!
//! A locked argument always reaches the backend with its configured value.
//! This differs from [`default_args`, `inject_args`](crate::inject) and hidden
//! parameter defaults ([`crate::param_override`]), which a client can override.
//! Use locks for values that are constraints rather than conveniences, such as
//! a tenant identifier or a read-only flag.
//!
//! # Configuration
//!
//! Configuration fragment: append this block after [proxy.toml](https://github.com/joshrotenberg/mcp-proxy/blob/main/tests/fixtures/documentation/proxy.toml).
//! No optional features required.
//!
//! <!-- doc-config: fragment=tests/fixtures/documentation/proxy.toml features=none -->
//! ```toml
//! [[backends]]
//! name = "db"
//! transport = "http"
//! url = "http://db.internal:8080"
//!
//! # Every tool on this backend
//! [[backends.locked_args]]
//! args = { tenant_id = "acme" }
//!
//! # Only these tools; replace conflicting client values instead of rejecting
//! [[backends.locked_args]]
//! tools = ["query", "export"]
//! args = { read_only = true }
//! on_conflict = "replace"
//! ```
//!
//! # Semantics
//!
//! - `tools` lists backend-local tool names. Omitted, the rule applies to every
//!   tool on the backend.
//! - Keys are the backend's own parameter names: aliases and
//!   `param_overrides.rename` are resolved before locks are applied.
//! - Missing or `null` arguments are treated as `{}`. Other non-object
//!   arguments are rejected with `-32602 Invalid params`.
//! - A key the client did not send is inserted. A client value equal to the
//!   locked value is accepted.
//! - A different client value is rejected with `-32602 Invalid params`
//!   (`on_conflict = "reject"`, the default) or replaced
//!   (`on_conflict = "replace"`). The error names the key, never a value.
//! - `ListTools` omits locked keys from each matching tool's `properties` and
//!   `required`.
//! - Backends configured with `canary_of`, `failover_for`, or `mirror_of`
//!   inherit the locks of the backend they name, so rerouted traffic carries
//!   the same values.
//!
//! The service runs innermost in the configured middleware stack, after alias,
//! parameter override, argument injection, canary, failover, and mirror
//! routing, so no other layer can change a locked value afterwards. Rejections
//! are logged at `warn` and replacements at `debug`; with the `metrics` feature
//! both are counted in `mcp_proxy_locked_args_total{backend, tool, outcome}`.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::JsonRpcError;
use tower_mcp_types::protocol::{McpRequest, McpResponse};

pub use crate::config::OnConflict;
use crate::config::{BackendConfig, LockedArgsConfig, ProxyConfig};

/// A resolved lock rule for one backend namespace.
///
/// # Example
///
/// ```rust
/// use mcp_proxy::config::LockedArgsConfig;
/// use mcp_proxy::locked_args::{LockRule, OnConflict};
///
/// let config = LockedArgsConfig {
///     tools: Some(vec!["query".to_string()]),
///     args: serde_json::json!({"read_only": true}).as_object().cloned().unwrap(),
///     on_conflict: OnConflict::Reject,
/// };
/// let rule = LockRule::new("db", "/", &config);
/// # let _ = rule;
/// ```
#[derive(Debug, Clone)]
pub struct LockRule {
    backend: String,
    namespace: String,
    tools: Option<HashSet<String>>,
    args: serde_json::Map<String, serde_json::Value>,
    on_conflict: OnConflict,
}

impl LockRule {
    /// Resolve a [`LockedArgsConfig`] for the backend named `backend`, whose
    /// tools are published as `{backend}{separator}{tool}`.
    pub fn new(backend: &str, separator: &str, config: &LockedArgsConfig) -> Self {
        Self {
            backend: backend.to_string(),
            namespace: format!("{backend}{separator}"),
            tools: config
                .tools
                .as_ref()
                .map(|tools| tools.iter().cloned().collect()),
            args: config.args.clone(),
            on_conflict: config.on_conflict,
        }
    }

    /// The backend-local tool name, if this rule applies to `name`.
    fn local_tool<'a>(&self, name: &'a str) -> Option<&'a str> {
        let local = name.strip_prefix(&self.namespace)?;
        match &self.tools {
            Some(tools) if !tools.contains(local) => None,
            _ => Some(local),
        }
    }
}

/// The lock configurations that apply to `backend`: its own, followed by those
/// of every backend it names in `canary_of`, `failover_for`, or `mirror_of`.
pub(crate) fn effective_configs<'a>(
    config: &'a ProxyConfig,
    backend: &'a BackendConfig,
) -> Vec<&'a LockedArgsConfig> {
    let mut configs: Vec<_> = backend.locked_args.iter().collect();
    for primary in [
        &backend.canary_of,
        &backend.failover_for,
        &backend.mirror_of,
    ]
    .into_iter()
    .flatten()
    {
        if primary == &backend.name {
            continue;
        }
        if let Some(primary) = config.backends.iter().find(|b| &b.name == primary) {
            configs.extend(primary.locked_args.iter());
        }
    }
    configs
}

/// Resolve every backend's effective lock rules, including inherited ones.
pub(crate) fn rules_for_config(config: &ProxyConfig) -> Vec<LockRule> {
    config
        .backends
        .iter()
        .flat_map(|backend| {
            effective_configs(config, backend)
                .into_iter()
                .map(|lock| LockRule::new(&backend.name, &config.proxy.separator, lock))
        })
        .collect()
}

/// Tower layer that produces a [`LockedArgsService`].
///
/// # Example
///
/// ```rust
/// use tower::ServiceBuilder;
/// use mcp_proxy::config::LockedArgsConfig;
/// use mcp_proxy::locked_args::{LockRule, LockedArgsLayer, OnConflict};
/// # let proxy = tower_mcp::McpRouter::new();
///
/// let config = LockedArgsConfig {
///     tools: None,
///     args: serde_json::json!({"tenant_id": "acme"}).as_object().cloned().unwrap(),
///     on_conflict: OnConflict::Reject,
/// };
/// let service = ServiceBuilder::new()
///     .layer(LockedArgsLayer::new(vec![LockRule::new("db", "/", &config)]))
///     .service(proxy);
/// # let _ = service;
/// ```
#[derive(Clone)]
pub struct LockedArgsLayer {
    rules: Arc<Vec<LockRule>>,
}

impl LockedArgsLayer {
    /// Create a layer enforcing `rules`.
    pub fn new(rules: Vec<LockRule>) -> Self {
        Self {
            rules: Arc::new(rules),
        }
    }
}

impl<S> Layer<S> for LockedArgsLayer {
    type Service = LockedArgsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LockedArgsService {
            inner,
            rules: Arc::clone(&self.rules),
        }
    }
}

/// Middleware that enforces locked argument values on `CallTool` and removes
/// locked keys from `ListTools` schemas. See the [module docs](self).
#[derive(Clone)]
pub struct LockedArgsService<S> {
    inner: S,
    rules: Arc<Vec<LockRule>>,
}

impl<S> LockedArgsService<S> {
    /// Wrap `inner`, enforcing `rules`.
    pub fn new(inner: S, rules: Vec<LockRule>) -> Self {
        Self {
            inner,
            rules: Arc::new(rules),
        }
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Rejected,
    Replaced,
}

impl Outcome {
    #[cfg_attr(not(feature = "metrics"), allow(dead_code))]
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Rejected => "rejected",
            Outcome::Replaced => "replaced",
        }
    }
}

fn record(rule: &LockRule, tool: &str, key: &str, outcome: Outcome) {
    match outcome {
        Outcome::Rejected => tracing::warn!(
            backend = %rule.backend,
            tool,
            key,
            "Rejected tool call: conflicting value for a locked argument"
        ),
        Outcome::Replaced => tracing::debug!(
            backend = %rule.backend,
            tool,
            key,
            "Replaced client value for a locked argument"
        ),
    }
    #[cfg(feature = "metrics")]
    metrics::counter!(
        "mcp_proxy_locked_args_total",
        "backend" => rule.backend.clone(),
        "tool" => tool.to_string(),
        "outcome" => outcome.as_str(),
    )
    .increment(1);
}

/// Apply every matching rule to `arguments`. Returns the error to send instead
/// of forwarding when the call must be rejected.
fn enforce(
    rules: &[LockRule],
    name: &str,
    arguments: &mut serde_json::Value,
) -> Result<(), JsonRpcError> {
    let matching: Vec<_> = rules
        .iter()
        .filter_map(|rule| rule.local_tool(name).map(|tool| (rule, tool)))
        .collect();
    if matching.is_empty() {
        return Ok(());
    }
    if arguments.is_null() {
        *arguments = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(args) = arguments.as_object_mut() else {
        return Err(JsonRpcError::invalid_params(
            "tool arguments must be an object",
        ));
    };

    // Decide on rejection before changing anything.
    for (rule, tool) in &matching {
        if rule.on_conflict != OnConflict::Reject {
            continue;
        }
        for (key, locked) in &rule.args {
            if args.get(key).is_some_and(|value| value != locked) {
                record(rule, tool, key, Outcome::Rejected);
                return Err(JsonRpcError::invalid_params(format!(
                    "argument '{key}' is locked by proxy configuration"
                )));
            }
        }
    }
    for (rule, tool) in &matching {
        for (key, locked) in &rule.args {
            match args.get(key) {
                Some(value) if value == locked => {}
                Some(_) => {
                    record(rule, tool, key, Outcome::Replaced);
                    args.insert(key.clone(), locked.clone());
                }
                None => {
                    args.insert(key.clone(), locked.clone());
                }
            }
        }
    }
    Ok(())
}

/// Remove `keys` from a JSON Schema's `properties` and `required`.
fn hide_keys(schema: &mut serde_json::Value, keys: &serde_json::Map<String, serde_json::Value>) {
    let Some(schema) = schema.as_object_mut() else {
        return;
    };
    if let Some(properties) = schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    {
        for key in keys.keys() {
            properties.remove(key);
        }
    }
    if let Some(required) = schema
        .get_mut("required")
        .and_then(|value| value.as_array_mut())
    {
        required.retain(|name| name.as_str().is_none_or(|name| !keys.contains_key(name)));
    }
}

impl<S> Service<RouterRequest> for LockedArgsService<S>
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
        if let McpRequest::CallTool(ref mut params) = req.inner
            && let Err(error) = enforce(&self.rules, &params.name, &mut params.arguments)
        {
            let id = req.id;
            return Box::pin(async move {
                Ok(RouterResponse {
                    id,
                    inner: Err(error),
                })
            });
        }

        let rules = Arc::clone(&self.rules);
        let fut = self.inner.call(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            if let Ok(McpResponse::ListTools(ref mut result)) = resp.inner {
                for tool in &mut result.tools {
                    for rule in rules.iter() {
                        if rule.local_tool(&tool.name).is_some() {
                            hide_keys(&mut tool.input_schema, &rule.args);
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
    use crate::test_util::{ArgsEchoService, MockService, call_service, forwarded_args};
    use serde_json::json;
    use tower_mcp_types::protocol::CallToolParams;

    fn rule(tools: Option<&[&str]>, args: serde_json::Value, on_conflict: OnConflict) -> LockRule {
        LockRule::new(
            "db",
            "/",
            &LockedArgsConfig {
                tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
                args: args.as_object().cloned().unwrap(),
                on_conflict,
            },
        )
    }

    fn call(name: &str, arguments: serde_json::Value) -> McpRequest {
        McpRequest::CallTool(CallToolParams {
            name: name.to_string(),
            arguments,
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        })
    }

    fn tenant_lock(on_conflict: OnConflict) -> LockedArgsService<ArgsEchoService> {
        LockedArgsService::new(
            ArgsEchoService,
            vec![rule(None, json!({"tenant": "acme"}), on_conflict)],
        )
    }

    #[tokio::test]
    async fn absent_key_is_inserted() {
        let mut svc = tenant_lock(OnConflict::Reject);
        let args = forwarded_args(&mut svc, "db/query", json!({"sql": "SELECT 1"})).await;
        assert_eq!(args, json!({"sql": "SELECT 1", "tenant": "acme"}));
    }

    #[tokio::test]
    async fn equal_value_is_accepted() {
        let mut svc = tenant_lock(OnConflict::Reject);
        let args = forwarded_args(&mut svc, "db/query", json!({"tenant": "acme"})).await;
        assert_eq!(args, json!({"tenant": "acme"}));
    }

    #[tokio::test]
    async fn conflicting_value_is_rejected_without_echoing_values() {
        let mut svc = tenant_lock(OnConflict::Reject);
        let resp = call_service(&mut svc, call("db/query", json!({"tenant": "other"}))).await;
        let error = resp.inner.expect_err("conflict must be rejected");
        assert_eq!(error.code, -32602);
        assert!(error.message.contains("'tenant'"));
        assert!(!error.message.contains("acme"));
        assert!(!error.message.contains("other"));
    }

    #[tokio::test]
    async fn conflicting_value_is_replaced() {
        let mut svc = tenant_lock(OnConflict::Replace);
        let args = forwarded_args(&mut svc, "db/query", json!({"tenant": "other"})).await;
        assert_eq!(args, json!({"tenant": "acme"}));
    }

    #[tokio::test]
    async fn null_arguments_receive_locked_values() {
        let mut svc = tenant_lock(OnConflict::Reject);
        let args = forwarded_args(&mut svc, "db/query", serde_json::Value::Null).await;
        assert_eq!(args, json!({"tenant": "acme"}));
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        let mut svc = tenant_lock(OnConflict::Replace);
        let resp = call_service(&mut svc, call("db/query", json!(["tenant"]))).await;
        assert_eq!(resp.inner.expect_err("must reject").code, -32602);
    }

    #[tokio::test]
    async fn tools_list_limits_scope_and_other_backends_are_untouched() {
        let mut svc = LockedArgsService::new(
            ArgsEchoService,
            vec![rule(
                Some(&["query"]),
                json!({"read_only": true}),
                OnConflict::Reject,
            )],
        );
        let args = forwarded_args(&mut svc, "db/query", json!({})).await;
        assert_eq!(args, json!({"read_only": true}));
        let args = forwarded_args(&mut svc, "db/write", json!({})).await;
        assert_eq!(args, json!({}));
        let args = forwarded_args(&mut svc, "dbx/query", json!({"read_only": false})).await;
        assert_eq!(args, json!({"read_only": false}));
        let args = forwarded_args(&mut svc, "other/query", serde_json::Value::Null).await;
        assert_eq!(args, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn reject_rule_wins_over_replace_rule_for_the_same_call() {
        let mut svc = LockedArgsService::new(
            ArgsEchoService,
            vec![
                rule(None, json!({"mode": "safe"}), OnConflict::Replace),
                rule(None, json!({"tenant": "acme"}), OnConflict::Reject),
            ],
        );
        let resp = call_service(
            &mut svc,
            call("db/query", json!({"mode": "fast", "tenant": "other"})),
        )
        .await;
        assert!(resp.inner.is_err());
        let args = forwarded_args(&mut svc, "db/query", json!({"mode": "fast"})).await;
        assert_eq!(args, json!({"mode": "safe", "tenant": "acme"}));
    }

    #[tokio::test]
    async fn list_tools_hides_locked_keys() {
        let mut mock = MockService::with_tools(&["db/query", "db/write"]);
        for tool in &mut mock.tools {
            tool.input_schema = json!({
                "type": "object",
                "properties": {"sql": {"type": "string"}, "tenant": {"type": "string"}},
                "required": ["sql", "tenant"]
            });
        }
        let mut svc = LockedArgsService::new(
            mock,
            vec![rule(
                Some(&["query"]),
                json!({"tenant": "acme"}),
                OnConflict::Reject,
            )],
        );
        let resp = call_service(&mut svc, McpRequest::ListTools(Default::default())).await;
        let Ok(McpResponse::ListTools(result)) = resp.inner else {
            panic!("expected ListTools");
        };
        let query = &result.tools[0].input_schema;
        assert!(query["properties"].get("tenant").is_none());
        assert_eq!(query["required"], json!(["sql"]));
        let write = &result.tools[1].input_schema;
        assert!(write["properties"].get("tenant").is_some());
    }

    #[test]
    fn derived_backends_inherit_primary_locks() {
        let config = ProxyConfig::parse(
            r#"
            [proxy]
            name = "p"
            [proxy.listen]
            [[backends]]
            name = "api"
            transport = "http"
            url = "http://localhost:1"
            [[backends.locked_args]]
            args = { tenant = "acme" }
            [[backends]]
            name = "api-canary"
            transport = "http"
            url = "http://localhost:2"
            canary_of = "api"
            [[backends]]
            name = "api-backup"
            transport = "http"
            url = "http://localhost:3"
            failover_for = "api"
            [[backends]]
            name = "api-shadow"
            transport = "http"
            url = "http://localhost:4"
            mirror_of = "api"
            "#,
        )
        .unwrap();
        let namespaces: Vec<_> = rules_for_config(&config)
            .into_iter()
            .map(|rule| rule.namespace)
            .collect();
        assert_eq!(
            namespaces,
            ["api/", "api-canary/", "api-backup/", "api-shadow/"]
        );
    }
}
