# Choosing an MCP gateway

Several open projects put a gateway between MCP clients and MCP servers. They
belong to different classes and solve different problems; the right choice
depends on where your servers run and what you need the layer in front of them
to do. This page describes each class and where mcp-proxy fits. None of these
is a universal winner, and for some setups the right answer is more than one
of them.

Last reviewed: August 2026. Project scopes change; follow the links for
current capabilities.

## At a glance

| Project | Class | Built for |
|---|---|---|
| [Docker MCP Gateway](https://github.com/docker/mcp-gateway) | Desktop toolkit and catalog gateway | Local development with Docker Desktop and the Docker MCP Catalog |
| [IBM ContextForge](https://github.com/IBM/mcp-context-forge) | Federation and registry platform | Organization-wide governance of MCP, A2A, and REST services |
| [Microsoft MCP Gateway](https://github.com/microsoft/mcp-gateway) | Kubernetes control plane | Session-aware MCP routing and server lifecycle on Kubernetes with Entra ID |
| [Kong AI Gateway](https://developer.konghq.com/ai-gateway/) | Enterprise API and AI gateway | Standardizing API, LLM, and MCP traffic on one gateway platform |
| mcp-proxy | MCP traffic plane | Self-hosted MCP fleets that need resilient, policy-aware routing without adopting a platform |

## Docker MCP Gateway

A Docker CLI plugin that powers the MCP Toolkit in Docker Desktop. Its center
of gravity is the developer workstation: it installs MCP servers from the
Docker MCP Catalog, runs them in containers, manages their lifecycle, and
gives local clients a single access point. Choose it when your
servers come from the catalog and Docker Desktop is already part of the
workflow. It is a local development tool first; mcp-proxy is a deployed
service first.

## IBM ContextForge

An open-source gateway platform that consolidates MCP servers, A2A agents,
and REST APIs behind one endpoint, with a server registry, virtual server
composition, federation, and an administration surface. Choose it when the
problem is organization-wide: many teams registering servers, multiple
protocols to bridge, and governance requirements that call for a managed
platform. The tradeoff is operating a platform; mcp-proxy is a single-purpose
data plane configured from one file.

## Microsoft MCP Gateway

A reverse proxy and management layer for running MCP servers on Kubernetes,
with session-aware stateful routing, server lifecycle management, and
Microsoft Entra ID authentication. Choose it when your infrastructure is
Kubernetes and your identity is Entra. mcp-proxy targets the complementary
case: environments that are not Kubernetes-shaped, or that want a gateway
without a cluster.

## Kong AI Gateway

An AI connectivity layer built on Kong Gateway that governs LLM provider and
MCP traffic alongside conventional APIs, with Kong's plugin ecosystem,
enterprise controls, and management tooling. Choose it when your organization
already standardizes on Kong (or wants one platform for all API and AI
traffic) and MCP is one traffic type among many. mcp-proxy is the opposite
tradeoff: MCP-only, no surrounding platform.

## Where mcp-proxy does not compete

- **Catalog and desktop UI.** There is no curated server catalog and no GUI
  for browsing or installing servers. The Docker MCP Toolkit owns that
  workflow.
- **Server deployment lifecycle.** mcp-proxy launches the stdio commands you
  configure and connects to HTTP or WebSocket servers you already run. It
  does not install, containerize, schedule, or upgrade servers.
- **Registry and federation.** There is no server registry, no
  gateway-to-gateway federation, and no cross-gateway composition.
  ContextForge is built around those.
- **Broad API-gateway ecosystem.** mcp-proxy speaks MCP only. It does not
  proxy REST or gRPC APIs, route LLM provider traffic, or offer a plugin
  marketplace.

## Where mcp-proxy fits

mcp-proxy is a focused, Tower-native MCP traffic plane. It aggregates stdio,
HTTP, and WebSocket backends behind one endpoint and gives each backend its
own resilience chain (timeouts, rate and concurrency limits, retries with
budgets, circuit breakers, hedging, outlier ejection) plus policy middleware
(JWT/JWKS and OAuth 2.1 authentication, role-based tool visibility,
capability filtering, argument injection and validation). It deploys as a
single binary with a TOML config, or embeds in a Rust application as a
library.

These classes also combine. A general API gateway can sit in front of
mcp-proxy and treat it as one more HTTP upstream; servers installed by a
catalog tool can be backends behind it; a local Docker MCP Toolkit setup can
coexist with a shared mcp-proxy endpoint for the fleet.

See the [README](../README.md) for capabilities,
[architectures.md](architectures.md) for deployment patterns, and
[config.example.toml](../config.example.toml) for the full configuration
reference.
