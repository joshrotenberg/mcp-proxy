# Production gateway showcase

One config file that shows why the proxy exists: ten internal MCP services
behind a single authenticated endpoint, each with the middleware its failure
modes deserve, plus a canary receiving mirrored production traffic.

The full config is [`examples/production-gateway.toml`](../examples/production-gateway.toml).
Validate it without starting anything:

```bash
mcp-proxy --config examples/production-gateway.toml --check
```

Set the required secrets in the deployment environment first: `GITHUB_TOKEN`,
`ADMIN_TOKEN` and `ANALYTICS_DB_URL`. For an **offline check only**, disposable
values demonstrate the command without contacting an IdP or backend:

```bash
GITHUB_TOKEN=offline-github ADMIN_TOKEN=offline-admin \
ANALYTICS_DB_URL=postgres://offline.invalid/analytics \
mcp-proxy --config examples/production-gateway.toml --check
```

Successful output starts with `Config OK` and lists ten backends. Never use
these placeholder credentials in a running deployment. `--check` validates and
resolves supported fields; missing/empty secret references are **errors**, not
warnings. It does not connect to JWKS/backends, install npm packages, verify tool
names/read-only behavior, or prove a reachable listener. The `.internal` peers,
IdP, Node/npm and `/usr/local/bin/db-mcp-server` must be supplied by your deployment.
The database executable must accept `--read-only` and read `DB_CONNECTION_STRING`
from its environment. Subprocess arguments and URLs are literal; they do not
expand `${...}` references. Resolve other values before generating the config.

The sample JWT audience is the public MCP URL. If an ingress exposes `/mcp`,
route it to the proxy's `/` and expose the path-aware protected-resource metadata
endpoint too. Do not derive audience from the internal listener. See
[OAuth resource identity](../README.md#oauth-resource-identity-and-endpoint-setup).

## Request flow

See the [authoritative middleware order](https://docs.rs/mcp-proxy/latest/mcp_proxy/#middleware-order).
Caller authorization runs before aliases/filtering, validation and shared
coalescing/cache when enabled (coalescing is off in this mixed-write recipe). Each selected backend then runs its resilience chain.
The mirror forks a real copy to `orders-canary`; its response is discarded.
The client receives the primary path's result. This describes topology, not
an alternative ordering diagram.

## Which setting solves which concern

| Operational concern | Setting | Where in the example |
|---|---|---|
| Only authenticated clients get in | `[auth] type = "jwt"` with JWKS | global |
| A role sees only the tools it needs | `[[auth.roles]]` + `[auth.role_mapping]` | `developer`, `analyst`, `operator` |
| Dangerous tools are not exposed at all | `expose_tools` / `hide_tools` | `payments`, `github` |
| Bound dispatched backend requests (not startup/health probes) | `[backends.timeout]` | non-mirror backends |
| Third-party API limits are respected | `[backends.rate_limit]` | `github` (30/s), `reports` (2/s) |
| A failing backend stops receiving traffic | `[backends.circuit_breaker]` | `github`, `orders-api`, `legacy-crm` |
| Transient failures are retried, without retry storms | `[backends.retry]` with `budget_percent` | `github`, `orders-api` |
| Tail latency on read-only lookups is cut | `[backends.hedging]` | `docs`, `search` |
| Limit logical backend requests (hedges can add attempts; not a DB pool size) | `[backends.concurrency]` | `db-analytics` (5), `payments` (2) |
| A misbehaving instance is temporarily ejected | `[backends.outlier_detection]` | `orders-api`, `legacy-crm` |
| v2 is validated against real traffic, invisibly | `mirror_of` + `mirror_percent` | `orders-canary` |
| Hot repeated reads skip the backend | `[backends.cache]` | `docs`, `db-analytics`, `reports` |
| Independent writes remain independent | `[performance] coalesce_requests = false` | global; enable only on a wholly shareable read workload |
| Oversized arguments are rejected early | `[security] max_argument_size` | global |
| MCP completions have metadata, without caller identity or bodies | `[observability] audit` (JSON logs) | global |
| Dashboards and alerts have data | `[observability.metrics]` + `/admin/metrics` | global |
| Live state is inspectable | `/admin/backends`, `/admin/health`, `/admin/cache/stats` | global |
| The admin surface has its own credential | `[security] admin_token` | global |

## Safety: middleware that re-executes or short-circuits tool calls

Five mechanisms change how many times a tool call actually executes, or
whether it executes at all. Read-only tools still need freshness/caller-sharing review; mutating tools
need an explicit side-effect/idempotency contract. The proxy cannot know which tools mutate; you declare it by
construction, with `expose_tools` and per-backend placement.

**Caching short-circuits execution.** `tool_ttl_seconds` caches every tool on
that backend; there is no per-tool cacheability flag. Cache only backends
whose exposed tools you have verified read-only, then use `expose_tools`
to restrict the surface (`docs`, `db-analytics`, `reports` here). A cached
`create_refund` would return a stale success without executing; accordingly,
`payments`, `jira`, `orders-api`, and `legacy-crm` have no cache block.

**Mirroring re-executes for real.** A mirrored tool call is not a dry run:
the canary receives and executes it. Mirror only backends whose state you
can afford to touch twice, or point the canary at isolated state. Here
`orders-canary` targets `orders-v2.staging.internal`, so mirrored writes land
in staging, never in production data.

**Hedging duplicates in-flight calls.** When the original request is slow, a
hedge fires another copy and a successful alternative can beat a failed
primary, so a single client call can execute more than once on the backend.
If all attempts fail, the original primary result is retained. Only `docs` and
`search`, whose exposed tools must be verified read-only, are hedged. Never hedge a backend with mutating tools.

**Retries re-execute after ambiguous failures.** A timeout can fire after the
backend applied the change, so a retry may double-execute. Retries here sit
on backends whose tools are read-dominant or idempotent (`github`,
`orders-api`, one cautious retry on `legacy-crm`), with `budget_percent`
capping retry volume during incidents. `payments` gets no retries: a failed
refund is investigated, not replayed. The retry blocks are illustrative: verify
that every exposed operation is idempotent or deduplicated by the backend before
enabling them. A read-dominant backend can still contain unsafe writes; remove
its retry block if that contract is unavailable.

Request coalescing collapses identical concurrent calls (same backend, tool,
and arguments) into one execution and fans the response out. That is the
point for reads; for mutating tools it means N identical concurrent submits
execute once. That is not a safe default for independent writes: disable
global coalescing unless every affected tool can share one result. Cancellation
or a group deadline wakes followers with errors without replay; remote side
effects may already have happened. See the [coalescing contract](https://docs.rs/mcp-proxy/latest/mcp_proxy/coalesce/).

## Operating it

- Scrape `GET /admin/metrics` with Prometheus; request counts and duration
  histograms use bounded public requested-namespace labels (`backend:<name>`), not the actual
  canary/failover destination. Unknown dynamic namespaces collapse to `_unknown`.
  Configured instances own isolated recorders unless an embedding host injects
  a shared owner; see the [telemetry guide](https://docs.rs/mcp-proxy/latest/mcp_proxy/telemetry/) for ownership, attribution limits and exporter shutdown.
- `/livez` and `/readyz` are credential-free process probes, not backend checks.
  Use authenticated `GET /admin/health` for cached backend health, `GET /admin/backends` for
  per-backend health and middleware, `GET /admin/cache/stats` for hit rates.
- With JWT auth, the admin API requires `[security] admin_token`; send it as
  a bearer token.
- Run `--check` with required secrets before deploy, then verify a full initialized
  MCP session, discovery and a safe call against the actual deployment. Audit and
  access logs omit caller identity, argument/result bodies and HTTP auth failures;
  they are not a compliance ledger. Supply identity correlation/immutable storage
  outside this proxy if required.
- Keep the dedicated admin token separate from ordinary JWT roles and credentials.
  `proxy/config` and `GET /admin/config` are redacted startup diagnostics, not an
  editable backup or the current live backend topology. See the
  [management trust contract](../README.md#admin-api).
- Follow the [reload/restart contract](https://docs.rs/mcp-proxy/latest/mcp_proxy/#hot-reload).
  This policy-bearing recipe cannot replace backends or edit policy live.
  Even admitted plain replacement is remove-then-add; failure may lose the old
  backend (#277). Independent startup/health budgets remain #283; list-change
  refresh after removing an earlier backend remains #289.
- For multiple replicas, follow the [session routing and owner-loss guide](https://docs.rs/mcp-proxy/latest/mcp_proxy/#multiple-replicas-and-sessions)
  and [Helm guidance](../charts/mcp-proxy/README.md). Redis shares responses, not
  sessions. Cache reuse requires an explicit generation changed with backend or
  authority semantics; see [cache isolation](https://docs.rs/mcp-proxy/latest/mcp_proxy/cache/).
  Do not replay ambiguous mutating calls after retries, owner loss or cancellation.

## What this example leaves out

Failover chains (`failover_for`), weighted canary routing (`canary_of` +
`weight`, when you want the canary to serve real responses), tool aliasing,
argument injection, composite tools, and hot reload are covered in
[`config.example.toml`](../config.example.toml) and
[architectures.md](architectures.md).
