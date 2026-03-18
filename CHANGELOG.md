# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0] - 2026-03-18

### Bug Fixes

- Add PUT /admin/config endpoint for config updates ([#162](https://github.com/joshrotenberg/mcp-proxy/pull/162))

### Features

- Helm chart for Kubernetes deployment ([#158](https://github.com/joshrotenberg/mcp-proxy/pull/158))
- Agentskills.io compliant skills for proxy management ([#159](https://github.com/joshrotenberg/mcp-proxy/pull/159))
- Admin API auth protection ([#164](https://github.com/joshrotenberg/mcp-proxy/pull/164))
- Expose circuit breaker states via admin API ([#166](https://github.com/joshrotenberg/mcp-proxy/pull/166))

### Testing

- Add unit tests for session admin endpoints ([#160](https://github.com/joshrotenberg/mcp-proxy/pull/160))

### Research

- Benchmark proxy overhead with criterion ([#165](https://github.com/joshrotenberg/mcp-proxy/pull/165))



## [0.2.0] - 2026-03-17

### Bug Fixes

- Access log includes backend name in structured output ([#144](https://github.com/joshrotenberg/mcp-proxy/pull/144))
- --check warns about unset environment variables ([#145](https://github.com/joshrotenberg/mcp-proxy/pull/145))
- Add missing REST API endpoints ([#150](https://github.com/joshrotenberg/mcp-proxy/pull/150))
- Failover supports priority field for backend ordering ([#153](https://github.com/joshrotenberg/mcp-proxy/pull/153))
- Add remaining REST API endpoints ([#155](https://github.com/joshrotenberg/mcp-proxy/pull/155))

### Documentation

- Add runnable examples for library embedding ([#127](https://github.com/joshrotenberg/mcp-proxy/pull/127))
- Comprehensive config.example.toml with all options documented ([#142](https://github.com/joshrotenberg/mcp-proxy/pull/142))

### Features

- Add --check config validation flag ([#89](https://github.com/joshrotenberg/mcp-proxy/pull/89))
- Add structured access logging middleware ([#91](https://github.com/joshrotenberg/mcp-proxy/pull/91))
- Add glob pattern support for tool filtering ([#90](https://github.com/joshrotenberg/mcp-proxy/pull/90))
- Expose middleware as composable tower::Layer implementations ([#98](https://github.com/joshrotenberg/mcp-proxy/pull/98))
- Add backend failover routing ([#99](https://github.com/joshrotenberg/mcp-proxy/pull/99))
- Add global rate limiting across all backends ([#108](https://github.com/joshrotenberg/mcp-proxy/pull/108))
- Support .mcp.json as a backend config source ([#109](https://github.com/joshrotenberg/mcp-proxy/pull/109))
- Complete hot reload with backend removal and modification ([#110](https://github.com/joshrotenberg/mcp-proxy/pull/110))
- Add ProxyBuilder for programmatic proxy construction ([#111](https://github.com/joshrotenberg/mcp-proxy/pull/111))
- Add REST management API for backend lifecycle ([#112](https://github.com/joshrotenberg/mcp-proxy/pull/112))
- Integrate utoipa for OpenAPI spec generation ([#128](https://github.com/joshrotenberg/mcp-proxy/pull/128))
- Add cache backend config and validation ([#133](https://github.com/joshrotenberg/mcp-proxy/pull/133))
- Add composite/parallel tool fan-out middleware ([#135](https://github.com/joshrotenberg/mcp-proxy/pull/135))
- Add parameter hiding and renaming for tool customization ([#134](https://github.com/joshrotenberg/mcp-proxy/pull/134))
- Annotation-aware tool filtering ([#137](https://github.com/joshrotenberg/mcp-proxy/pull/137))
- Add per-token tool scoping for bearer auth ([#138](https://github.com/joshrotenberg/mcp-proxy/pull/138))
- Add WebSocket backend transport support ([#139](https://github.com/joshrotenberg/mcp-proxy/pull/139))
- BM25-based tool discovery and search ([#140](https://github.com/joshrotenberg/mcp-proxy/pull/140))
- Add OAuth 2.1 authorization flow support ([#141](https://github.com/joshrotenberg/mcp-proxy/pull/141))
- Support YAML config format ([#146](https://github.com/joshrotenberg/mcp-proxy/pull/146))
- Ergonomic per-backend builder methods for ProxyBuilder ([#147](https://github.com/joshrotenberg/mcp-proxy/pull/147))
- Regex support for tool filtering (re: prefix) ([#149](https://github.com/joshrotenberg/mcp-proxy/pull/149))
- Pure .mcp.json mode (no TOML config needed) ([#148](https://github.com/joshrotenberg/mcp-proxy/pull/148))
- CacheLayer tower::Layer implementation ([#151](https://github.com/joshrotenberg/mcp-proxy/pull/151))
- Search-mode tool exposure for large tool sets ([#154](https://github.com/joshrotenberg/mcp-proxy/pull/154))
- Bump tower-mcp 0.8.8, add session and circuit breaker admin endpoints ([#156](https://github.com/joshrotenberg/mcp-proxy/pull/156))
- Implement Redis and SQLite cache backends ([#157](https://github.com/joshrotenberg/mcp-proxy/pull/157))

### Miscellaneous Tasks

- Move scratch config files to examples/ ([#143](https://github.com/joshrotenberg/mcp-proxy/pull/143))

### Testing

- Add comprehensive end-to-end integration test suite ([#94](https://github.com/joshrotenberg/mcp-proxy/pull/94))
- HTTP transport-level E2E tests ([#152](https://github.com/joshrotenberg/mcp-proxy/pull/152))



## [0.1.1] - 2026-03-08

### Documentation

- Add installation methods to README
- Add installation methods and optimize dist profile ([#71](https://github.com/joshrotenberg/mcp-proxy/pull/71))

### Miscellaneous Tasks

- Release v0.1.0



## [0.1.0] - 2026-03-08

### Bug Fixes

- Exclude examples/docker-compose/proxy.toml from gitignore
- Update CI workflows to use main branch

### Documentation

- Add README, LICENSE files, CI workflow
- Add architecture patterns research and module doc improvements
- Add missing doc comments across all public APIs
- Update README and config.example.toml for all features
- Add AGENTS.md for AI agent context

### Features

- Library mode, hot reload, and gateway refactor
- Middleware modules and example configs
- MCP admin tools under gateway/ namespace
- Enrich per-backend health with timestamps, failure tracking, and transport info
- Add health_check and add_backend MCP admin tools
- Apply per-backend middleware to hot-reloaded backends
- Add cache stats and clear endpoints to admin API
- Add per-backend retry with exponential backoff
- Token passthrough and per-backend static auth
- Retry budget to prevent retry storms
- Passive health checks via outlier detection
- Traffic mirroring / shadowing
- Request hedging via tower-resilience
- Argument injection for tool calls
- Add library mode example
- Add canary/weighted routing middleware

### Miscellaneous Tasks

- Add Dockerfile and .dockerignore
- Switch tower-resilience to published 0.9.1 and fix stale mcp-gateway refs

### Refactor

- Rename to mcp-proxy
- Migrate retry to tower-resilience RetryLayer
- Rename gateway to proxy across entire codebase

### Styling

- Fix rustfmt formatting in retry.rs

### Testing

- Add unit tests for all middleware modules (46 tests)
- Add integration tests with in-process MCP backends
- Add integration tests for admin tools, dynamic backends, and cache stats
- Add admin and metrics test coverage
- Integration tests for inject, mirror, coalesce, and full stack


