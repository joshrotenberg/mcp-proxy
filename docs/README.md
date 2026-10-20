# Documentation map

Choose a task below. Rustdoc owns current behavior/API contracts; repository
guides combine those contracts into recipes. Architecture research is not a
supported-feature checklist. The [strategy review](documentation-review-2026-10-20.md)
proposes the next structure; no Pages/mdBook migration has been selected.

**Version:** this checkout contains unreleased hardening changes. Installation
commands fetch published artifacts, and `docs.rs/latest` describes the latest
published crate, which may differ. For this checkout run
`cargo doc --no-deps --all-features --open`; the crate entry point is
[`src/lib.rs`](../src/lib.rs), with typed configuration in
[`src/config.rs`](../src/config.rs). Do not mix registry and development Git
crate identities; see the [dependency/release prerequisite](../CONTRIBUTING.md#temporary-upstream-fix-and-release-prerequisite).

| Audience / task | Start here | Contract / verification |
| --- | --- | --- |
| Run a first local proxy | [README quickstart](../README.md#quick-start) | Node/npm required; CLI `--check`, then a full MCP session |
| Configure transports and policies | [Annotated config](../config.example.toml), [example configs](../examples/) | [Typed config rustdoc](https://docs.rs/mcp-proxy/latest/mcp_proxy/config/); strict preparation before effects |
| Authenticate and limit callers | [Auth/RBAC guide](../README.md#authentication), [resource identity](../README.md#oauth-resource-identity-and-endpoint-setup) | [Management trust](../README.md#admin-api), separate backend credentials |
| Operate a production gateway | [Production recipe](production-gateway.md) | Read-only/replay constraints, telemetry ownership, reload and session limits |
| Deploy containers / Kubernetes | [Compose](../examples/docker-compose/README.md), [Helm](../charts/mcp-proxy/README.md) | Process probes, listener ports, credential rotation and session affinity |
| Understand ordering / caching / recovery | [Crate guide](../src/lib.rs), [cache module](../src/cache.rs), [outcome module](../src/outcome.rs) | Authoritative request order, cache generations and no transparent replay |
| Embed and shut down | [Crate quickstart/lifecycle](../src/lib.rs), [Rust examples](../examples/library-mode.rs) | Configured router preserves policy; raw upstream access does not |
| Inspect management contracts | [Admin rustdoc](../src/admin.rs), authenticated `/admin/openapi.json` | Schema/handler inventory plus configured HTTP contract tests |
| Troubleshoot or change configuration | [Reload guide](../README.md#supported-hot-reload-changes), [management guide](../README.md#admin-api) | Redacted startup snapshots; policy changes require restart |
| Contribute, validate, prepare a release | [CONTRIBUTING](../CONTRIBUTING.md), [AGENTS](../AGENTS.md) | Docker/NGINX/Redis, feature matrix, exact dependency pins and release hold |
| Assess patterns / alternatives / performance | [Architecture research](architectures.md), [comparison](comparison.md), [historical benchmarks](benchmarks.md) | Distinguish external/future ingredients and historical measurements |
| Integrate an external decision gate | [Embedding pilot](external-tool-gates.md) | Application-owned design; no built-in vendor endpoint or live policy stack |

## What the checks establish

`cargo test --test documentation --all-features` checks complete Markdown TOML and explicitly composed README fragments
with the actual CLI, every example TOML and the annotated config. Test child
processes receive disposable credentials; real secrets are never needed.
README quickstart/JWT workflows run with local test peers and a test IdP,
proving initialization, discovery, tool results and management separation.
Local inline Markdown link paths are checked. `cargo test --doc --all-features`
compiles/runs Rust examples; TOML schema fragments in module rustdoc are not
automatically executable configs. Other configured suites verify the associated
runtime contracts; see the [coverage map](../CONTRIBUTING.md#configured-application-contracts).

An offline check does not install a backend, contact an IdP, test external
URLs or Kubernetes, or prove a tool is read-only. Verify those conditions in
your deployment before enabling cache, coalescing, mirror, retry or hedging.
