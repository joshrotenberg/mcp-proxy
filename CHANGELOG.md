# Changelog

All notable changes to this project will be documented in this file.

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


