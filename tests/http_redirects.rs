//! Verify upgraded HTTP redirect policy through configured backend connections.
use axum::{
    Router, extract::Request, http::HeaderMap, middleware::Next, response::Redirect, routing::any,
};
use std::sync::{Arc, Mutex};

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), task)
}

fn config(url: &str) -> mcp_proxy::ProxyConfig {
    mcp_proxy::ProxyConfig::parse(&format!(
        r#"
        [proxy]
        name = "redirect-test"
        [proxy.listen]
        [[backends]]
        name = "api"
        transport = "http"
        url = "{url}"
        headers = {{ "X-API-Key" = "private-key" }}
    "#
    ))
    .unwrap()
}

fn backend(captured: Arc<Mutex<Vec<HeaderMap>>>, path: &str) -> Router {
    let transport = tower_mcp::transport::http::HttpTransport::new(tower_mcp::McpRouter::new());
    let router = if path == "/" {
        transport.into_router()
    } else {
        transport.into_router_at(path)
    };
    router.layer(axum::middleware::from_fn(
        move |req: Request, next: Next| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(req.headers().clone());
                next.run(req).await
            }
        },
    ))
}

#[tokio::test]
async fn cross_origin_redirect_never_receives_backend_credentials() {
    let target_headers = Arc::new(Mutex::new(Vec::new()));
    let (target, target_task) = serve(backend(target_headers.clone(), "/")).await;
    let source_headers = Arc::new(Mutex::new(Vec::new()));
    let captured = source_headers.clone();
    let source = Router::new().fallback(any(move |req: Request| {
        let captured = captured.clone();
        let target = format!("{target}/");
        async move {
            captured.lock().unwrap().push(req.headers().clone());
            Redirect::temporary(&target)
        }
    }));
    let (source, source_task) = serve(source).await;
    assert!(
        mcp_proxy::Proxy::from_config(config(&source))
            .await
            .is_err()
    );
    let headers = source_headers.lock().unwrap();
    assert!(!headers.is_empty(), "initialization must reach the source");
    assert_eq!(headers[0]["x-api-key"], "private-key");
    assert!(
        target_headers.lock().unwrap().is_empty(),
        "redirect target must receive no request"
    );
    source_task.abort();
    target_task.abort();
}

#[tokio::test]
async fn same_origin_redirect_preserves_configured_headers() {
    let headers = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .merge(backend(headers.clone(), "/backend"))
        .route("/", any(|| async { Redirect::temporary("/backend") }));
    let (url, task) = serve(router).await;
    let _proxy = mcp_proxy::Proxy::from_config(config(&url)).await.unwrap();
    let captured = headers.lock().unwrap();
    assert!(!captured.is_empty());
    for request in captured.iter() {
        assert_eq!(request["x-api-key"], "private-key");
    }
    task.abort();
}
