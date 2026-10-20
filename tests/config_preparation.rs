//! One offline configuration contract across public entry points, before effects.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[path = "support/http.rs"]
mod http;

use mcp_proxy::{Proxy, ProxyBuilder, ProxyConfig};
use serde_json::{Value, json};
use std::time::Duration;

const BASE: &str = "[proxy]\nname='preparation'\n[proxy.listen]\n[security]\nadmin_token='manager'\n[[backends]]\nname='first'\ntransport='stdio'\ncommand='touch'\n[[backends]]\nname='api'\ntransport='stdio'\ncommand='echo'\n";

fn cases() -> Vec<(&'static str, Value)> {
    vec![
        ("empty name", json!({"name":""})),
        (
            "unsupported credential forwarding",
            json!({"forward_auth":true}),
        ),
        ("reserved name", json!({"name":"proxy"})),
        ("ambiguous namespace", json!({"name":"nested/name"})),
        ("missing command", json!({"command":null})),
        ("empty command", json!({"command":" "})),
        (
            "bad URL",
            json!({"transport":"http","url":"file:///sentinel-private"}),
        ),
        ("invalid regex", json!({"expose_tools":["re:["]})),
        (
            "hidden invalid regex",
            json!({"canary_of":"first","hide_prompts":["re:["]}),
        ),
        (
            "concurrency overflow",
            json!({"concurrency":{"max_concurrent":usize::MAX}}),
        ),
        ("timer overflow", json!({"timeout":{"seconds":u64::MAX}})),
        (
            "cache expiry overflow",
            json!({"cache":{"tool_ttl_seconds":u64::MAX}}),
        ),
        (
            "breaker allocation overflow",
            json!({"circuit_breaker":{"minimum_calls":usize::MAX}}),
        ),
        (
            "hedge overflow",
            json!({"hedging":{"max_hedges":usize::MAX}}),
        ),
        ("missing reference", json!({"mirror_of":"missing"})),
        (
            "alias collision",
            json!({"aliases":[{"from":"one","to":"same"},{"from":"two","to":"same"}]}),
        ),
        (
            "secret resolution",
            json!({"bearer_token":"${MCP_PROXY_287_MISSING_8647578764758645}"}),
        ),
    ]
}
fn config(marker: &std::path::Path, patch: &Value) -> ProxyConfig {
    let mut value = serde_json::to_value(ProxyConfig::parse(BASE).unwrap()).unwrap();
    value["backends"][0]["args"] = json!([marker.to_str().unwrap()]);
    for (key, value_patch) in patch.as_object().unwrap() {
        value["backends"][1][key] = value_patch.clone();
    }
    serde_json::from_value(value).unwrap()
}
fn rejected<T>(result: anyhow::Result<T>) -> String {
    match result {
        Ok(_) => panic!("invalid configuration unexpectedly accepted"),
        Err(error) => format!("{error:#}"),
    }
}
async fn cli(path: &std::path::Path, check: bool) -> String {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"));
    command.arg("--config").arg(path).kill_on_drop(true);
    if check {
        command.arg("--check");
    }
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("invalid startup must finish before backend work")
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !diagnostic.contains("panicked") && !diagnostic.contains("sentinel-private"),
        "{diagnostic}"
    );
    diagnostic
}

#[tokio::test]
async fn every_backend_error_matches_prepare_library_builder_and_cli_before_any_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.toml");
    for (name, patch) in cases() {
        let mut candidate = config(&marker, &patch);
        let expected = rejected(candidate.prepare());
        assert!(!expected.contains("sentinel-private"));
        let text = toml::to_string(&config(&marker, &patch)).unwrap();
        std::fs::write(&path, text).unwrap();
        let error = rejected(
            tokio::time::timeout(
                Duration::from_secs(5),
                Proxy::from_config(config(&marker, &patch)),
            )
            .await
            .unwrap(),
        );
        assert_eq!(error, expected, "library: {name}");
        let bad_backend = serde_json::to_value(&config(&marker, &patch).backends[1]).unwrap();
        let builder = ProxyBuilder::new("preparation")
            .stdio_backend("first", "touch", &[marker.to_str().unwrap()])
            .stdio_backend("api", "echo", &[])
            .configure_backend(|backend| *backend = serde_json::from_value(bad_backend).unwrap());
        assert_eq!(
            rejected(
                tokio::time::timeout(Duration::from_secs(5), builder.build())
                    .await
                    .unwrap()
            ),
            expected,
            "builder: {name}"
        );
        for check in [true, false] {
            assert!(
                cli(&path, check).await.contains(&expected),
                "CLI check={check}: {name}, expected {expected}"
            );
        }
        assert!(
            !marker.exists(),
            "{name}: validation started an earlier backend"
        );
    }
}

#[test]
fn unknown_fields_rejected_in_json_toml_and_yaml_including_nested_policy_sections() {
    for pointer in [
        "",
        "/proxy",
        "/proxy/listen",
        "/security",
        "/performance",
        "/performance/coalesce",
        "/cache",
        "/observability",
        "/observability/access_log",
        "/observability/tracing",
        "/observability/metrics",
        "/backends/0",
    ] {
        let mut value = serde_json::to_value(ProxyConfig::parse(BASE).unwrap()).unwrap();
        value.pointer_mut(pointer).unwrap()["sentinel_unknown_policy"] = json!(true);
        assert!(
            serde_json::from_value::<ProxyConfig>(value.clone()).is_err(),
            "JSON: {pointer}"
        );
        fn remove_nulls(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.retain(|_, v| !v.is_null());
                    for value in map.values_mut() {
                        remove_nulls(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        remove_nulls(value);
                    }
                }
                _ => {}
            }
        }
        remove_nulls(&mut value);
        let toml = toml::to_string(&value).unwrap();
        assert!(ProxyConfig::parse(&toml).is_err(), "TOML: {pointer}");
        #[cfg(feature = "yaml")]
        assert!(
            ProxyConfig::parse_yaml(&yaml_serde::to_string(&value).unwrap()).is_err(),
            "YAML: {pointer}"
        );
    }
    for field in [
        "timeout",
        "concurrency",
        "rate_limit",
        "circuit_breaker",
        "retry",
        "outlier_detection",
        "hedging",
        "cache",
    ] {
        let mut value = serde_json::to_value(ProxyConfig::parse(BASE).unwrap()).unwrap();
        value["backends"][0][field] = json!({"sentinel_unknown_policy":true});
        assert!(
            serde_json::from_value::<ProxyConfig>(value).is_err(),
            "nested {field}"
        );
    }
    let mut value = serde_json::to_value(ProxyConfig::parse(BASE).unwrap()).unwrap();
    value["auth"] = json!({"type":"bearer","tokens":["data"],"scoped_toknes":[]});
    assert!(
        serde_json::from_value::<ProxyConfig>(value).is_err(),
        "unknown auth fields must not disable a policy"
    );
    let mut value = serde_json::to_value(ProxyConfig::parse(BASE).unwrap()).unwrap();
    value["auth"] = json!({"type":"jwt","issuer":"https://issuer.test","audience":"https://resource.test/mcp","jwks_uri":"https://issuer.test/jwks","token_validation":"introspection"});
    assert!(
        serde_json::from_value::<ProxyConfig>(value).is_err(),
        "JWT must not ignore OAuth-only policies"
    );
}

#[tokio::test]
async fn management_validation_and_additions_reject_before_mutating_or_connecting() {
    use tower_mcp::{CallToolResult, HttpTransport, McpRouter, ToolBuilder};
    let mut backend = http::Server::start(
        HttpTransport::new(
            McpRouter::new().tool(
                ToolBuilder::new("ping")
                    .handler(|_: Value| async { Ok(CallToolResult::text("pong")) })
                    .build(),
            ),
        )
        .into_router(),
    )
    .await;
    let text = format!(
        "[proxy]\nname='preparation'\n[proxy.listen]\n[security]\nadmin_token='manager'\n[[backends]]\nname='first'\ntransport='http'\nurl='{}'\n",
        backend.url
    );
    let mut app = app::App::embedded(&text).await;
    let admin = http::Session::start(&app.url, "manager").await;
    let data = http::Session::start(&app.url, "").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    for (name, patch) in cases() {
        let mut candidate = config(&marker, &patch);
        let expected = rejected(candidate.prepare());
        let text = toml::to_string(&config(&marker, &patch)).unwrap();
        for (method, route) in [
            (reqwest::Method::POST, "config/validate"),
            (reqwest::Method::PUT, "config"),
        ] {
            let reply = http::client()
                .request(method, format!("{}/admin/{route}", app.url))
                .bearer_auth("manager")
                .body(text.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), reqwest::StatusCode::BAD_REQUEST, "{name}");
            assert!(reply.text().await.unwrap().contains(&expected), "{name}");
        }
    }
    // A listening sentinel catches even an attempted TCP connection. No timeout
    // inference: all rejected operations have completed before try_accept.
    let sentinel = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    sentinel.set_nonblocking(true).unwrap();
    let url = format!("http://{}", sentinel.local_addr().unwrap());
    for name in ["", "proxy", "bad/name"] {
        let reply = http::client()
            .post(format!("{}/admin/backends/add", app.url))
            .bearer_auth("manager")
            .json(&json!({"name":name,"url":url}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), reqwest::StatusCode::BAD_REQUEST);
        let reply = admin
            .call("proxy/add_backend", json!({"name":name,"url":url}))
            .await;
        assert_eq!(reply["result"]["isError"], true, "{reply}");
    }
    let reply=http::client().post(format!("{}/admin/backends/add",app.url)).bearer_auth("manager").json(&json!({"name":"new","url":url,"bearer_token":"${MCP_PROXY_287_MISSING_8647578764758645}"})).send().await.unwrap();
    assert_eq!(reply.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        sentinel.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    // Invalid direct replacement must not remove the active backend.
    let response = http::client()
        .put(format!("{}/admin/backends/first", app.url))
        .bearer_auth("manager")
        .json(&json!({"url":"file:///sentinel-private"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!response.text().await.unwrap().contains("sentinel-private"));
    assert!(!marker.exists());
    let listed = data.rpc("tools/list", json!({})).await;
    assert_eq!(http::success(&listed)["tools"].as_array().unwrap().len(), 1);
    assert_eq!(
        http::success(&data.call("first/ping", json!({})).await)["content"][0]["text"],
        "pong"
    );
    data.terminate().await;
    admin.terminate().await;
    app.shutdown().await;
    backend.stop().await;
}

#[test]
fn fallible_cache_constructors_reject_extreme_expiry_capacity_and_bad_storage() {
    use mcp_proxy::{
        cache::{CacheLayer, CacheService},
        config::{BackendCacheConfig, CacheBackendConfig},
    };
    let mut policy = BackendCacheConfig {
        resource_ttl_seconds: 1,
        tool_ttl_seconds: u64::MAX,
        max_entries: 100,
    };
    let backend = CacheBackendConfig::default();
    assert!(CacheLayer::try_new(vec![("api/".into(), &policy)], &backend).is_err());
    assert!(CacheService::try_new((), vec![("api/".into(), &policy)], &backend).is_err());
    policy.tool_ttl_seconds = 1;
    policy.max_entries = 0;
    assert!(CacheLayer::try_new(vec![("api/".into(), &policy)], &backend).is_err());
    policy.max_entries = 100;
    let mut invalid = backend.clone();
    invalid.backend = "typo".into();
    assert!(CacheLayer::try_new(vec![("api/".into(), &policy)], &invalid).is_err());
    assert!(CacheLayer::try_new(vec![("api/".into(), &policy)], &backend).is_ok());
    #[cfg(feature = "redis-cache")]
    {
        invalid.backend = "redis".into();
        for url in [
            "redis://127.0.0.1:6379/0",
            "redis+unix:///tmp/mcp-proxy-not-opened.sock",
        ] {
            invalid.url = Some(url.into());
            assert!(CacheLayer::try_new(vec![("api/".into(), &policy)], &invalid).is_ok());
        }
        invalid.url = Some("redis://sentinel-private:password@localhost/not-a-db".into());
        let error = match CacheLayer::try_new(vec![("api/".into(), &policy)], &invalid) {
            Ok(_) => panic!("invalid database accepted"),
            Err(error) => error,
        };
        assert!(!format!("{error:#?}").contains("sentinel-private"));
    }
}

#[test]
fn duration_allocation_and_reference_limits_check_boundaries_without_runtime_work() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    for (section, field, min, max) in [
        ("timeout", "seconds", 1, 31_536_000u64),
        ("rate_limit", "period_seconds", 1, 31_536_000),
        ("rate_limit", "requests", 1, 1_000_000),
        ("concurrency", "max_concurrent", 1, 1_000_000),
        ("circuit_breaker", "minimum_calls", 1, 1_000_000),
        (
            "circuit_breaker",
            "permitted_calls_in_half_open",
            1,
            1_000_000,
        ),
        ("circuit_breaker", "wait_duration_seconds", 1, 31_536_000),
        ("outlier_detection", "base_ejection_seconds", 1, 31_536_000),
        ("outlier_detection", "interval_seconds", 1, 31_536_000),
        ("outlier_detection", "max_ejection_percent", 0, 100),
        ("hedging", "max_hedges", 0, 16),
        ("hedging", "delay_ms", 0, 31_536_000_000),
        ("cache", "tool_ttl_seconds", 0, 31_536_000),
        ("cache", "resource_ttl_seconds", 0, 31_536_000),
        ("cache", "max_entries", 1, 1_000_000),
    ] {
        for (value, valid) in [
            (min, true),
            (max, true),
            (max + 1, false),
            (
                if field == "max_ejection_percent" {
                    u32::MAX as u64
                } else {
                    u64::MAX
                },
                false,
            ),
        ] {
            let mut settings = match section {
                "timeout" => json!({"seconds":1}),
                "rate_limit" => json!({"requests":1}),
                "concurrency" => json!({"max_concurrent":1}),
                _ => json!({}),
            };
            settings[field] = json!(value);
            let mut patch = json!({});
            patch[section] = settings;
            assert_eq!(
                config(&marker, &patch).prepare().is_ok(),
                valid,
                "{section}.{field}={value}"
            );
        }
        if min == 1 {
            let mut settings = if section == "rate_limit" {
                json!({"requests":1})
            } else {
                json!({})
            };
            settings[field] = json!(0);
            let mut patch = json!({});
            patch[section] = settings;
            assert!(
                config(&marker, &patch).prepare().is_err(),
                "{section}.{field}=0"
            );
        }
    }
    for patch in [
        json!({"mirror_of":"api"}),
        json!({"mirror_of":"first","failover_for":"first"}),
        json!({"param_overrides":[{"tool":"work","rename":{"a":"b","b":"a"}}]}),
    ] {
        assert!(config(&marker, &patch).prepare().is_err());
    }
    let mut cycle = config(&marker, &json!({"mirror_of":"first"}));
    cycle.backends[0].mirror_of = Some("api".into());
    assert!(cycle.prepare().is_err());
    for (separator, one, two) in [("aba", "a", "aab"), ("roxy", "first", "p")] {
        let mut overlapping = config(&marker, &json!({}));
        overlapping.proxy.separator = separator.into();
        overlapping.backends[0].name = one.into();
        overlapping.backends[1].name = two.into();
        assert!(rejected(overlapping.prepare()).contains("namespace prefixes"));
    }
    let mut composite = config(&marker, &json!({}));
    composite.composite_tools = vec![
        serde_json::from_value(
            json!({"name":"proxy/hijack","description":"test","tools":["api/work"]}),
        )
        .unwrap(),
    ];
    assert!(composite.prepare().is_err());
    composite.composite_tools[0].name = "combined".into();
    composite.composite_tools[0].tools = vec!["missing/work".into()];
    assert!(composite.prepare().is_err());
    composite.composite_tools[0].tools = vec!["api/work".into()];
    assert!(composite.prepare().is_ok());
    // The internal namespace is a real target. Existing configured authority
    // regressions still require ordinary/indirect access to be denied at runtime.
    composite.composite_tools[0].tools = vec!["proxy/config".into()];
    assert!(composite.prepare().is_ok());
    assert!(!marker.exists());
}

#[tokio::test]
async fn invalid_rbac_is_rejected_identically_before_backend_effects_even_without_oauth() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.toml");
    let mut value = serde_json::to_value(config(&marker, &json!({}))).unwrap();
    value["auth"] = json!({"type":"jwt","issuer":"https://issuer.test","audience":"https://resource.test/mcp","jwks_uri":"https://issuer.test/jwks","roles":[{"name":"reader"}],"role_mapping":{"claim":"scope","mapping":{"read":"undefined"}}});
    let mut candidate: ProxyConfig = serde_json::from_value(value.clone()).unwrap();
    let expected = rejected(candidate.prepare());
    assert!(expected.contains("auth.role"));
    std::fs::write(&path, toml::to_string(&candidate).unwrap()).unwrap();
    assert_eq!(
        rejected(Proxy::from_config(serde_json::from_value(value).unwrap()).await),
        expected
    );
    for check in [true, false] {
        assert!(cli(&path, check).await.contains(&expected));
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn enabled_features_are_checked_offline_consistently_for_this_binary() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.toml");
    let checks = [
        (
            "oauth",
            cfg!(feature = "oauth"),
            json!({"auth":{"type":"bearer","scoped_tokens":[{"token":"data","allow_tools":["api/work"]}]}}),
        ),
        (
            "oauth",
            cfg!(feature = "oauth"),
            json!({"auth":{"type":"jwt","issuer":"https://issuer.test","audience":"https://resource.test/mcp","jwks_uri":"https://issuer.test/jwks"}}),
        ),
        (
            "discovery",
            cfg!(feature = "discovery"),
            json!({"discovery":true}),
        ),
        (
            "otel",
            cfg!(feature = "otel"),
            json!({"tracing":{"enabled":true,"endpoint":"http://localhost:4317","service_name":"preparation"}}),
        ),
        (
            "metrics",
            cfg!(feature = "metrics"),
            json!({"metrics":{"enabled":true}}),
        ),
        (
            "websocket",
            cfg!(feature = "websocket"),
            json!({"backend":{"transport":"websocket","command":null,"url":"ws://127.0.0.1:9/mcp"}}),
        ),
        (
            "redis-cache",
            cfg!(feature = "redis-cache"),
            json!({"cache":{"backend":"redis","url":"redis://127.0.0.1:9/0"}}),
        ),
        (
            "sqlite-cache",
            cfg!(feature = "sqlite-cache"),
            json!({"cache":{"backend":"sqlite","url":dir.path().join("not-created.db").to_str().unwrap()}}),
        ),
    ];
    for (feature, supported, patch) in checks {
        let mut value = serde_json::to_value(config(&marker, &json!({}))).unwrap();
        if patch.get("auth").is_some() {
            value["auth"] = patch["auth"].clone();
        }
        if patch.get("discovery").is_some() {
            value["proxy"]["tool_discovery"] = patch["discovery"].clone();
        }
        for field in ["tracing", "metrics"] {
            if patch.get(field).is_some() {
                value["observability"][field] = patch[field].clone();
            }
        }
        if let Some(patch) = patch.get("backend") {
            for (field, replacement) in patch.as_object().unwrap() {
                value["backends"][1][field] = replacement.clone();
            }
        }
        if let Some(cache) = patch.get("cache") {
            value["cache"] = cache.clone();
        }
        let mut candidate: ProxyConfig = serde_json::from_value(value.clone()).unwrap();
        let result = candidate.prepare();
        if supported {
            result.unwrap();
            continue;
        }
        let expected = rejected(result);
        assert!(
            expected.contains(&format!("requires the '{feature}' feature")),
            "{expected}"
        );
        std::fs::write(&path, toml::to_string(&candidate).unwrap()).unwrap();
        assert_eq!(
            rejected(Proxy::from_config(serde_json::from_value(value).unwrap()).await),
            expected
        );
        for check in [true, false] {
            assert!(cli(&path, check).await.contains(&expected));
        }
    }
    assert!(!marker.exists());
    assert!(!dir.path().join("not-created.db").exists());
}

#[cfg(not(feature = "yaml"))]
#[tokio::test]
async fn yaml_files_fail_explicitly_before_effects_without_feature() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.yaml");
    std::fs::write(&path, format!("proxy:\n  name: feature-boundary\n  listen: {{}}\nbackends:\n  - name: first\n    transport: stdio\n    command: touch\n    args: ['{}']\n", marker.display())).unwrap();
    for check in [true, false] {
        let error = cli(&path, check).await;
        assert!(
            error.contains("yaml") && error.contains("feature"),
            "{error}"
        );
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn nested_composites_are_rejected_before_effects_instead_of_at_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.toml");
    let mut value = serde_json::to_value(config(&marker, &json!({}))).unwrap();
    value["composite_tools"] = json!([
        {"name":"allowed_composite","description":"ordinary target","tools":["api/public"]},
        {"name":"nested_composite","description":"unsupported nesting","tools":["allowed_composite"]}
    ]);
    let mut candidate: ProxyConfig = serde_json::from_value(value.clone()).unwrap();
    let expected = rejected(candidate.prepare());
    assert!(expected.contains("composite_tools target"));
    std::fs::write(&path, toml::to_string(&candidate).unwrap()).unwrap();
    assert_eq!(
        rejected(Proxy::from_config(serde_json::from_value(value).unwrap()).await),
        expected
    );
    for check in [true, false] {
        assert!(cli(&path, check).await.contains(&expected));
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn invalid_listen_host_is_rejected_before_backend_work() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let path = dir.path().join("proxy.toml");
    for host in [
        "",
        "bad host",
        "http://localhost",
        "localhost:8080",
        "bad..host",
    ] {
        let mut candidate = config(&marker, &json!({}));
        candidate.proxy.listen.host = host.into();
        let expected = rejected(candidate.prepare());
        assert!(expected.contains("proxy.listen.host"));
        std::fs::write(&path, toml::to_string(&candidate).unwrap()).unwrap();
        assert_eq!(rejected(Proxy::from_config(candidate).await), expected);
        let builder = ProxyBuilder::new("preparation")
            .listen(host, 8080)
            .stdio_backend("first", "touch", &[marker.to_str().unwrap()]);
        assert_eq!(rejected(builder.build().await), expected);
        for check in [true, false] {
            assert!(cli(&path, check).await.contains(&expected));
        }
    }
    for host in [
        "127.0.0.1",
        "::1",
        "[::1]",
        "localhost",
        "mcp-proxy.default.svc.cluster.local",
    ] {
        let mut candidate = config(&marker, &json!({}));
        candidate.proxy.listen.host = host.into();
        candidate.prepare().unwrap();
    }
    assert!(!marker.exists());
}
