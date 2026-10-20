//! Bounded configuration, transformation and cancellation-schedule properties.
#[path = "support/invariants.rs"]
mod invariants;
use mcp_proxy::ProxyConfig;
use proptest::prelude::*;
use serde_json::{Value, json};

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| json!(n)),
        ".{0,24}".prop_map(Value::String)
    ];
    leaf.prop_recursive(3, 48, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::btree_map("[a-z_]{1,12}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect()))
        ]
    })
}
fn raw() -> Value {
    json!({"proxy":{"name":"properties","listen":{}},"backends":[{"name":"api","transport":"stdio","command":"never-spawned"}]})
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn preparation_is_idempotent_and_format_independent(port in any::<u16>(), timeout in 1u64..500, entries in 1usize..1000, retries in 0u32..8, separator in prop::sample::select(vec!["/","::","|"])) {
        let mut value = raw();
        value["proxy"]["listen"]["port"] = json!(port);
        value["proxy"]["separator"] = json!(separator);
        value["backends"][0]["timeout"] = json!({"seconds":timeout});
        value["backends"][0]["retry"] = json!({"max_retries":retries,"initial_backoff_ms":2,"max_backoff_ms":8,"budget_percent":20.0,"min_retries_per_sec":0});
        value["performance"] = json!({"coalesce_requests":true,"coalesce":{"max_entries":entries,"max_waiters":8,"timeout_seconds":30}});
        let mut from_json: ProxyConfig = serde_json::from_value(value).unwrap();
        from_json.prepare().unwrap();
        let canonical = serde_json::to_value(&from_json).unwrap();
        from_json.prepare().unwrap();
        prop_assert_eq!(serde_json::to_value(&from_json).unwrap(), canonical.clone());
        let text = toml::to_string(&from_json).unwrap();
        let mut from_toml = ProxyConfig::parse(&text).unwrap();
        from_toml.prepare().unwrap();
        prop_assert_eq!(serde_json::to_value(&from_toml).unwrap(), canonical.clone());
        #[cfg(feature = "yaml")]
        {
            let text = yaml_serde::to_string(&from_json).unwrap();
            let mut from_yaml = ProxyConfig::parse_yaml(&text).unwrap();
            from_yaml.prepare().unwrap();
            prop_assert_eq!(serde_json::to_value(from_yaml).unwrap(), canonical);
        }
    }
    #[test]
    fn raw_preparation_and_parser_reject_the_same_generated_invalid_revision(kind in 0usize..5, number in 1_000_001u64..u64::MAX, name in "[a-z]{1,12}") {
        let mut value = raw();
        match kind {
            0 => value["backends"][0]["timeout"] = json!({"seconds":number.saturating_add(31_536_000)}),
            1 => value["backends"][0]["concurrency"] = json!({"max_concurrent":number}),
            2 => value["backends"][0]["forward_auth"] = json!(true),
            3 => value["backends"][0]["mirror_of"] = json!(format!("missing-{name}")),
            _ => { value["backends"][0]["expose_tools"] = json!([name.clone()]); value["backends"][0]["hide_tools"] = json!([name]); }
        }
        let mut candidate: ProxyConfig = serde_json::from_value(value.clone()).unwrap();
        let expected = candidate.prepare().unwrap_err().to_string();
        // TOML integers are signed; the JSON/raw contract additionally covers
        // values beyond its representation. Test format equivalence when expressible.
        if let Ok(text) = toml::to_string(&candidate) {
            prop_assert_eq!(ProxyConfig::parse(&text).unwrap_err().to_string(), expected.clone());
        }
        #[cfg(feature = "yaml")]
        {
            let text = yaml_serde::to_string(&candidate).unwrap();
            prop_assert_eq!(ProxyConfig::parse_yaml(&text).unwrap_err().to_string(), expected);
        }
        prop_assert_eq!(serde_json::to_value(candidate).unwrap(), serde_json::to_value(serde_json::from_value::<ProxyConfig>(value).unwrap()).unwrap(), "rejection must not partially mutate config");
    }
    #[test]
    fn transformations_preserve_envelope_context_and_unrelated_arguments(args in json_value(), number in any::<i32>(), string_id in ".{0,16}", use_string in any::<bool>(), name in prop::sample::select(vec!["api/public","api/echo","api/other","outside/public"]), continuation in ".{0,16}") {
        let id = if use_string { json!(string_id) } else { json!(number) };
        let wire = json!({"id":id,"params":{"name":name,"arguments":args,"inputResponses":{},"requestState":continuation,"_meta":{"progressToken":"progress","extra":{"preserved":true}},"task":{"ttl":60000}}});
        prop_assert!(invariants::transformation(&serde_json::to_vec(&wire).unwrap()), "generated request must exercise the transforms");
    }
    #[test]
    fn bounded_parser_inputs_never_panic_and_roundtrip_when_accepted(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        invariants::parsing(&data);
    }
}

#[test]
fn checked_in_fuzz_seeds_exercise_the_same_assertions() {
    for (directory, invariant) in [
        ("config_parse", invariants::parsing as fn(&[u8]) -> bool),
        (
            "request_transform",
            invariants::transformation as fn(&[u8]) -> bool,
        ),
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz/seeds")
            .join(directory);
        let mut count = 0;
        let mut accepted = 0;
        for entry in std::fs::read_dir(path).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(bytes.len() <= invariants::MAX_INPUT);
            accepted += usize::from(invariant(&bytes));
            count += 1;
        }
        assert!(
            count >= 3,
            "each target needs valid and invalid seed inputs"
        );
        assert!(accepted > 0, "seed corpus must reach a valid invariant");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn coalescing_model_releases_cancelled_and_expired_generations(followers in 1usize..8, cancelled in any::<u8>(), finish in 0usize..3) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        runtime.block_on(async {
            use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
            use std::time::Duration;
            use futures_util::{poll, FutureExt};
            use tower::ServiceExt;
            use tower_mcp::{RouterRequest, RouterResponse, McpResponse};
            use tower_mcp::protocol::{McpRequest, RequestId};
            let calls = Arc::new(AtomicUsize::new(0));
            let (release, receiver) = tokio::sync::watch::channel(false);
            let captured = calls.clone();
            let inner = tower::service_fn(move |request: RouterRequest| {
                captured.fetch_add(1, Ordering::SeqCst);
                let mut release = receiver.clone();
                async move {
                    while !*release.borrow_and_update() { release.changed().await.unwrap(); }
                    Ok::<_, std::convert::Infallible>(RouterResponse { id:request.id, inner:Ok(McpResponse::CallTool(tower_mcp::CallToolResult::text("completed"))) })
                }
            });
            let config = mcp_proxy::config::CoalesceConfig { max_entries:1, max_waiters:followers, timeout_seconds:1 };
            let service = mcp_proxy::coalesce::CoalesceService::with_config(inner, &config).unwrap();
            let request = |id| RouterRequest { id:RequestId::Number(id), inner:McpRequest::CallTool(serde_json::from_value(json!({"name":"api/work","arguments":{"shared":true}})).unwrap()), extensions:Default::default() };
            let mut leader = Some(Box::pin(service.clone().oneshot(request(0))));
            assert!(poll!(leader.as_mut().unwrap().as_mut()).is_pending());
            let mut waiting = Vec::new();
            for index in 0..followers {
                let mut follower = Box::pin(service.clone().oneshot(request(index as i64 + 1)));
                assert!(poll!(follower.as_mut()).is_pending());
                waiting.push((index as i64 + 1, Some(follower)));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let mut freed = false;
            for (index, (_, future)) in waiting.iter_mut().enumerate() {
                if cancelled & (1 << index) != 0 { future.take(); freed = true; }
            }
            if freed {
                let mut replacement = Box::pin(service.clone().oneshot(request(100)));
                assert!(poll!(replacement.as_mut()).is_pending(), "cancelled follower must release its slot");
                waiting.push((100, Some(replacement)));
            }
            match finish {
                0 => { release.send_replace(true); },
                1 => { leader.take(); },
                _ => tokio::time::advance(Duration::from_secs(1)).await,
            }
            if let Some(leader) = leader {
                let response = leader.now_or_never().expect("leader must be ready after release/expiry").unwrap();
                assert_eq!(response.id, RequestId::Number(0));
                assert_eq!(response.inner.is_ok(), finish == 0);
            }
            for (id, future) in waiting {
                if let Some(future) = future {
                    let response = future.now_or_never().expect("followers must wake on completion/cancellation/expiry").unwrap();
                    assert_eq!(response.id, RequestId::Number(id));
                    assert_eq!(response.inner.is_ok(), finish == 0);
                }
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "cancellation/expiry must not replay");
            release.send_replace(true);
            let next = service.oneshot(request(999)).now_or_never().expect("fresh generation must complete without waiting").unwrap();
            assert_eq!(next.id, RequestId::Number(999));
            assert!(next.inner.is_ok(), "same key must elect a fresh generation");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        });
    }
}

#[cfg(feature = "oauth")]
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[cfg(feature = "oauth")]
#[allow(dead_code)]
#[path = "support/backend.rs"]
mod backend;
#[cfg(feature = "oauth")]
#[path = "support/http.rs"]
mod http;
#[cfg(feature = "oauth")]
#[path = "support/oauth.rs"]
mod oauth;

#[cfg(feature = "oauth")]
#[test]
fn configured_role_union_denies_and_filters_are_order_independent() {
    use proptest::test_runner::{Config, TestRunner};
    let mut runner = TestRunner::new(Config {
        cases: 16,
        source_file: Some(file!()),
        max_shrink_iters: 128,
        ..Config::default()
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let strategy = (
        proptest::collection::vec(0usize..4, 0..7),
        any::<bool>(),
        any::<bool>(),
        json_value(),
    );
    let verify = |(roles, hidden, search, value): (Vec<usize>, bool, bool, Value)| {
        runtime.block_on(async {
            let mut authority = oauth::Authority::new().await;
            let backend = backend::Backend::new("http").await;
            let exposure = if search && cfg!(feature = "discovery") {
                "search"
            } else {
                "direct"
            };
            let config = format!(
                r#"
[proxy]
name = "policy-properties"
tool_exposure = "{exposure}"
[proxy.listen]
[auth]
type = "jwt"
issuer = "{issuer}"
audience = "{resource}"
jwks_uri = "{keys}/keys"
[[auth.roles]]
name = "reader"
allow_tools = ["api/public"]
[[auth.roles]]
name = "broad"
[[auth.roles]]
name = "ban"
allow_tools = ["api/public"]
deny_tools = ["api/public"]
[auth.role_mapping]
claim = "scope"
default_deny = true
mapping = {{ reader="reader", broad="broad", ban="ban" }}
[security]
admin_token = "manager"
[[backends]]
name = "api"
{backend}
hide_tools = {hide}
[[backends.aliases]]
from = "echo"
to = "public"
[[backends.inject_args]]
tool = "echo"
args = {{ injected=true }}
overwrite = true
"#,
                issuer = authority.state.issuer,
                resource = oauth::RESOURCE,
                keys = authority.server.url,
                backend = backend.config,
                hide = if hidden { "['echo']" } else { "[]" }
            );
            let mut app = app::App::embedded(&config).await;
            let expected =
                !hidden && !roles.contains(&2) && (roles.contains(&0) || roles.contains(&1));
            let scopes = |indices: &[usize]| {
                indices
                    .iter()
                    .map(|&i| ["reader", "broad", "ban", "unmapped"][i])
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let mut reverse = roles.clone();
            reverse.reverse();
            for order in [&roles, &reverse] {
                let mut claims = authority.claims("generated-caller");
                claims["scope"] = json!(scopes(order));
                let token = authority.jwt(&claims);
                let before = backend.calls().len();
                if !roles.iter().any(|role| *role < 3) {
                    // default_deny applies before a session can be established.
                    let response = http::client()
                        .post(format!("{}/", app.url))
                        .bearer_auth(&token)
                        .header("accept", "application/json, text/event-stream")
                        .header("mcp-protocol-version", http::VERSION)
                        .json(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":http::initialization()}))
                        .send().await.unwrap();
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    assert!(response.headers().get("mcp-session-id").is_none());
                    let reply = http::payload(response).await;
                    assert_eq!(reply["id"], 0);
                    assert_eq!(reply["error"]["code"], -32602);
                    assert_eq!(backend.calls().len(), before);
                    continue;
                }
                let session = http::Session::start(&app.url, &token).await;
                let listed = session.rpc("tools/list", json!({})).await;
                let names = http::success(&listed)["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t["name"].as_str().unwrap())
                    .collect::<Vec<_>>();
                if exposure == "direct" {
                    assert_eq!(names.contains(&"api/public"), expected);
                } else {
                    assert!(!names.contains(&"api/public"));
                }
                assert_eq!(backend.calls().len(), before);
                let arguments = json!({"value":value,"injected":"caller"});
                let response = if exposure == "search" {
                    session
                        .call(
                            "proxy/call_tool",
                            json!({"name":"api/public","arguments":arguments}),
                        )
                        .await
                } else {
                    session.call("api/public", arguments).await
                };
                if expected {
                    let content = &http::success(&response)["content"];
                    assert_eq!(content.as_array().unwrap().len(), 1);
                    let expected = json!({"value":value,"injected":true});
                    assert_eq!(
                        serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap())
                            .unwrap(),
                        expected
                    );
                    assert_eq!(backend.calls().len(), before + 1);
                    assert_eq!(
                        backend.calls().last().unwrap(),
                        &json!({"name":"echo","arguments":expected})
                    );
                } else {
                    assert_eq!(response["error"]["code"], -32602);
                    assert_eq!(
                        backend.calls().len(),
                        before,
                        "denied target must never dispatch"
                    );
                }
                let management = http::client()
                    .get(format!("{}/admin/config", app.url))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    management.status(),
                    reqwest::StatusCode::UNAUTHORIZED,
                    "even an unrestricted role is ordinary authority"
                );
                session.terminate().await;
            }
            app.shutdown().await;
            authority.server.stop().await;
        });
        Ok(())
    };
    // Guarantee the important boundaries even in a short random run.
    for (roles, hidden, search) in [
        (vec![], false, false),
        (vec![3], false, true),
        (vec![0], false, false),
        (vec![1], false, true),
        (vec![0, 1, 2, 0], false, false),
        (vec![2, 1], false, true),
        (vec![0, 1], true, true),
        (vec![1, 0, 1], true, false),
    ] {
        verify((roles, hidden, search, json!({"fixed":"boundary"}))).unwrap();
    }
    runner.run(&strategy, verify).unwrap();
}
