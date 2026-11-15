//! Benchmarks for measuring proxy overhead.
//!
//! Measures per-request latency through the proxy with various middleware
//! configurations to quantify the cost of each layer, without HTTP.
//!
//! Every response is checked: a JSON-RPC error or an `is_error` tool result
//! panics instead of being timed, so a misconfigured layer cannot report
//! failed requests as fast ones. `cargo test --benches` runs each benchmark
//! once in test mode, which exercises these checks without timing.
//!
//! The stacked group composes layers in the order `build_middleware_stack`
//! in `src/proxy.rs` uses: capability filter outermost, then validation,
//! then the response cache, then the proxy.

use std::convert::Infallible;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tower::Service;
use tower::util::BoxCloneService;

use tower_mcp::client::ChannelTransport;
use tower_mcp::protocol::{CallToolParams, ListToolsParams, McpRequest, McpResponse, RequestId};
use tower_mcp::proxy::McpProxy;
use tower_mcp::router::{Extensions, RouterRequest, RouterResponse};
use tower_mcp::{CallToolResult, McpRouter, ToolBuilder};

use mcp_proxy::cache::{CacheHandle, CacheService};
use mcp_proxy::config::{BackendCacheConfig, BackendFilter, CacheBackendConfig, NameFilter};
use mcp_proxy::filter::CapabilityFilterService;
use mcp_proxy::validation::{ValidationConfig, ValidationService};

type Svc = BoxCloneService<RouterRequest, RouterResponse, Infallible>;

/// Catalog sizes for the `tools/list` scaling group.
const CATALOG_SIZES: [usize; 3] = [10, 100, 1000];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A proxy with one backend `bench` exposing `echo` plus `extra` filler tools
/// (`tool_0` .. `tool_{extra-1}`).
async fn build_proxy_with_tools(extra: usize) -> McpProxy {
    let echo = ToolBuilder::new("echo")
        .description("Echo input")
        .handler(
            |input: serde_json::Value| async move { Ok(CallToolResult::text(input.to_string())) },
        )
        .build();

    let mut router = McpRouter::new()
        .server_info("bench-server", "1.0.0")
        .tool(echo);
    for i in 0..extra {
        let tool = ToolBuilder::new(format!("tool_{i}"))
            .description("Catalog filler tool")
            .handler(|_: serde_json::Value| async move { Ok(CallToolResult::text("ok")) })
            .build();
        router = router.tool(tool);
    }

    McpProxy::builder("bench-proxy", "1.0.0")
        .separator("/")
        .backend("bench", ChannelTransport::new(router))
        .await
        .build_strict()
        .await
        .unwrap()
}

async fn build_proxy() -> McpProxy {
    build_proxy_with_tools(0).await
}

fn tool_call_with(arguments: serde_json::Value) -> McpRequest {
    McpRequest::CallTool(CallToolParams {
        name: "bench/echo".to_string(),
        arguments,
        input_responses: None,
        request_state: None,
        meta: None,
        task: None,
    })
}

fn tool_call() -> McpRequest {
    tool_call_with(serde_json::json!({"message": "hello"}))
}

/// A call whose arguments differ per `n`, so the response cache misses.
fn distinct_tool_call(n: u64) -> McpRequest {
    tool_call_with(serde_json::json!({"message": "hello", "n": n}))
}

fn list_tools() -> McpRequest {
    McpRequest::ListTools(ListToolsParams::default())
}

fn pass_all_filter() -> BackendFilter {
    BackendFilter {
        namespace: "bench/".to_string(),
        tool_filter: NameFilter::PassAll,
        resource_filter: NameFilter::PassAll,
        prompt_filter: NameFilter::PassAll,
        hide_destructive: false,
        read_only_only: false,
    }
}

fn cache_config() -> BackendCacheConfig {
    BackendCacheConfig {
        resource_ttl_seconds: 60,
        tool_ttl_seconds: 60,
        max_entries: 1000,
    }
}

fn validation_config() -> ValidationConfig {
    ValidationConfig {
        max_argument_size: Some(1_048_576),
    }
}

/// Clone-and-call for criterion's FnMut requirement. Panics on a JSON-RPC
/// error or an `is_error` tool result, so failures are never timed as
/// successes.
async fn call_ok(svc: &Svc, request: McpRequest) -> McpResponse {
    let mut clone = svc.clone();
    let req = RouterRequest {
        id: RequestId::Number(1),
        inner: request,
        extensions: Extensions::new(),
    };
    let response = match clone.call(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    match response.inner {
        Ok(McpResponse::CallTool(result)) if result.is_error => {
            panic!("benchmark tool call returned an error result: {result:?}")
        }
        Ok(response) => response,
        Err(error) => panic!("benchmark request failed: {error:?}"),
    }
}

/// Number of tools a `tools/list` response returned.
async fn listed_tools(svc: &Svc) -> usize {
    match call_ok(svc, list_tools()).await {
        McpResponse::ListTools(result) => result.tools.len(),
        other => panic!("expected a tools/list result, got {other:?}"),
    }
}

async fn cache_stats(handle: &CacheHandle) -> (u64, u64) {
    let stats = handle.stats().await;
    let bench = stats
        .iter()
        .find(|s| s.namespace == "bench/")
        .expect("bench namespace is cached");
    (bench.hits, bench.misses)
}

fn cached(proxy: McpProxy) -> (Svc, CacheHandle) {
    let cfg = cache_config();
    let (cached, handle) = CacheService::new(
        proxy,
        vec![("bench/".to_string(), &cfg)],
        &CacheBackendConfig::default(),
    );
    (BoxCloneService::new(cached), handle)
}

/// Production order: filter -> validation -> cache -> proxy.
fn stacked(proxy: McpProxy) -> (Svc, CacheHandle) {
    let cfg = cache_config();
    let (cached, handle) = CacheService::new(
        proxy,
        vec![("bench/".to_string(), &cfg)],
        &CacheBackendConfig::default(),
    );
    let validated = ValidationService::new(cached, validation_config());
    let filtered = CapabilityFilterService::new(validated, vec![pass_all_filter()]);
    (BoxCloneService::new(filtered), handle)
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_bare_proxy(c: &mut Criterion) {
    let rt = build_runtime();
    let svc: Svc = BoxCloneService::new(rt.block_on(build_proxy()));

    c.bench_function("bare_proxy/call_tool", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, tool_call()));
    });

    c.bench_function("bare_proxy/list_tools", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, list_tools()));
    });
}

fn bench_with_validation(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let svc: Svc = BoxCloneService::new(ValidationService::new(proxy, validation_config()));

    c.bench_function("validation/call_tool", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, tool_call()));
    });
}

fn bench_with_filter(c: &mut Criterion) {
    let rt = build_runtime();
    let proxy = rt.block_on(build_proxy());
    let svc: Svc =
        BoxCloneService::new(CapabilityFilterService::new(proxy, vec![pass_all_filter()]));

    c.bench_function("filter/call_tool", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, tool_call()));
    });

    c.bench_function("filter/list_tools", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, list_tools()));
    });
}

fn bench_with_cache(c: &mut Criterion) {
    let rt = build_runtime();

    // Hits: one priming call, then identical requests.
    let (svc, handle) = cached(rt.block_on(build_proxy()));
    rt.block_on(call_ok(&svc, tool_call()));
    c.bench_function("cache/call_tool_hit", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, tool_call()));
    });
    let (hits, misses) = rt.block_on(cache_stats(&handle));
    assert_eq!(misses, 1, "only the priming call may miss");
    assert!(hits > 0, "hit benchmark recorded no hits");

    // Misses: distinct arguments on every call, each forwarded and stored.
    let (svc, handle) = cached(rt.block_on(build_proxy()));
    let mut n = 0u64;
    c.bench_function("cache/call_tool_miss", |b| {
        b.to_async(&rt).iter(|| {
            n += 1;
            call_ok(&svc, distinct_tool_call(n))
        });
    });
    let (hits, misses) = rt.block_on(cache_stats(&handle));
    assert_eq!(hits, 0, "miss benchmark must never hit");
    assert!(misses > 0, "miss benchmark recorded no misses");
}

fn bench_stacked_middleware(c: &mut Criterion) {
    let rt = build_runtime();
    let (svc, handle) = stacked(rt.block_on(build_proxy()));

    // Every request still traverses filter and validation; only the
    // innermost cache short-circuits the backend call.
    rt.block_on(call_ok(&svc, tool_call()));
    c.bench_function("stacked/call_tool_hit", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, tool_call()));
    });
    let (hits, _) = rt.block_on(cache_stats(&handle));
    assert!(hits > 0, "stacked hit benchmark recorded no hits");

    let mut n = 0u64;
    c.bench_function("stacked/call_tool_miss", |b| {
        b.to_async(&rt).iter(|| {
            n += 1;
            call_ok(&svc, distinct_tool_call(n))
        });
    });

    c.bench_function("stacked/list_tools", |b| {
        b.to_async(&rt).iter(|| call_ok(&svc, list_tools()));
    });
}

fn bench_catalog_scaling(c: &mut Criterion) {
    let rt = build_runtime();
    let mut group = c.benchmark_group("catalog/list_tools");

    for size in CATALOG_SIZES {
        // `size` tools in total: echo plus filler.
        let bare: Svc = BoxCloneService::new(rt.block_on(build_proxy_with_tools(size - 1)));
        assert_eq!(rt.block_on(listed_tools(&bare)), size);
        group.bench_with_input(BenchmarkId::new("bare", size), &size, |b, _| {
            b.to_async(&rt).iter(|| call_ok(&bare, list_tools()));
        });

        // An allow list keeping echo and the even-numbered filler tools, so
        // the filter evaluates patterns against every listed name.
        let proxy = rt.block_on(build_proxy_with_tools(size - 1));
        let filter = BackendFilter {
            tool_filter: NameFilter::allow_list([
                "echo".to_string(),
                "re:^tool_[0-9]*[02468]$".to_string(),
            ])
            .unwrap(),
            ..pass_all_filter()
        };
        let filtered: Svc = BoxCloneService::new(CapabilityFilterService::new(proxy, vec![filter]));
        assert_eq!(
            rt.block_on(listed_tools(&filtered)),
            1 + (size - 1).div_ceil(2)
        );
        group.bench_with_input(BenchmarkId::new("allow_list", size), &size, |b, _| {
            b.to_async(&rt).iter(|| call_ok(&filtered, list_tools()));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_bare_proxy,
    bench_with_validation,
    bench_with_filter,
    bench_with_cache,
    bench_stacked_middleware,
    bench_catalog_scaling,
);
criterion_main!(benches);
