# Benchmarks

**Scope:** the HTTP baseline below was measured on 0.7.0 on one developer
machine. Numbers are scale, not guarantees. Representative workloads beyond these
harnesses (Redis and SQLite caches, large payloads, slow peers, memory) are
tracked in #296.

Two harnesses answer two different questions. Neither runs in CI; both are
reproducible locally.

## End-to-end HTTP overhead

**Question:** what does putting mcp-proxy in front of a backend cost, over
real HTTP, with protocol-correct MCP clients?

```bash
cargo run --release --example overhead
```

[`examples/overhead.rs`](../examples/overhead.rs) starts a tower-mcp HTTP
backend (an echo tool) on an ephemeral loopback port and mcp-proxy in front
of it twice: a minimal config with no middleware, and a loaded config with
timeout, retry, circuit breaker, response cache, and request coalescing. It
then drives three scenarios (direct to the backend, through the minimal
proxy, through the loaded proxy) with `McpClient` workers at 1, 8, and 32
concurrent sessions, 2000 requests per cell after warmup.

Methodology notes:

- Every request carries distinct arguments, so the cache and coalescer on
  the loaded proxy cannot short-circuit the measurement; the loaded numbers
  pay the full middleware traversal cost.
- Workers establish and settle their sessions before a barrier; timing
  starts after it, so throughput measures steady state rather than
  connection storms.
- Latency percentiles come from per-request wall clock on the client side.

### Baseline

Apple M4 Pro (14 cores, 24 GB), macOS 27.2.1, rustc 1.100.0, release profile,
loopback HTTP. mcp-proxy 0.7.0 with tower-mcp 0.26.0, 2026-11-14. The machine
was not idle (load average 8 to 9). A second run matched throughput within 10%
on every row; tail latencies varied more.

```text
scenario         conc      req/s        p50        p95        p99   resets
direct              1      17123    46.83us   105.12us   177.33us        0
direct              8      53421   136.25us   182.42us   196.71us        0
direct             32      38063   636.88us   685.29us   703.38us        0
proxied-minimal     1       9189   106.29us   122.79us   136.54us        0
proxied-minimal     8      30203   240.29us   293.12us   323.33us        0
proxied-minimal    32      31375   762.75us   855.79us   893.00us        0
proxied-loaded      1       8984   107.38us   127.29us   148.42us        0
proxied-loaded      8      28141   254.17us   306.12us   334.12us        0
proxied-loaded     32      25370   951.17us     1.07ms     1.12ms        0
```

Compare rows within one run only. Throughput at 32 sessions is lower than in
the 0.4.1 baseline for the `direct` row too, which has no proxy in the path, so
that difference comes from the backend transport and machine load rather than
from proxy overhead. The 0.4.1 baseline (2026-08-04, macOS 26.6, rustc 1.97.1)
is in this file's history.

How to read it:

- The proxy adds roughly 60us to p50 at one session: one additional HTTP
  hop, session lookup, and namespace routing. Against a loopback echo
  backend that roughly halves throughput; against any real backend doing
  real work, that difference disappears into the noise. This echo setup is
  the worst case for relative overhead by construction.
- The full resilience and caching stack costs about 1us p50 over the
  minimal proxy at one session and more under contention (compare
  proxied-loaded to proxied-minimal at 32 sessions).
- The `resets` column counts sessions that had to reconnect before becoming
  usable. The 0.4.1 baseline showed resets at 32 sessions from a tower-mcp
  HTTP transport issue (joshrotenberg/tower-mcp#1174); this run had none.

Numbers vary run to run by a few percent; treat them as scale, not truth.

## Per-middleware overhead (criterion)

**Question:** what does each middleware layer cost in isolation, without
HTTP?

```bash
cargo bench
```

[`benches/proxy_overhead.rs`](../benches/proxy_overhead.rs) measures
per-request latency through in-process `ChannelTransport` backends, isolating
layer cost from transport cost. Criterion writes reports to
`target/criterion/`.

| Group | Measures |
|---|---|
| `bare_proxy` | `tools/call` and `tools/list` through the proxy with no middleware |
| `validation`, `filter` | One layer each over the bare proxy |
| `cache` | `call_tool_hit` repeats one request after priming; `call_tool_miss` sends distinct arguments every iteration, so each call is forwarded and stored |
| `stacked` | Filter, validation and cache composed in the order `build_middleware_stack` in `src/proxy.rs` uses (filter outermost, cache innermost), for hits, misses and `tools/list`; every request traverses filter and validation |
| `catalog/list_tools` | `tools/list` at 10, 100 and 1000 tools, bare and through an allow-list filter that keeps about half |

Every response is checked: a JSON-RPC error or an `is_error` tool result
panics instead of being timed, and the cache groups assert their hit and miss
counts afterwards. CI runs each benchmark once in test mode, without timing:

```bash
cargo test --all-features --bench proxy_overhead
```

There are no regression budgets yet; run-to-run variance has to be measured
first.

## Ground rules

Performance claims in the README or comparison docs must cite one of these
harnesses and be reproducible with a single command. No cross-proxy
comparative numbers: environments and feature sets differ too much for a
one-line comparison to inform anyone.
