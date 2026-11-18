//! Per-tool execution policy: which tool calls the proxy may send more than
//! once, or answer without sending.
//!
//! Retry, hedging, mirroring and failover can execute one client call several
//! times; caching and coalescing can answer it without executing it. Each
//! backend classifies its tools by [`Effect`], and every one of these features
//! checks the effect of the call it handles:
//!
//! | Effect | Retry, hedging | Failover re-send | Mirror | Cache, coalescing |
//! |---|---|---|---|---|
//! | `read` | yes | yes | yes | yes |
//! | `idempotent` | yes | yes | yes | no |
//! | `write` (default) | no | only when the primary never received the call | no | no |
//!
//! Tools no entry matches take the backend's `default_effect`, which is
//! `write`. A backend that enables any of these features applies them only to
//! tools listed as `read` or `idempotent`; `default_effect = "read"` restores
//! the behavior of releases before 0.9, where every tool was treated as `read`.
//!
//! # Configuration
//!
//! Configuration fragment: append this block after [proxy.toml](https://github.com/joshrotenberg/mcp-proxy/blob/main/tests/fixtures/documentation/proxy.toml).
//! No optional features required.
//!
//! <!-- doc-config: fragment=tests/fixtures/documentation/proxy.toml features=none -->
//! ```toml
//! [[backends]]
//! name = "api"
//! transport = "http"
//! url = "http://localhost:8080"
//! default_effect = "write"        # default: unlisted tools are sent once
//!
//! [backends.retry]
//! max_retries = 2
//!
//! [backends.cache]
//! tool_ttl_seconds = 30
//!
//! [[backends.tool_effects]]
//! tools = ["get_*", "list_*", "search"]
//! effect = "read"                 # retried, cached, coalesced
//!
//! [[backends.tool_effects]]
//! tools = ["put_record"]
//! effect = "idempotent"           # retried, never cached
//!
//! [[backends.tool_effects]]
//! tools = ["create_order"]
//! effect = "write"
//! idempotency_key = "request_id"  # retried only when the call carries one
//! ```
//!
//! - `tools` lists backend-local names or wildcard patterns, with the syntax
//!   of `expose_tools` and `hide_tools`. The first matching entry wins.
//! - `idempotency_key` names a backend argument (after `param_overrides` and
//!   `inject_args`). A `write` call with a non-empty value for it is treated as
//!   `idempotent` for that call, because the backend can deduplicate by key. It
//!   never enables caching or coalescing.
//! - Upstream tool annotations (`readOnlyHint`, `idempotentHint`) are not
//!   used: a backend that marks a mutating tool read-only by mistake would be
//!   replayed silently.
//! - Resources and prompts are unaffected; the policy covers tool calls.
//!
//! # Failover
//!
//! Failover re-sends a call only when the primary's outcome is a backend
//! failure (internal, server or transport error), never after a client error
//! such as `-32602` or a tool `isError` result. A `write` call without an
//! idempotency key is re-sent only when it was rejected inside the proxy
//! before reaching the primary: an open circuit breaker, a rate or
//! concurrency limit, an outlier ejection, or a timeout while queued. A
//! connection failure to the primary is not proof that the call was not
//! received, so with only failover configured a `write` call is not re-sent;
//! adding a circuit breaker lets writes fail over once it opens.
//!
//! Changing `tool_effects` or `default_effect` requires a restart. Backends
//! added at runtime carry no policy fields, so their tools are `write`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use tower::util::BoxCloneService;
use tower::{Service, ServiceExt};
use tower_mcp::router::{RouterRequest, RouterResponse};
use tower_mcp_types::protocol::McpRequest;

pub use crate::config::Effect;
use crate::config::{BackendConfig, ProxyConfig};

/// One backend's tool classification.
#[derive(Debug, Clone)]
pub struct BackendEffects {
    default: Effect,
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    tools: Vec<String>,
    effect: Effect,
    idempotency_key: Option<String>,
}

impl BackendEffects {
    /// Build from a backend's `default_effect` and `tool_effects`.
    pub fn from_backend(backend: &BackendConfig) -> Self {
        Self {
            default: backend.default_effect,
            rules: backend
                .tool_effects
                .iter()
                .map(|entry| Rule {
                    tools: entry.tools.clone(),
                    effect: entry.effect,
                    idempotency_key: entry.idempotency_key.clone(),
                })
                .collect(),
        }
    }

    fn rule(&self, tool: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| {
            rule.tools
                .iter()
                .any(|pattern| crate::config::wildcard_matches(pattern, tool))
        })
    }

    /// The configured effect of a backend-local tool, ignoring arguments.
    pub fn configured(&self, tool: &str) -> Effect {
        self.rule(tool).map_or(self.default, |rule| rule.effect)
    }

    /// The effect of one call: a `write` call carrying a non-empty value for
    /// its idempotency key is `idempotent`.
    pub fn call(&self, tool: &str, arguments: &serde_json::Value) -> Effect {
        let Some(rule) = self.rule(tool) else {
            return self.default;
        };
        match (&rule.effect, &rule.idempotency_key) {
            (Effect::Write, Some(key)) if has_value(arguments.get(key)) => Effect::Idempotent,
            (effect, _) => *effect,
        }
    }

    /// Whether a request addressed with backend-local names may be sent more
    /// than once. Requests other than tool calls are unaffected.
    pub(crate) fn replayable(&self, request: &McpRequest) -> bool {
        match request {
            McpRequest::CallTool(params) => {
                self.call(&params.name, &params.arguments) != Effect::Write
            }
            _ => true,
        }
    }

    /// Number of configured rules and whether any tool can resolve to `write`.
    fn summary(&self) -> (usize, bool) {
        let write = self.default == Effect::Write
            || self.rules.iter().any(|rule| rule.effect == Effect::Write);
        (self.rules.len(), write)
    }
}

fn has_value(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Every backend's tool classification, addressed by namespaced names.
///
/// ```
/// use mcp_proxy::config::{Effect, ProxyConfig};
/// use mcp_proxy::execution::ExecutionPolicy;
///
/// let config = ProxyConfig::parse(r#"
/// [proxy]
/// name = "gw"
/// [proxy.listen]
/// [[backends]]
/// name = "api"
/// transport = "http"
/// url = "http://localhost:8080"
/// [[backends.tool_effects]]
/// tools = ["get_*"]
/// effect = "read"
/// "#).unwrap();
/// let policy = ExecutionPolicy::from_config(&config);
/// assert_eq!(policy.configured("api/get_user"), Effect::Read);
/// assert_eq!(policy.configured("api/delete_user"), Effect::Write);
/// ```
#[derive(Debug, Clone)]
pub struct ExecutionPolicy {
    /// `(namespace prefix, effects)`, longest prefix first.
    backends: Vec<(String, Arc<BackendEffects>)>,
    by_name: HashMap<String, Arc<BackendEffects>>,
}

impl ExecutionPolicy {
    /// Build the table for every configured backend.
    pub fn from_config(config: &ProxyConfig) -> Self {
        let mut backends: Vec<(String, Arc<BackendEffects>)> = Vec::new();
        let mut by_name = HashMap::new();
        for backend in &config.backends {
            let effects = Arc::new(BackendEffects::from_backend(backend));
            backends.push((
                format!("{}{}", backend.name, config.proxy.separator),
                effects.clone(),
            ));
            by_name.insert(backend.name.clone(), effects);
        }
        backends.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        Self { backends, by_name }
    }

    /// A backend's classification by backend name.
    pub fn backend(&self, name: &str) -> Option<Arc<BackendEffects>> {
        self.by_name.get(name).cloned()
    }

    fn resolve<'a>(&self, name: &'a str) -> Option<(&BackendEffects, &'a str)> {
        self.backends.iter().find_map(|(prefix, effects)| {
            name.strip_prefix(prefix.as_str())
                .map(|local| (effects.as_ref(), local))
        })
    }

    /// The configured effect of a namespaced tool, ignoring arguments. A name
    /// outside every configured backend is `write`.
    pub fn configured(&self, name: &str) -> Effect {
        self.resolve(name)
            .map_or(Effect::Write, |(effects, local)| effects.configured(local))
    }

    /// The effect of one namespaced call, including its idempotency key.
    pub fn call(&self, name: &str, arguments: &serde_json::Value) -> Effect {
        self.resolve(name)
            .map_or(Effect::Write, |(effects, local)| {
                effects.call(local, arguments)
            })
    }

    /// Whether a response may be stored or shared for this request. Only
    /// `read` tool calls qualify; other requests are unaffected.
    pub(crate) fn shareable(&self, request: &McpRequest) -> bool {
        match request {
            McpRequest::CallTool(params) => self.configured(&params.name) == Effect::Read,
            _ => true,
        }
    }

    /// Whether a namespaced request may be sent more than once.
    pub(crate) fn replayable(&self, request: &McpRequest) -> bool {
        match request {
            McpRequest::CallTool(params) => {
                self.call(&params.name, &params.arguments) != Effect::Write
            }
            _ => true,
        }
    }
}

/// Log, once per backend at startup, which features skip some of its tools.
pub(crate) fn log_protection(config: &ProxyConfig, policy: &ExecutionPolicy) {
    let coalescing = config.performance.coalesce_requests;
    let mirrored: std::collections::HashSet<&str> = config
        .backends
        .iter()
        .filter_map(|b| b.mirror_of.as_deref())
        .collect();
    let failover: std::collections::HashSet<&str> = config
        .backends
        .iter()
        .filter_map(|b| b.failover_for.as_deref())
        .collect();
    for backend in &config.backends {
        let Some(effects) = policy.backend(&backend.name) else {
            continue;
        };
        let (rules, write) = effects.summary();
        if !write {
            continue;
        }
        let mut features = Vec::new();
        if backend.retry.is_some() {
            features.push("retry");
        }
        if backend.hedging.is_some() {
            features.push("hedging");
        }
        if mirrored.contains(backend.name.as_str()) {
            features.push("mirror");
        }
        if failover.contains(backend.name.as_str()) {
            features.push("failover");
        }
        if backend
            .cache
            .as_ref()
            .is_some_and(|c| c.tool_ttl_seconds > 0)
        {
            features.push("cache");
        }
        if coalescing {
            features.push("coalescing");
        }
        if features.is_empty() {
            continue;
        }
        tracing::info!(
            backend = %backend.name,
            default_effect = ?backend.default_effect,
            tool_effect_rules = rules,
            features = %features.join(","),
            "Execution policy: write tools are sent once and never cached or coalesced; \
             list read and idempotent tools in tool_effects, or set default_effect"
        );
    }
}

/// Records whether a request reached a backend's middleware and its
/// transport, so failover can tell a local rejection from a failed call.
#[derive(Clone, Default)]
pub(crate) struct DispatchProbe(Arc<ProbeState>);

#[derive(Default)]
struct ProbeState {
    entered: AtomicBool,
    dispatched: AtomicBool,
}

impl DispatchProbe {
    /// The request entered the backend's middleware and was rejected there
    /// without being handed to the transport. A request that never passed a
    /// marked layer is not a local rejection.
    pub(crate) fn locally_rejected(&self) -> bool {
        self.0.entered.load(Ordering::SeqCst) && !self.0.dispatched.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Mark {
    Entered,
    Dispatched,
}

/// Marks the request's [`DispatchProbe`], if any, then calls the inner service.
#[derive(Clone)]
pub(crate) struct MarkProbe<S> {
    inner: S,
    mark: Mark,
}

impl<S> MarkProbe<S> {
    pub(crate) fn new(inner: S, mark: Mark) -> Self {
        Self { inner, mark }
    }
}

impl<S> Service<RouterRequest> for MarkProbe<S>
where
    S: Service<RouterRequest, Response = RouterResponse, Error = Infallible>,
{
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        if let Some(probe) = req.extensions.get::<DispatchProbe>() {
            let flag = match self.mark {
                Mark::Entered => &probe.0.entered,
                Mark::Dispatched => &probe.0.dispatched,
            };
            flag.store(true, Ordering::SeqCst);
        }
        self.inner.call(req)
    }
}

type InfallibleService = BoxCloneService<RouterRequest, RouterResponse, Infallible>;

/// Sends replayable calls through `replay` (retry and hedging) and every
/// other tool call straight to `direct`.
#[derive(Clone)]
pub(crate) struct EffectBranch {
    effects: Arc<BackendEffects>,
    replay: InfallibleService,
    direct: InfallibleService,
}

impl EffectBranch {
    pub(crate) fn new(
        effects: Arc<BackendEffects>,
        replay: InfallibleService,
        direct: InfallibleService,
    ) -> Self {
        Self {
            effects,
            replay,
            direct,
        }
    }
}

impl Service<RouterRequest> for EffectBranch {
    type Response = RouterResponse;
    type Error = Infallible;
    type Future = tower::util::Oneshot<InfallibleService, RouterRequest>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        // The chosen branch is readied by oneshot in `call`.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RouterRequest) -> Self::Future {
        let branch = if self.effects.replayable(&req.inner) {
            self.replay.clone()
        } else {
            self.direct.clone()
        };
        branch.oneshot(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(backends: &str) -> ExecutionPolicy {
        let config = ProxyConfig::parse(&format!(
            "[proxy]\nname = \"t\"\n[proxy.listen]\n{backends}"
        ))
        .unwrap();
        ExecutionPolicy::from_config(&config)
    }

    const API: &str = r#"
[[backends]]
name = "api"
transport = "http"
url = "http://localhost:1"
[[backends.tool_effects]]
tools = ["get_*", "search"]
effect = "read"
[[backends.tool_effects]]
tools = ["put_{a,b}"]
effect = "idempotent"
[[backends.tool_effects]]
tools = ["create"]
effect = "write"
idempotency_key = "request_id"
"#;

    #[test]
    fn first_match_and_default_resolve_effects() {
        let p = policy(API);
        assert_eq!(p.configured("api/get_user"), Effect::Read);
        assert_eq!(p.configured("api/search"), Effect::Read);
        assert_eq!(p.configured("api/put_a"), Effect::Idempotent);
        assert_eq!(p.configured("api/put_c"), Effect::Write);
        assert_eq!(p.configured("api/create"), Effect::Write);
        assert_eq!(p.configured("api/unlisted"), Effect::Write);
        assert_eq!(p.configured("other/get_user"), Effect::Write);
    }

    #[test]
    fn default_effect_applies_to_unlisted_tools() {
        let p = policy(&API.replace(
            "url = \"http://localhost:1\"",
            "url = \"http://localhost:1\"\ndefault_effect = \"read\"",
        ));
        assert_eq!(p.configured("api/unlisted"), Effect::Read);
        assert_eq!(p.configured("api/create"), Effect::Write);
    }

    #[test]
    fn idempotency_key_value_makes_a_write_call_idempotent() {
        let p = policy(API);
        let call = |args| p.call("api/create", &args);
        assert_eq!(call(json!({"request_id": "r-1"})), Effect::Idempotent);
        assert_eq!(call(json!({"request_id": 7})), Effect::Idempotent);
        assert_eq!(call(json!({"request_id": ""})), Effect::Write);
        assert_eq!(call(json!({"request_id": null})), Effect::Write);
        assert_eq!(call(json!({})), Effect::Write);
        // The key never makes a call shareable.
        let request = McpRequest::CallTool(tower_mcp::protocol::CallToolParams {
            name: "api/create".into(),
            arguments: json!({"request_id": "r-1"}),
            input_responses: None,
            request_state: None,
            meta: None,
            task: None,
        });
        assert!(p.replayable(&request));
        assert!(!p.shareable(&request));
    }

    #[test]
    fn other_requests_are_unaffected() {
        let p = policy(API);
        let request = McpRequest::ReadResource(tower_mcp::protocol::ReadResourceParams {
            uri: "api/file".into(),
            input_responses: None,
            request_state: None,
            meta: None,
        });
        assert!(p.shareable(&request));
        assert!(p.replayable(&request));
    }

    #[test]
    fn probe_reports_local_rejection_only_after_entry_without_dispatch() {
        let probe = DispatchProbe::default();
        assert!(!probe.locally_rejected(), "unmarked counts as dispatched");
        probe.0.entered.store(true, Ordering::SeqCst);
        assert!(probe.locally_rejected());
        probe.0.dispatched.store(true, Ordering::SeqCst);
        assert!(!probe.locally_rejected());
    }
}
