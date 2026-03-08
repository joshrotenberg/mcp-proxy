# MCP Gateway Architecture Patterns and Use Cases

Research compiled March 2026. This document catalogs theoretical and real-world
architecture patterns for an MCP gateway, informed by emerging industry practice,
traditional API gateway patterns (Kong, Envoy, Traefik), and the specific
capabilities of `mcp-proxy`.

---

## 1. Enterprise Tool Consolidation Hub

**Scenario:** A large engineering organization has dozens of internal MCP servers
(GitHub, Jira, Confluence, internal APIs, databases) used by hundreds of
developers through AI coding assistants. Each developer's `mcp.json` is
unmanageable, and IT has no visibility into what tools are being used or by whom.

**Architecture:**
```
Developer (Claude Code / Cursor / Windsurf)
  --> mcp-proxy (central, IT-managed)
        --> github backend (stdio)
        --> jira backend (http)
        --> confluence backend (http)
        --> internal-api backend (http)
        --> db-query backend (stdio)
```

**Middleware config:**
- JWT auth via corporate IdP (Okta/Azure AD) with RBAC
- Per-backend rate limits to protect upstream APIs
- Circuit breakers on flaky third-party backends
- Timeouts tuned per backend (short for DB, long for Jira)
- Capability filtering: hide destructive tools (e.g., `jira/delete_issue`)
  from non-admin roles
- Audit logging to corporate SIEM

**Gateway features leveraged:** Namespace routing, JWT/RBAC, rate limiting,
circuit breaker, capability filtering, audit logging, Prometheus metrics.

**Industry precedent:** Kong, Traefik Hub, and Envoy AI Gateway all offer
enterprise MCP gateway products with similar consolidation patterns. Microsoft's
mcp-proxy project provides Kubernetes-based session-aware routing for this
use case.

---

## 2. Compliance-First Financial Services Gateway

**Scenario:** A bank's AI development team needs MCP tool access but must satisfy
SOX, SOC 2, and internal audit requirements. Every tool invocation must be
logged with caller identity, arguments, and results. Certain tools (anything
that writes data) require additional authorization checks. PII must never leave
the network.

**Architecture:**
```
AI Assistants (restricted network)
  --> mcp-proxy (DMZ, hardened)
        --> market-data backend (http, read-only)
        --> portfolio-analytics backend (http, read-only)
        --> trade-execution backend (http, write, restricted)
        --> compliance-check backend (stdio)
```

**Middleware config:**
- JWT auth with fine-grained RBAC: `analyst` role gets read-only tools,
  `trader` role gets trade execution, `compliance` gets audit tools
- All write tools require `trader` or `compliance` role
- Max argument size limits to prevent data exfiltration
- Rate limiting on trade execution (prevent runaway AI agent trades)
- Structured JSON audit logs shipped to immutable storage
- No caching on trade-execution (must always hit live system)
- Response caching on market-data (60s TTL, reduces API costs)

**Gateway features leveraged:** JWT/RBAC, capability filtering per role,
audit logging (JSON), rate limiting, cache (selective), max argument size,
namespace routing.

**Industry precedent:** FactSet has published on enterprise MCP patterns for
financial services. The Gravitee 2026 survey found 85.6% of organizations have
MCP servers running without security review -- this pattern addresses that gap.

---

## 3. Micro-MCP: Single-Responsibility Backend Composition

**Scenario:** A platform team wants each MCP capability to be an independently
deployable, single-purpose microservice. A file server, a search server, a
database server, and a vector store server each run in their own container
with isolated permissions.

**Architecture:**
```
Agent
  --> mcp-proxy
        --> fs backend (stdio, runs as restricted user)
        --> search backend (http, Kubernetes pod)
        --> db backend (http, Kubernetes pod)
        --> vector backend (http, Kubernetes pod)
```

**Middleware config:**
- Namespace routing: `fs/listDir`, `search/query`, `db/select`, `vector/search`
- Per-backend concurrency limits (prevent one backend from starving others)
- Circuit breakers on each backend (independent failure domains)
- Capability filtering: `fs` backend only exposes read operations
- Hot reload: new microservices added without gateway restart

**Gateway features leveraged:** Namespace routing, concurrency limits,
circuit breaker, capability filtering, hot reload.

**Industry precedent:** The MicroMCP project on GitHub explicitly implements
this pattern. Envoy AI Gateway auto-prefixes tool names with backend names
(e.g., `github__issue_read`), identical to mcp-proxy's namespace separator.

---

## 4. Developer Productivity Multiplier (Local Dev)

**Scenario:** An individual developer wants a single MCP endpoint that aggregates
their personal toolchain: filesystem, GitHub, a local database, a note-taking
app, and a custom CLI tool. They switch between Claude Code and Cursor and
want both to see the same tools without duplicating config.

**Architecture:**
```
Claude Code  --\
                --> mcp-proxy (localhost:8080)
Cursor       --/      --> filesystem backend (stdio)
                      --> github backend (stdio)
                      --> sqlite backend (stdio)
                      --> notes backend (stdio)
                      --> custom-cli backend (stdio)
```

**Middleware config:**
- No auth (local only, bound to 127.0.0.1)
- Timeouts on all backends (30s default)
- Tool aliasing: rename verbose tool names to short forms
  (`read_file` -> `read`, `list_directory` -> `ls`)
- Response caching on filesystem reads (reduce subprocess spawns)
- Debug-level audit logging for troubleshooting

**Gateway features leveraged:** Namespace routing, tool aliasing, response
caching, timeout, observability (debug logging).

**Industry precedent:** Envoy AI Gateway's standalone mode explicitly supports
this -- it reads existing `mcp.json` config and runs a local gateway. FastMCP's
proxy server similarly aggregates multiple backends for local development.

---

## 5. Multi-Model AI Orchestration Layer

**Scenario:** An AI orchestration platform runs multiple specialized agents
(a coding agent, a research agent, a data analysis agent) that each need
different subsets of tools. A planning agent coordinates them. All agents
connect to the same gateway but see different tool surfaces based on their
identity.

**Architecture:**
```
Planning Agent (role: orchestrator)  --\
Coding Agent (role: coder)           --+--> mcp-proxy
Research Agent (role: researcher)    --/      --> github backend
Data Agent (role: analyst)           --/      --> web-search backend
                                              --> filesystem backend
                                              --> database backend
                                              --> code-execution backend
```

**Middleware config:**
- JWT auth with per-agent role claims
- RBAC filtering:
  - `coder`: github, filesystem, code-execution
  - `researcher`: web-search, filesystem (read-only)
  - `analyst`: database, filesystem (read-only)
  - `orchestrator`: all tools
- Rate limiting per agent identity (prevent token-heavy agents from
  monopolizing backends)
- Request coalescing (if multiple agents request the same resource
  simultaneously, deduplicate)
- Prometheus metrics per role for cost attribution

**Gateway features leveraged:** JWT/RBAC, capability filtering per role,
rate limiting, request coalescing, Prometheus metrics, namespace routing.

**Industry precedent:** The "Agent Mesh" pattern identified by FlowZap as one
of six core MCP architecture patterns. The lastmile-ai/mcp-agent project
implements multi-agent coordination through shared MCP context.

---

## 6. SaaS Platform MCP-as-a-Service

**Scenario:** A SaaS company wants to offer MCP access to its platform as a
product feature. Each customer tenant gets their own tool surface, credentials,
and rate limits. The gateway is the product boundary.

**Architecture:**
```
Customer A Agent --> mcp-proxy (multi-tenant)
Customer B Agent -->      --> customer-a/crm backend
Customer C Agent -->      --> customer-a/analytics backend
                          --> customer-b/crm backend
                          --> customer-b/billing backend
                          --> shared/docs backend
```

**Middleware config:**
- JWT auth with tenant ID in claims
- Per-tenant namespace isolation (customer A cannot see customer B's tools)
- Per-tenant rate limits (based on subscription tier)
- Per-tenant circuit breakers (one tenant's failures don't cascade)
- Capability filtering driven by subscription tier:
  - Free: read-only tools, 100 req/min
  - Pro: read/write tools, 1000 req/min
  - Enterprise: all tools, custom limits
- Prometheus metrics with tenant labels for billing
- Hot reload for onboarding new tenants without downtime

**Gateway features leveraged:** JWT/RBAC, namespace routing, rate limiting,
circuit breaker, capability filtering, hot reload, Prometheus metrics.

**Industry precedent:** Composio offers a managed PaaS with 1000+ pre-built
tools and multi-tenancy by default. MintMCP provides one-click deployment
with SOC 2 Type II governance. IBM ContextForge supports multi-tenant
workspaces with isolated tool catalogs.

---

## 7. Edge/IoT Device Fleet Gateway

**Scenario:** A manufacturing company deploys MCP servers on edge compute nodes
connected to PLCs, sensors, and cameras. A central AI system queries device
telemetry and issues control commands through the gateway. Latency and
reliability are critical.

**Architecture:**
```
Central AI System
  --> mcp-proxy (edge aggregation point)
        --> sensor-array-1 backend (http, 10.0.1.x)
        --> sensor-array-2 backend (http, 10.0.2.x)
        --> plc-controller backend (http, 10.0.3.x)
        --> camera-feed backend (http, 10.0.4.x)
```

**Middleware config:**
- Bearer token auth (lightweight, no IdP dependency)
- Aggressive timeouts (2-5s, devices must respond quickly)
- Circuit breakers with fast recovery (devices go offline frequently)
- Capability filtering: AI can read sensors but control commands
  require explicit allowlist
- Response caching on sensor reads (reduce polling load)
- Concurrency limits (devices have limited connection capacity)
- Prometheus metrics for fleet health monitoring

**Gateway features leveraged:** Namespace routing, timeout, circuit breaker,
capability filtering, response caching, concurrency limits, bearer auth,
Prometheus metrics.

**Industry precedent:** IEEE research on MCP for IoT (Internet of Robotic
Things) demonstrates semantic decoupling at the edge. MCP servers embedded
on Raspberry Pi boards expose GPIO states, temperature, and camera feeds
through standard tool interfaces.

---

## 8. CI/CD Pipeline Integration Gateway

**Scenario:** A CI/CD system uses AI agents to analyze build failures, suggest
fixes, and run automated remediation. The agents need access to build logs,
source code, issue trackers, and deployment tools -- but with strict guardrails
to prevent unintended deployments.

**Architecture:**
```
CI Agent (role: ci-bot)
  --> mcp-proxy
        --> circleci backend (http)
        --> github backend (stdio)
        --> sonarqube backend (http)
        --> deploy backend (http, heavily restricted)
        --> slack-notify backend (http)
```

**Middleware config:**
- JWT auth with CI system service account
- RBAC: `ci-bot` role can read build logs, source, and quality metrics;
  can post to Slack; cannot trigger deployments without `deployer` role
- Capability filtering: deploy backend only exposes `deploy/status`,
  hides `deploy/trigger` from ci-bot role
- Rate limiting on GitHub API calls (avoid hitting rate limits)
- Tool aliasing: `circleci/get_build_logs` -> `ci/logs`
- Audit logging for compliance (who deployed what, when)

**Gateway features leveraged:** JWT/RBAC, capability filtering, rate limiting,
tool aliasing, audit logging, namespace routing.

**Industry precedent:** CircleCI's MCP server enables natural language CI
for AI-driven workflows. AWS has published on transforming CI/CD pipelines
with MCP and agentic AI.

---

## 9. Testing and Development Sandbox

**Scenario:** A team developing MCP servers needs a staging environment where
they can test new backends alongside stable ones, with the ability to A/B test
tool implementations, mock backends for integration tests, and validate
configuration changes before production.

**Architecture:**
```
Test harness / MCP Inspector
  --> mcp-proxy (test config)
        --> stable-api backend (http, production mirror)
        --> experimental-api backend (http, staging)
        --> mock-db backend (stdio, returns canned responses)
        --> mock-external backend (stdio, simulates failures)
```

**Middleware config:**
- No auth (test environment)
- Tool aliasing: both `stable-api` and `experimental-api` expose tools
  with the same names under different namespaces, enabling A/B comparison
- Circuit breaker with low thresholds on mock-external (validates
  circuit breaker behavior)
- Response caching validation (verify TTLs work correctly)
- Debug-level logging and tracing for test diagnostics
- Hot reload: swap backend configs during test runs

**Gateway features leveraged:** Namespace routing, tool aliasing, circuit
breaker, response caching, hot reload, observability (debug + tracing).

**Industry precedent:** The MCP Inspector provides programmatic CLI mode
for CI/CD integration testing. Spec-workflow-mcp provides structured
spec-driven development workflows for AI-assisted development.

---

## 10. Zero-Trust Security Perimeter

**Scenario:** A security-conscious organization wants every MCP tool invocation
to go through a central enforcement point that validates identity, checks
authorization, scans arguments for injection attacks, and produces an immutable
audit trail. No direct MCP server access is permitted.

**Architecture:**
```
Any AI client (must present JWT)
  --> mcp-proxy (zero-trust perimeter)
        [Auth layer: verify JWT, extract claims]
        [Validation layer: check argument sizes, scan for injection]
        [Filter layer: RBAC-based tool visibility]
        [Audit layer: log everything]
          --> internal backends (all on private network)
```

**Middleware config:**
- JWT auth with JWKS rotation, short-lived tokens
- RBAC with deny-by-default posture
- Max argument size enforcement (prevent data exfiltration)
- Input validation / sanitization layer
- Structured JSON audit logs to immutable storage (WORM)
- Rate limiting (DDoS protection)
- No caching (security-sensitive, always verify fresh)
- Prometheus metrics with security-relevant counters
  (auth failures, blocked requests, circuit breaker trips)

**Gateway features leveraged:** JWT/RBAC, capability filtering, max argument
size, audit logging (JSON), rate limiting, Prometheus metrics.

**Industry precedent:** Peta MCP Suite implements a zero-trust gateway that
intercepts every MCP request, validates caller identity, checks policies, and
requires human approval for risky actions. Traefik's MCP Gateway enforces
TBAC (task-based access control). The Strata MCP Gateway and Sentinel Gateway
(Rust) implement similar patterns.

---

## 11. Cost Optimization and Token Reduction Proxy

**Scenario:** An organization is spending heavily on AI API costs because agents
make redundant tool calls and fetch the same resources repeatedly. The gateway
acts as an intelligent cache and deduplication layer to minimize upstream calls
and reduce token consumption.

**Architecture:**
```
Multiple AI agents
  --> mcp-proxy (cost optimization layer)
        [Cache layer: aggressive TTLs on read-heavy tools]
        [Coalesce layer: deduplicate concurrent identical requests]
        [Filter layer: hide tools that generate excessive tokens]
          --> expensive-api backend (http, pay-per-call)
          --> knowledge-base backend (http, static content)
          --> search backend (http, metered)
```

**Middleware config:**
- Response caching with tuned TTLs:
  - knowledge-base: 3600s (rarely changes)
  - search: 300s (moderate freshness)
  - expensive-api: 60s (balance cost vs. freshness)
- Request coalescing enabled (deduplicate concurrent identical calls)
- Capability filtering: hide verbose/expensive tools from agents that
  don't need them
- Tool aliasing: expose simplified interfaces that return less data
- Prometheus metrics for cache hit rates and cost tracking

**Gateway features leveraged:** Response caching, request coalescing,
capability filtering, tool aliasing, Prometheus metrics.

**Industry precedent:** FlowZap's "Context Proxy" pattern reports 95%+ token
reduction through caching and compression. Their "Tool Router" pattern
achieves 96% reduction in input tokens via semantic routing that only
exposes relevant tools. MCP server rate limiting guides note that unthrottled
agents can generate 1000+ API calls per minute.

---

## 12. API-to-MCP Bridge (Legacy System Modernization)

**Scenario:** An organization has dozens of existing REST/gRPC APIs that they
want to expose as MCP tools without rewriting them. The gateway fronts
lightweight MCP server adapters that wrap each legacy API.

**Architecture:**
```
AI Agents
  --> mcp-proxy
        --> crm-adapter backend (stdio, wraps REST CRM API)
        --> erp-adapter backend (stdio, wraps gRPC ERP)
        --> legacy-db backend (stdio, wraps ODBC connection)
        --> email-adapter backend (http, wraps SMTP/IMAP)
```

**Middleware config:**
- Tool aliasing: normalize inconsistent naming from different adapters
  into a clean, unified vocabulary
- Capability filtering: expose only safe subset of legacy operations
- Timeouts tuned per legacy system (some are slow)
- Circuit breakers (legacy systems are often fragile)
- Rate limiting to protect legacy systems from AI-generated load
- Hot reload: add new API adapters as they're built

**Gateway features leveraged:** Tool aliasing, namespace routing, capability
filtering, timeout, circuit breaker, rate limiting, hot reload.

**Industry precedent:** IBM ContextForge explicitly federates REST/gRPC APIs
alongside MCP servers behind a unified MCP interface. The pattern of wrapping
existing APIs in MCP servers is widely discussed, with Scalekit publishing
guidance on when and how to wrap APIs in MCP.

---

## 13. Federated Multi-Gateway Mesh

**Scenario:** A large organization has regional or departmental MCP gateways
that need to be discoverable and composable. A top-level gateway federates
the departmental gateways, providing a single entry point while preserving
team autonomy.

**Architecture:**
```
Organization-wide AI agents
  --> org-gateway (top-level federation)
        --> engineering-gateway backend (http, runs its own mcp-proxy)
              --> github, ci, monitoring backends
        --> data-team-gateway backend (http, runs its own mcp-proxy)
              --> warehouse, analytics, ml-ops backends
        --> security-gateway backend (http, runs its own mcp-proxy)
              --> siem, scanner, compliance backends
```

**Middleware config (org-gateway):**
- JWT auth at the top level, token forwarding to child gateways
- Double namespace: `engineering/github/create_issue`
- Per-gateway circuit breakers (entire department failover)
- Rate limiting at org level (global quotas) and child level (team quotas)
- Prometheus metrics aggregated from all child gateways

**Gateway features leveraged:** Namespace routing (nested), circuit breaker,
rate limiting, JWT auth, Prometheus metrics, HTTP backend transport.

**Industry precedent:** IBM ContextForge supports federation of gateways where
multiple instances auto-discover each other and sync their tool registries.
This mirrors traditional API gateway patterns like Kong's multi-zone mesh
and Envoy's service mesh federation.

---

## 14. AI Agent Guardrails and Kill Switch

**Scenario:** An organization running autonomous AI agents needs the ability
to instantly disable specific tools or entire backends if an agent exhibits
unexpected behavior. The gateway serves as the control point for
human-in-the-loop oversight.

**Architecture:**
```
Autonomous AI Agents
  --> mcp-proxy (guardrail layer)
        [Admin API: enable/disable tools in real-time]
        [Rate limiting: detect and throttle runaway agents]
        [Circuit breaker: automatic protection]
        [Audit: full trace of agent actions]
          --> tool backends
```

**Middleware config:**
- Admin API for live tool enable/disable (kill switch)
- Per-tool rate limits (detect agents calling the same tool in a loop)
- Circuit breaker with aggressive thresholds
- RBAC: agents have minimal permissions, human operators have admin
- Hot reload: update capability filters without restart
- Prometheus alerts on anomalous tool call patterns
- Audit logging for post-incident review

**Gateway features leveraged:** Admin API, hot reload, capability filtering,
rate limiting, circuit breaker, RBAC, audit logging, Prometheus metrics.

**Industry precedent:** Sentinel MCP Gateway (Rust) implements kill switch
capabilities alongside circuit breaker isolation. Peta MCP Suite requires
human approval for risky actions. The concept of "circuit breakers for
agentic AI" is an emerging pattern with dedicated research.

---

## 15. Embedded Gateway (Library Mode)

**Scenario:** A Rust application (CLI tool, web service, or desktop app) wants
to embed MCP gateway functionality directly, exposing aggregated tools through
its own interface without running a separate gateway process.

**Architecture:**
```
Custom Rust Application
  --> embedded mcp-proxy (library)
        --> app-specific backend (in-process)
        --> external-api backend (http)
        --> local-tools backend (stdio)
```

**Middleware config:**
- Gateway router embedded in application's axum server
- Application controls auth (integrates with its own auth system)
- Programmatic backend registration (add/remove backends at runtime)
- Custom middleware layers (application-specific validation, transformation)
- Metrics integrated with application's existing observability

**Gateway features leveraged:** Library mode (`Gateway::from_config`,
`gateway.into_router()`), programmatic configuration, all middleware layers.

**Industry precedent:** This follows the pattern of embedding Envoy as a
library (via envoy-mobile) or embedding Kong's core as a library. The
mcp-proxy already supports this via `mcp-proxy = "0.1"` as a crate
dependency with `into_router()` for axum integration.

---

## Cross-Cutting Observations

### Patterns from Traditional API Gateways That Map to MCP

| API Gateway Pattern | MCP Gateway Equivalent |
|---|---|
| Virtual host routing | Namespace-based routing |
| Backend-per-route middleware | Per-backend middleware stack |
| API key / OAuth auth | Bearer token / JWT auth |
| Rate limiting per consumer | Rate limiting per role/tenant |
| Circuit breaker | Circuit breaker per backend |
| Response caching | Tool/resource response caching |
| Request transformation | Tool aliasing |
| API versioning | Namespace versioning (v1/math, v2/math) |
| Canary/blue-green deploy | Hot reload with parallel backends |
| Service mesh federation | Federated multi-gateway mesh |
| Plugin system | Tower middleware layers |
| Health checks | Admin API backend health |

### Emerging Industry Trends (as of March 2026)

1. **Every major API gateway vendor now supports MCP:** Kong, Traefik, Envoy,
   Gravitee, and API7 all have MCP gateway products or features.

2. **Task-Based Access Control (TBAC)** is emerging as a replacement for
   traditional RBAC in AI agent contexts, because agents don't have static
   job functions.

3. **Stateless session management** (Envoy's token-encoding approach) enables
   horizontal scaling of MCP gateways despite MCP being a stateful protocol.

4. **Tool governance and audit** is the top concern -- the Gravitee 2026 survey
   found 85.6% of organizations have unreviewed MCP servers in production.

5. **Federation and registry** patterns are maturing, with ContextForge
   and MintMCP providing tool discovery, versioning, and marketplace features.

6. **Rust-based gateways** (Sentinel, mcp-proxy) are emerging for
   performance-critical deployments, following the trajectory of Envoy (C++)
   and Linkerd (Rust) in the service mesh space.

---

## Sources

- [MCP Architecture Patterns for Production-Grade Agents (FlowZap)](https://flowzap.xyz/blog/mcp-architecture-patterns-for-production-grade-agents)
- [Micro-MCP: Namespaces and Policy (GitHub/DEV)](https://github.com/mabualzait/MicroMCP)
- [Envoy AI Gateway MCP Implementation](https://aigateway.envoyproxy.io/blog/mcp-implementation/)
- [Envoy AI Gateway MCP Performance](https://aigateway.envoyproxy.io/blog/mcp-in-envoy-ai-gateway/)
- [Kong Enterprise MCP Gateway](https://konghq.com/blog/product-releases/enterprise-mcp-proxy)
- [Traefik MCP Gateway](https://traefik.io/solutions/mcp-proxy)
- [IBM ContextForge](https://ibm.github.io/mcp-context-forge/)
- [MCP Best Practices (modelcontextprotocol.info)](https://modelcontextprotocol.info/docs/best-practices/)
- [Enterprise MCP Part Two (FactSet)](https://medium.com/factset/enterprise-mcp-model-context-protocol-part-two-f5cdd7c0444b)
- [MCP for IoT (Glama)](https://glama.ai/blog/2025-08-19-bringing-ai-to-the-edge-mcp-for-iot)
- [MCP and IoT (IEEE)](https://ieeexplore.ieee.org/iel8/8548628/11333934/11235950.pdf)
- [MCP Gateway (Composio)](https://composio.dev/blog/mcp-proxys-guide)
- [MCP Gateway Best Practices (Traefik Hub)](https://doc.traefik.io/traefik-hub/mcp-proxy/guides/mcp-proxy-best-practices)
- [MCP API Gateway Explained (Gravitee)](https://www.gravitee.io/blog/mcp-api-gateway-explained-protocols-caching-and-remote-server-integration)
- [Advanced Auth for MCP Gateway (Red Hat)](https://developers.redhat.com/articles/2025/12/12/advanced-authentication-authorization-mcp-proxy)
- [MCP Registry and Gateway Comparison](https://www.paperclipped.de/en/blog/mcp-registry-gateway-enterprise-ai-agents/)
- [Sentinel MCP Gateway (Rust)](https://wallyblanchard.com/sentinel.html)
- [Resilient AI Agents: Timeout and Retry (Octopus)](https://octopus.com/blog/mcp-timeout-retry)
- [Circuit Breakers for Agentic AI](https://medium.com/@michael.hannecke/resilience-circuit-breakers-for-agentic-ai-cc7075101486)
- [Multi-Tenant SaaS with MCP (NovumLogic)](https://www.novumlogic.com/blog/build-a-dynamic-multi-tenant-saas-platform-with-ai-agents-and-a-custom-mcp-server-client)
- [Tool Aggregation and Conflict Resolution (Stacklok)](https://docs.stacklok.com/toolhive/guides-vmcp/tool-aggregation)
- [CircleCI MCP Server](https://circleci.com/blog/circleci-mcp-server/)
- [MCP on AWS](https://aws.amazon.com/blogs/machine-learning/unlocking-the-power-of-model-context-protocol-mcp-on-aws/)
- [A Deep Dive Into MCP (a16z)](https://a16z.com/a-deep-dive-into-mcp-and-the-future-of-ai-tooling/)
- [Securing MCP with OIDC](https://subramanya.ai/2025/05/21/securing-mcp-with-oidc-and-oidc-a-identity-aware-gateway/)
- [AI Gateway Deep Dive 2026](https://jimmysong.io/blog/ai-gateway-in-depth/)
