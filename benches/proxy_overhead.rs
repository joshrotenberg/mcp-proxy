//! Benchmarks for measuring proxy overhead.
//!
//! Measures per-request latency through the proxy with various middleware
//! configurations to quantify the cost of each layer.

use std::convert::Infallible;

use criterion::{Criterion, criterion_group, criterion_main};
use tower::Service;
use tower::util::BoxCloneService;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, ListToolsParams, McpRequest, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_proxy::cache::CacheService;
use mcp_proxy::config::{BackendCacheConfig, BackendFilter, CacheBackendConfig, NameFilter};
use mcp_proxy::filter::CapabilityFilterService;
use mcp_proxy::validation::{ValidationConfig, ValidationService};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn build_proxy() -> McpProxy {
    let tool = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(
            |input: serde_json::Value| async move { Ok(CallToolResult::text(input.to_string())) },
        )
        .build();

    let router = McpRouter::new()
        .server_info("bench-server", "1.0.0")
        .tool(tool);

    McpProxy::builder("bench-proxy", "1.0.0")
        .separator("/")
        .backend("bench", ChannelTransport::new(router))
        .await
        .build_strict()
        .await
        .unwrap()
}

fn tool_call() -> McpRequest {
    McpRequest::CallTool(CallToolParams {
        name: "bench/echo".to_string(),
        arguments: serde_json::json!({"message": "hello"}),
        meta: None,
        task: None,
    })
}

fn list_tools() -> McpRequest {
    McpRequest::ListTools(ListToolsParams::default())
}

/// Clone-and-call for criterion's FnMut requirement.
async fn bench_call(
    svc: &BoxCloneService<RouterRequest, RouterResponse, Infallible>,
    request: McpRequest,
) {
    let mut clone = svc.clone();
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: request,
        extensions: Extensions::new(),
    };
    let _ = clone.call(req).await;
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_bare_proxy(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let svc = BoxCloneService::new(proxy);

    c.bench_function("bare_proxy/call_tool", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, tool_call()));
    });

    c.bench_function("bare_proxy/list_tools", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, list_tools()));
    });
}

fn bench_with_validation(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let validated = ValidationService::new(
        proxy,
        ValidationConfig {
            max_argument_size: Some(1_048_576),
        },
    );
    let svc = BoxCloneService::new(validated);

    c.bench_function("validation/call_tool", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, tool_call()));
    });
}

fn bench_with_filter(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let filter = BackendFilter {
        namespace: "bench/".to_string(),
        tool_filter: NameFilter::PassAll,
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    };
    let filtered = CapabilityFilterService::new(proxy, vec![filter]);
    let svc = BoxCloneService::new(filtered);

    c.bench_function("filter/call_tool", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, tool_call()));
    });

    c.bench_function("filter/list_tools", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, list_tools()));
    });
}

fn bench_with_cache(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 1000,
    };
    let (cached, _handle) = CacheService::new(
        proxy,
        vec![("bench/".to_string(), &cfg)],
        &CacheBackendConfig::default(),
    );
    let svc = BoxCloneService::new(cached);

    // Prime the cache
    rt.block_on(bench_call(&svc, tool_call()));

    c.bench_function("cache_hit/call_tool", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, tool_call()));
    });
}

fn bench_stacked_middleware(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());

    // Stack: validation -> filter -> cache (similar to production)
    let validated = ValidationService::new(
        proxy,
        ValidationConfig {
            max_argument_size: Some(1_048_576),
        },
    );
    let filter = BackendFilter {
        namespace: "bench/".to_string(),
        tool_filter: NameFilter::PassAll,
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    };
    let filtered = CapabilityFilterService::new(validated, vec![filter]);
    let cfg = BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 1000,
    };
    let (cached, _handle) = CacheService::new(
        filtered,
        vec![("bench/".to_string(), &cfg)],
        &CacheBackendConfig::default(),
    );
    let svc = BoxCloneService::new(cached);

    // Prime cache
    rt.block_on(bench_call(&svc, tool_call()));

    c.bench_function("stacked/call_tool_cached", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, tool_call()));
    });

    c.bench_function("stacked/list_tools", |b| {
        b.to_async(&rt).iter(|| bench_call(&svc, list_tools()));
    });
}

criterion_group!(
    benches,
    bench_bare_proxy,
    bench_with_validation,
    bench_with_filter,
    bench_with_cache,
    bench_stacked_middleware,
);
criterion_main!(benches);
