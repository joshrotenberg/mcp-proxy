# Production gateway showcase

One config file that shows why the proxy exists: ten internal MCP services
behind a single authenticated endpoint, each with the middleware its failure
modes deserve, plus a canary receiving mirrored production traffic.

The full config is [`examples/production-gateway.toml`](../examples/production-gateway.toml).
Validate it without starting anything:

```bash
mcp-proxy --config examples/production-gateway.toml --check
```

```text
Config OK

  Proxy:    gateway v1.0.0
  Listen:   0.0.0.0:8080
  Backends: 10
    - github (stdio) [timeout, rate-limit, circuit-breaker, retry, filter]
    - jira (http) [timeout, concurrency-limit]
    - docs (http) [timeout, hedging, cache, filter]
    - search (http) [timeout, hedging, filter]
    - db-analytics (stdio) [timeout, concurrency-limit, cache, filter]
    - orders-api (http) [timeout, circuit-breaker, retry, outlier-detection]
    - orders-canary (http) [mirror]
    - payments (http) [timeout, rate-limit, concurrency-limit, filter]
    - reports (http) [timeout, rate-limit, cache, filter]
    - legacy-crm (http) [timeout, circuit-breaker, retry, outlier-detection]
  Auth:     jwt/jwks
  Request coalescing: enabled
  Audit logging: enabled
  Metrics: enabled
```

## Request flow

```
Claude Code / IDEs / agents
        |
        |  HTTP + JWT bearer token
        v
+---------------------------- mcp-proxy :8080 ----------------------------+
|                                                                         |
|  [JWT verify] -> [audit log] -> [metrics] -> [RBAC tool visibility]     |
|      -> [capability filter] -> [argument validation]                    |
|      -> [coalesce identical calls] -> [response cache]                  |
|      -> [mirror tap] -> route by namespace                              |
|                            |                                            |
|   +------------------------+--------------------------+                 |
|   v                        v                          v                 |
|  github/*               orders-api/*               payments/*  ...      |
|  [retry]                [retry]                    [rate limit]         |
|  [rate limit]           [circuit breaker]          [concurrency]        |
|  [timeout]              [outlier detection]        [timeout]            |
|  [circuit breaker]      [timeout]                                       |
|   |                      |        \                    |                |
+---|----------------------|---------\-------------------|----------------+
    v                      v          \ 10% copy          v
  GitHub API          orders-v1        '-> orders-v2    payments service
  (stdio child)       (production)         (staging,    (HTTP, internal)
                                           responses
                                           discarded)
```

Global middleware runs once per request; each backend then applies its own
resilience chain. The mirror tap forks a fire-and-forget copy of matching
requests to the canary; the client only ever sees the primary's response.

## Which setting solves which concern

| Operational concern | Setting | Where in the example |
|---|---|---|
| Only authenticated clients get in | `[auth] type = "jwt"` with JWKS | global |
| A role sees only the tools it needs | `[[auth.roles]]` + `[auth.role_mapping]` | `developer`, `analyst`, `operator` |
| Dangerous tools are not exposed at all | `expose_tools` / `hide_tools` | `payments`, `github` |
| A slow backend cannot hold requests forever | `[backends.timeout]` | every backend |
| Third-party API limits are respected | `[backends.rate_limit]` | `github` (30/s), `reports` (2/s) |
| A failing backend stops receiving traffic | `[backends.circuit_breaker]` | `github`, `orders-api`, `legacy-crm` |
| Transient failures are retried, without retry storms | `[backends.retry]` with `budget_percent` | `github`, `orders-api` |
| Tail latency on read-only lookups is cut | `[backends.hedging]` | `docs`, `search` |
| The database sees a bounded connection count | `[backends.concurrency]` | `db-analytics` (5), `payments` (2) |
| A misbehaving instance is temporarily ejected | `[backends.outlier_detection]` | `orders-api`, `legacy-crm` |
| v2 is validated against real traffic, invisibly | `mirror_of` + `mirror_percent` | `orders-canary` |
| Hot repeated reads skip the backend | `[backends.cache]` | `docs`, `db-analytics`, `reports` |
| Identical concurrent calls run once | `[performance] coalesce_requests` | global |
| Oversized arguments are rejected early | `[security] max_argument_size` | global |
| Every call is attributable | `[observability] audit` (JSON logs) | global |
| Dashboards and alerts have data | `[observability.metrics]` + `/admin/metrics` | global |
| Live state is inspectable | `/admin/backends`, `/admin/health`, `/admin/cache/stats` | global |
| The admin surface has its own credential | `[security] admin_token` | global |

## Safety: middleware that re-executes or short-circuits tool calls

Four middlewares change how many times a tool call actually executes, or
whether it executes at all. They are safe on read-only tools and need care on
mutating ones. The proxy cannot know which tools mutate; you declare it by
construction, with `expose_tools` and per-backend placement.

**Caching short-circuits execution.** `tool_ttl_seconds` caches every tool on
that backend; there is no per-tool cacheability flag. Cache only backends
whose exposed tools are read-only, and use `expose_tools` to make that true
by construction (`docs`, `db-analytics`, `reports` here). A cached
`create_refund` would return a stale success without executing; accordingly,
`payments`, `jira`, `orders-api`, and `legacy-crm` have no cache block.

**Mirroring re-executes for real.** A mirrored tool call is not a dry run:
the canary receives and executes it. Mirror only backends whose state you
can afford to touch twice, or point the canary at isolated state. Here
`orders-canary` targets `orders-v2.staging.internal`, so mirrored writes land
in staging, never in production data.

**Hedging duplicates in-flight calls.** When the original request is slow, a
hedge fires a second copy and the first response wins, so a single client
call can execute twice on the backend. Only `docs` and `search`, read-only by
`expose_tools`, are hedged. Never hedge a backend with mutating tools.

**Retries re-execute after ambiguous failures.** A timeout can fire after the
backend applied the change, so a retry may double-execute. Retries here sit
on backends whose tools are read-dominant or idempotent (`github`,
`orders-api`, one cautious retry on `legacy-crm`), with `budget_percent`
capping retry volume during incidents. `payments` gets no retries: a failed
refund is investigated, not replayed.

Request coalescing collapses identical concurrent calls (same backend, tool,
and arguments) into one execution and fans the response out. That is the
point for reads; for mutating tools it means N identical concurrent submits
execute once, which is usually acceptable but worth knowing.

## Operating it

- Scrape `GET /admin/metrics` with Prometheus; request counts and duration
  histograms are labeled per backend.
- `GET /admin/health` for liveness dashboards, `GET /admin/backends` for
  per-backend health and middleware, `GET /admin/cache/stats` for hit rates.
- With JWT auth, the admin API requires `[security] admin_token`; send it as
  a bearer token.
- Run `mcp-proxy --config ... --check` in CI so config drift fails before
  deploy, not at startup. It also warns on references to unset environment
  variables (`${GITHUB_TOKEN}`, `${ADMIN_TOKEN}`, `${ANALYTICS_DB_URL}` here).

## What this example leaves out

Failover chains (`failover_for`), weighted canary routing (`canary_of` +
`weight`, when you want the canary to serve real responses), tool aliasing,
argument injection, composite tools, and hot reload are covered in
[`config.example.toml`](../config.example.toml) and
[architectures.md](architectures.md).
