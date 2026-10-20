//! Pure, bounded invariants shared by proptest and the coverage-guided fuzzers.
#![allow(dead_code)]
use futures_util::FutureExt;
use mcp_proxy::{
    ProxyConfig,
    alias::{AliasMap, AliasService},
    inject::{InjectArgsService, InjectionRules},
    param_override::{ParamOverrideService, ToolOverride},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_mcp::protocol::{CallToolParams, McpRequest, RequestId};
use tower_mcp::{Extensions, McpResponse, RequestContext, RouterRequest, RouterResponse};

pub const MAX_INPUT: usize = 4096;
fn roundtrip(config: ProxyConfig) {
    let canonical = serde_json::to_value(&config).unwrap();
    let recovered: ProxyConfig = serde_json::from_value(canonical.clone()).unwrap();
    assert_eq!(serde_json::to_value(&recovered).unwrap(), canonical);
    // TOML cannot represent every YAML/JSON value (notably null arguments).
    // When representable, its round-trip must preserve the same configuration.
    if let Ok(text) = toml::to_string(&config) {
        let parsed = ProxyConfig::parse(&text).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), canonical);
    }
}
pub fn parsing(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT {
        return false;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return false;
    };
    let mut accepted = false;
    if let Ok(config) = ProxyConfig::parse(text) {
        roundtrip(config);
        accepted = true;
    }
    #[cfg(feature = "yaml")]
    if let Ok(config) = ProxyConfig::parse_yaml(text) {
        roundtrip(config);
        accepted = true;
    }
    // Deliberately no prepare/from_config: fuzz inputs cannot read environment
    // secrets, connect to stores/IdPs/backends, or launch arbitrary executables.
    accepted
}

pub fn transformation(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT {
        return false;
    }
    let Ok(wire) = serde_json::from_slice::<Value>(data) else {
        return false;
    };
    let Ok(params) = serde_json::from_value::<CallToolParams>(wire["params"].clone()) else {
        return false;
    };
    let Ok(id) = serde_json::from_value::<RequestId>(wire["id"].clone()) else {
        return false;
    };
    let original = serde_json::to_value(&params).unwrap();
    let mut expected = original.clone();
    let name = if params.name == "api/public" {
        "api/echo"
    } else {
        params.name.as_str()
    };
    expected["name"] = json!(name);
    if let Some(args) = expected["arguments"].as_object_mut() {
        if name == "api/echo" {
            args.entry("internal").or_insert(json!(42));
            if let Some(value) = args.remove("public_arg") {
                args.insert("original".into(), value);
            }
        }
        if name.starts_with("api/") {
            args.entry("region").or_insert(json!("default"));
        }
        if name == "api/echo" {
            args.insert("injected".into(), json!(true));
        }
    }
    let mut extensions = Extensions::new();
    extensions.insert(String::from("verified-caller"));
    let context = RequestContext::new(id.clone());
    context.cancellation_token().cancel();
    extensions.insert(context);
    let expected_id = id.clone();
    let inner = tower::service_fn(move |request: RouterRequest| {
        assert_eq!(request.id, expected_id);
        assert_eq!(
            request.extensions.get::<String>().unwrap(),
            "verified-caller"
        );
        assert!(
            request
                .extensions
                .get::<RequestContext>()
                .unwrap()
                .is_cancelled()
        );
        async move {
            let McpRequest::CallTool(params) = request.inner else {
                panic!("call transformed into another method");
            };
            Ok::<_, std::convert::Infallible>(RouterResponse {
                id: request.id,
                inner: Ok(McpResponse::CallTool(tower_mcp::CallToolResult::json(
                    serde_json::to_value(params).unwrap(),
                ))),
            })
        }
    });
    let rules = InjectionRules::new(
        "api/".into(),
        serde_json::from_value(json!({"region":"default"})).unwrap(),
        vec![
            serde_json::from_value(
                json!({"tool":"echo","args":{"injected":true},"overwrite":true}),
            )
            .unwrap(),
        ],
    );
    let overrides = ToolOverride::new("api/", &serde_json::from_value(json!({"tool":"echo","hide":["internal"],"defaults":{"internal":42},"rename":{"original":"public_arg"}})).unwrap());
    let service = AliasService::new(
        ParamOverrideService::new(InjectArgsService::new(inner, vec![rules]), vec![overrides]),
        AliasMap::new(vec![("api/".into(), "echo".into(), "public".into())]).unwrap(),
    );
    // These pure transforms wrap an immediately ready capture service. A pending
    // result would be a regression, not a reason to wait or start another runtime.
    let response = service
        .oneshot(RouterRequest {
            id: id.clone(),
            inner: McpRequest::CallTool(params),
            extensions,
        })
        .now_or_never()
        .expect("pure transforms must complete on first poll")
        .unwrap();
    assert_eq!(response.id, id);
    let McpResponse::CallTool(result) = response.inner.unwrap() else {
        panic!("wrong response method");
    };
    let observed: Value = serde_json::from_str(&result.all_text()).unwrap();
    assert_eq!(
        observed, expected,
        "only the configured names/arguments may change"
    );
    true
}
