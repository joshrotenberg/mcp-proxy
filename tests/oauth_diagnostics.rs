//! OAuth endpoint failures must not disclose configured credentials.
#![cfg(feature = "oauth")]

use std::sync::{Arc, Mutex};

use mcp_proxy::introspection::{IntrospectionValidator, discover_auth_server};
use tower_mcp::oauth::TokenValidator;

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

// Avoid Debug formatting the success value: it may contain credentials.
fn expect_failure<T, E>(result: Result<T, E>) -> E {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected failure; sensitive success value omitted"),
    }
}

fn assert_private(text: &str) {
    for (index, secret) in [
        "sentinel-oauth-user",
        "sentinel-oauth-password",
        "sentinel-oauth-path",
        "sentinel-oauth-query",
        "sentinel-oauth-body",
        "sentinel-client-id",
        "sentinel-client-secret",
        "sentinel-access-token",
        "sentinel-issuer",
        "sentinel-audience",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            !text.contains(secret),
            "OAuth diagnostic exposed sentinel {index}"
        );
    }
}

#[tokio::test]
async fn oauth_startup_discovery_and_introspection_diagnostics_are_private() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Switch between successful discovery, OIDC fallback, reflected invalid
    // metadata, and status errors without relying on external networks.
    let mode = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = mode.clone();
    let router = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let state = state.clone();
        async move {
            use axum::response::IntoResponse;
            let mode = state.load(std::sync::atomic::Ordering::SeqCst);
            if mode == 3 || (mode == 1 && uri.path().contains("oauth-authorization-server")) {
                return axum::http::StatusCode::NOT_FOUND.into_response();
            }
            if mode == 2 {
                return "sentinel-oauth-body".into_response();
            }
            axum::Json(serde_json::json!({
                "issuer": "https://sentinel-issuer.example",
                "jwks_uri": "https://example.test/sentinel-oauth-path",
                "introspection_endpoint": "https://example.test/?key=sentinel-oauth-query"
            }))
            .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let endpoint = format!(
        "http://sentinel-oauth-user:sentinel-oauth-password@{addr}/sentinel-oauth-path?key=sentinel-oauth-query"
    );
    // Discovery appends a well-known path, so keep its issuer free of query
    // parameters; introspection below exercises query-bearing endpoints.
    let issuer =
        format!("http://sentinel-oauth-user:sentinel-oauth-password@{addr}/sentinel-oauth-path");
    for value in 0..4 {
        mode.store(value, std::sync::atomic::Ordering::SeqCst);
        let result = discover_auth_server(&issuer).await;
        if value < 2 {
            assert!(result.is_ok());
        } else {
            let error = expect_failure(result);
            assert_private(&format!("{error:#}"));
            assert_private(&format!("{error:?}"));
            assert!(
                error
                    .to_string()
                    .contains(if value == 2 { "parse" } else { "404" })
            );
        }
    }
    let error = expect_failure(discover_auth_server("http://[sentinel-oauth-query").await);
    assert_private(&format!("{error:?}"));

    for endpoint in [&endpoint, "http://[sentinel-oauth-query"] {
        mode.store(2, std::sync::atomic::Ordering::SeqCst);
        let validator =
            IntrospectionValidator::new(endpoint, "sentinel-client-id", "sentinel-client-secret");
        let error = expect_failure(validator.validate_token("sentinel-access-token").await);
        assert_private(&error.to_string());
        assert_private(&format!("{error:?}"));
    }

    // Exercise the actual startup paths, not only the discovery helper.
    let backend = tower_mcp::HttpTransport::new(tower_mcp::McpRouter::new()).into_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = listener.local_addr().unwrap();
    let backend_task = tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });
    mode.store(0, std::sync::atomic::Ordering::SeqCst);
    for (auth_type, strategy) in [("jwt", "jwt"), ("oauth", "jwt"), ("oauth", "both")] {
        let config = mcp_proxy::ProxyConfig::parse(&format!(
            r#"
            [proxy]
            name = "oauth-diagnostics"
            [proxy.listen]
            [[backends]]
            name = "fixture"
            transport = "http"
            url = "http://{backend_addr}/"
            [security]
            admin_token = "sentinel-client-secret"
            [auth]
            type = "{auth_type}"
            issuer = "{issuer}"
            audience = "sentinel-audience"
            jwks_uri = "{endpoint}"
            token_validation = "{strategy}"
            client_id = "sentinel-client-id"
            client_secret = "sentinel-client-secret"
            introspection_endpoint = "{endpoint}"
        "#
        ))
        .unwrap();
        let error = expect_failure(mcp_proxy::Proxy::from_config(config).await);
        assert_private(&format!("{error:#}"));
        assert_private(&format!("{error:?}"));
    }
    let captured = logs.0.lock().unwrap();
    let captured = String::from_utf8_lossy(&captured);
    assert!(captured.contains("Enabling JWT auth"));
    assert!(captured.contains("Enabling OAuth 2.1 auth"));
    assert!(captured.contains("Discovered auth server metadata (RFC 8414)"));
    assert!(captured.contains("Discovered auth server metadata (OIDC)"));
    assert_private(&captured);
    task.abort();
    backend_task.abort();
}
