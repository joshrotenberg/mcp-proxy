# Benchmarks

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

Apple M4 Pro (14 cores, 24 GB), macOS 26.6, rustc 1.97.1, release profile,
loopback HTTP. mcp-proxy v0.4.1 working tree, 2026-08-04:

```text
scenario         conc      req/s        p50        p95        p99   resets
direct              1      17596    50.12us    89.21us   113.54us        0
direct              8      63924   112.58us   143.12us   162.67us        0
direct             32      86951   266.12us   397.17us   482.00us        0
proxied-minimal     1       9551   102.04us   116.46us   137.46us        0
proxied-minimal     8      33489   218.33us   260.42us   281.17us        0
proxied-minimal    32      35113   601.46us   736.79us   808.96us        1
proxied-loaded      1       9153   106.88us   122.46us   149.29us        0
proxied-loaded      8      33113   217.25us   265.29us   284.00us        0
proxied-loaded     32      31518   616.08us   778.79us   822.00us        2
```

How to read it:

- The proxy adds roughly 50us to p50 at low concurrency: one additional
  HTTP hop, session lookup, and namespace routing. Against a loopback echo
  backend that roughly halves throughput; against any real backend doing
  real work, 50us disappears into the noise. This echo setup is the worst
  case for relative overhead by construction.
- The full resilience and caching stack costs about 5us p50 over the
  minimal proxy (compare proxied-loaded to proxied-minimal). The middleware
  chain is cheap relative to the hop itself.
- The `resets` column counts sessions that had to reconnect before
  becoming usable. Nonzero values at 32 concurrent sessions reflect a known
  tower-mcp HTTP transport issue where the `notifications/initialized`
  notification can be lost under concurrent session creation
  (joshrotenberg/tower-mcp#1174); the harness reconnects the way a real
  client would and reports the count.

Numbers vary run to run by a few percent; treat them as scale, not truth.

## Per-middleware overhead (criterion)

**Question:** what does each middleware layer cost in isolation, without
HTTP?

```bash
cargo bench
```

[`benches/proxy_overhead.rs`](../benches/proxy_overhead.rs) measures
per-request latency through in-process `ChannelTransport` backends with
individual middleware configurations (cache, filter, validation), isolating
layer cost from transport cost. Criterion writes reports to
`target/criterion/`.

## Ground rules

Performance claims in the README or comparison docs must cite one of these
harnesses and be reproducible with a single command. No cross-proxy
comparative numbers: environments and feature sets differ too much for a
one-line comparison to inform anyone.
