//! Generated contracts exercised through the configured application's real HTTP API.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[allow(dead_code)]
#[path = "support/backend.rs"]
mod backend;
#[path = "support/http.rs"]
mod http;

async fn fixture(executable: bool) -> (app::App, backend::Backend, String) {
    let backend = backend::Backend::new("http").await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let config = format!(
        "[proxy]\nname='openapi-contract'\n[proxy.listen]\nhost='127.0.0.1'\nport={port}\n[auth]\ntype='bearer'\ntokens=['ordinary-token']\n[security]\nadmin_token='management-token'\n[[backends]]\nname='api'\n{}\n",
        backend.config
    );
    drop(reservation);
    let app = if executable {
        app::App::cli(&config, port).await
    } else {
        app::App::embedded(&config).await
    };
    (app, backend, config)
}

#[tokio::test]
async fn spec_is_private_and_respects_the_openapi_feature_boundary() {
    let (mut app, _backend, config) = fixture(false).await;
    let protected_path = if cfg!(feature = "openapi") {
        "/admin/openapi.json"
    } else {
        "/admin/health"
    };
    for token in [None, Some("ordinary-token")] {
        let mut request = http::client().get(format!("{}{protected_path}", app.url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let reply = request.send().await.unwrap();
        assert_eq!(reply.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            reply.json::<serde_json::Value>().await.unwrap()["id"],
            serde_json::Value::Null
        );
    }
    let reply = http::client()
        .get(format!("{}/admin/openapi.json", app.url))
        .bearer_auth("management-token")
        .send()
        .await
        .unwrap();
    if cfg!(feature = "openapi") {
        assert_eq!(reply.status(), reqwest::StatusCode::OK);
        let text = reply.text().await.unwrap();
        assert!(!text.contains("management-token") && !text.contains("ordinary-token"));
        let spec: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
    } else {
        assert_eq!(reply.status(), reqwest::StatusCode::NOT_FOUND);
    }
    app.shutdown().await;
    let mut disabled =
        app::App::embedded(&config.replace("admin_token='management-token'", "")).await;
    let path = if cfg!(feature = "openapi") {
        "/admin/openapi.json"
    } else {
        "/admin/health"
    };
    let reply = http::client()
        .get(format!("{}{path}", disabled.url))
        .bearer_auth("management-token")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), reqwest::StatusCode::UNAUTHORIZED);
    disabled.shutdown().await;
}

#[cfg(feature = "openapi")]
mod contracts {
    use super::*;
    use serde_json::{Value, json};

    // Check serialized response shapes against generated local schemas. This is
    // deliberately a shape checker, not a general JSON Schema validator.
    fn shape(value: &Value, schema: &Value, root: &Value) -> bool {
        if let Some(reference) = schema.get("$ref") {
            return shape(
                value,
                root.pointer(reference.as_str().unwrap().strip_prefix('#').unwrap())
                    .unwrap(),
                root,
            );
        }
        if schema == &Value::Bool(true) {
            return true;
        }
        if schema == &Value::Bool(false) {
            return false;
        }
        if let Some(types) = schema.get("type") {
            let matches = |ty: &str| match ty {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "null" => value.is_null(),
                other => panic!("unsupported schema type {other}"),
            };
            let valid = if let Some(ty) = types.as_str() {
                matches(ty)
            } else {
                types
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|ty| matches(ty.as_str().unwrap()))
            };
            if !valid {
                return false;
            }
        }
        if let Some(values) = schema.get("enum")
            && !values.as_array().unwrap().contains(value)
        {
            return false;
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(options) = schema.get(keyword) {
                let count = options
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|option| shape(value, option, root))
                    .count();
                if (keyword == "allOf" && count != options.as_array().unwrap().len())
                    || (keyword == "anyOf" && count == 0)
                    || (keyword == "oneOf" && count != 1)
                {
                    return false;
                }
            }
        }
        if let Some(object) = value.as_object() {
            if let Some(required) = schema.get("required")
                && required
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|key| !object.contains_key(key.as_str().unwrap()))
            {
                return false;
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, value) in object {
                    if let Some(property) = properties.get(key) {
                        if !shape(value, property, root) {
                            return false;
                        }
                    } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                        return false;
                    }
                }
            }
        }
        if let Some(array) = value.as_array()
            && let Some(items) = schema.get("items")
        {
            return array.iter().all(|value| shape(value, items, root));
        }
        true
    }

    async fn checked(
        spec: &Value,
        path: &str,
        method: &str,
        response: reqwest::Response,
        status: u16,
    ) -> Value {
        assert_eq!(response.status().as_u16(), status, "{method} {path}");
        let contract = &spec["paths"][path][method]["responses"][status.to_string()];
        assert!(!contract.is_null(), "undocumented {method} {path} {status}");
        if contract.get("content").is_none() {
            assert!(response.bytes().await.unwrap().is_empty());
            return Value::Null;
        }
        let media = response.headers()["content-type"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let schema = &contract["content"][&media]["schema"];
        assert!(
            !schema.is_null(),
            "undocumented media {media} for {method} {path} {status}"
        );
        let text = response.text().await.unwrap();
        let value = if media == "application/json" {
            serde_json::from_str(&text).unwrap()
        } else {
            Value::String(text)
        };
        assert!(
            shape(&value, schema, spec),
            "schema mismatch {method} {path} {status}: {value} vs {schema}"
        );
        value
    }

    #[tokio::test]
    async fn configured_operations_match_documented_media_status_and_schema_shapes() {
        let (mut app, backend, config) = fixture(false).await;
        let client = http::client();
        let spec: Value = client
            .get(format!("{}/admin/openapi.json", app.url))
            .bearer_auth("management-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let _: utoipa::openapi::OpenApi = serde_json::from_value(spec.clone()).unwrap();
        let mut operations = 0;
        for (path, methods) in spec["paths"].as_object().unwrap() {
            for method in methods.as_object().unwrap().keys() {
                operations += 1;
                let url = format!(
                    "{}{}",
                    app.url,
                    path.replace("{name}", "api").replace("{id}", "missing")
                );
                for token in [None, Some("ordinary-token")] {
                    let mut request = client.request(
                        method
                            .to_ascii_uppercase()
                            .parse::<reqwest::Method>()
                            .unwrap(),
                        &url,
                    );
                    if let Some(token) = token {
                        request = request.bearer_auth(token);
                    }
                    checked(&spec, path, method, request.send().await.unwrap(), 401).await;
                }
            }
        }
        assert_eq!(operations, 20);
        for path in [
            "/admin/openapi.json",
            "/admin/backends",
            "/admin/health",
            "/admin/cache/stats",
            "/admin/metrics",
            "/admin/sessions",
            "/admin/sessions/detail",
            "/admin/stats",
            "/admin/circuit-breakers",
            "/admin/config",
            "/admin/config/status",
            "/admin/backends/api/health/history",
        ] {
            let documented = path.replace("/api/", "/{name}/");
            let value = checked(
                &spec,
                &documented,
                "get",
                client
                    .get(format!("{}{path}", app.url))
                    .bearer_auth("management-token")
                    .send()
                    .await
                    .unwrap(),
                200,
            )
            .await;
            if path == "/admin/config" {
                assert!(!value.as_str().unwrap().contains("management-token"));
            }
        }
        checked(
            &spec,
            "/admin/cache/clear",
            "post",
            client
                .post(format!("{}/admin/cache/clear", app.url))
                .bearer_auth("management-token")
                .send()
                .await
                .unwrap(),
            200,
        )
        .await;
        for (method, path, body, status) in [
            ("post", "/admin/config/validate", config.clone(), 200),
            ("post", "/admin/config/validate", "invalid".into(), 400),
            (
                "post",
                "/admin/config/validate",
                config.replace("openapi-contract", "changed-policy"),
                409,
            ),
            ("put", "/admin/config", config.clone(), 400),
            (
                "put",
                "/admin/config",
                config.replace("openapi-contract", "changed-policy"),
                409,
            ),
        ] {
            checked(
                &spec,
                path,
                method,
                client
                    .request(
                        method
                            .to_ascii_uppercase()
                            .parse::<reqwest::Method>()
                            .unwrap(),
                        format!("{}{path}", app.url),
                    )
                    .bearer_auth("management-token")
                    .header("content-type", "text/plain")
                    .body(body)
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        // JSON extractor rejections have text bodies, unlike handler errors.
        for (body, status) in [(vec![0xff], 400), (vec![b' '; 2 * 1024 * 1024 + 1], 413)] {
            checked(
                &spec,
                "/admin/config/validate",
                "post",
                client
                    .post(format!("{}/admin/config/validate", app.url))
                    .bearer_auth("management-token")
                    .body(body)
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        for (content_type, body, status) in [
            ("application/json", "{", 400),
            ("application/json", "{}", 422),
            ("text/plain", "{}", 415),
        ] {
            checked(
                &spec,
                "/admin/backends/add",
                "post",
                client
                    .post(format!("{}/admin/backends/add", app.url))
                    .bearer_auth("management-token")
                    .header("content-type", content_type)
                    .body(body)
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        let peer: toml::Value = toml::from_str(&backend.config).unwrap();
        for (method, path, documented, body, status) in [
            (
                "post",
                "/admin/backends/add",
                "/admin/backends/add",
                json!({"name":"extra","url":peer["url"].as_str().unwrap()}),
                201,
            ),
            (
                "post",
                "/admin/backends/add",
                "/admin/backends/add",
                json!({"name":"extra","url":peer["url"].as_str().unwrap()}),
                409,
            ),
            (
                "put",
                "/admin/backends/extra",
                "/admin/backends/{name}",
                json!({"url":"file:///invalid"}),
                400,
            ),
            (
                "put",
                "/admin/backends/extra",
                "/admin/backends/{name}",
                json!({"url":peer["url"].as_str().unwrap()}),
                200,
            ),
            (
                "put",
                "/admin/backends/missing",
                "/admin/backends/{name}",
                json!({"url":peer["url"].as_str().unwrap()}),
                404,
            ),
        ] {
            checked(
                &spec,
                documented,
                method,
                client
                    .request(
                        method
                            .to_ascii_uppercase()
                            .parse::<reqwest::Method>()
                            .unwrap(),
                        format!("{}{path}", app.url),
                    )
                    .bearer_auth("management-token")
                    .json(&body)
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        for (name, status) in [("extra", 200), ("missing", 404)] {
            checked(
                &spec,
                "/admin/backends/{name}",
                "delete",
                client
                    .delete(format!("{}/admin/backends/{name}", app.url))
                    .bearer_auth("management-token")
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        for name in ["api", "missing"] {
            let response = client
                .get(format!("{}/admin/backends/{name}/health", app.url))
                .bearer_auth("management-token")
                .send()
                .await
                .unwrap();
            let status = if name == "missing" {
                404
            } else {
                response.status().as_u16()
            };
            assert!([200, 404].contains(&status)); // The first health snapshot is asynchronous.
            checked(
                &spec,
                "/admin/backends/{name}/health",
                "get",
                response,
                status,
            )
            .await;
        }
        let session = http::Session::start(&app.url, "ordinary-token").await;
        for path in ["/admin/sessions", "/admin/sessions/detail"] {
            checked(
                &spec,
                path,
                "get",
                client
                    .get(format!("{}{path}", app.url))
                    .bearer_auth("management-token")
                    .send()
                    .await
                    .unwrap(),
                200,
            )
            .await;
        }
        for status in [200, 404] {
            checked(
                &spec,
                "/admin/sessions/{id}",
                "delete",
                client
                    .delete(format!("{}/admin/sessions/{}", app.url, session.id))
                    .bearer_auth("management-token")
                    .send()
                    .await
                    .unwrap(),
                status,
            )
            .await;
        }
        assert!(
            backend.calls().is_empty(),
            "admin tests must not execute data-plane tools"
        );
        app.shutdown().await;
    }

    #[tokio::test]
    async fn config_save_response_describes_saved_rather_than_applied() {
        let (mut app, _backend, config) = fixture(true).await;
        let client = http::client();
        let spec: Value = client
            .get(format!("{}/admin/openapi.json", app.url))
            .bearer_auth("management-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let value = checked(
            &spec,
            "/admin/config",
            "put",
            client
                .put(format!("{}/admin/config", app.url))
                .bearer_auth("management-token")
                .body(config)
                .send()
                .await
                .unwrap(),
            200,
        )
        .await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["applied"], false);
        assert_eq!(value["restart_required"], true);
        app.shutdown().await;
    }
}
