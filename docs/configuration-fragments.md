# Checked rustdoc configuration fragments

Rustdoc owns the configuration and behavior contracts. Its TOML examples are
checked by `cargo test --locked --test rustdoc_configs --all-features`; repeat
with `--no-default-features` and with default features to verify the declared
feature boundaries. CI's existing Cargo test matrix runs this integration test.
The existing `documentation` suite retains actual README CLI MCP sessions;
fragment preparation does not replace those runtime checks.

Each rustdoc TOML fence has a visible complete/fragment label, required features,
and an adjacent `doc-config` HTML comment consumed by the test. A complete block
is passed unchanged to the real executable's `--check`. For a fragment, the
checker concatenates the named context files in order, adds a newline, then adds
the fragment unchanged. It does not merge tables, repair fields, or silently
supply omitted settings. Copy that same context when trying an example, and use
one alternative at a time. Context filenames are relative to the repository root.

`features=none` means no optional feature is needed. Otherwise an enabled build
must accept the configuration and a build lacking a declared feature must reject
it with that feature's explicit diagnostic. The shared fixtures use loopback
addresses or illustrative commands, and secret references receive disposable
values only in each check subprocess. The proxy-settings import example copies
the declared JSON fixture to an adjacent `.mcp.json`, as required by that example.

## Inventory and composition sources

The test discovers every `//!` and `///` TOML fence under `src/`, including
indented outer doc comments, backtick/tilde fences of three or more characters,
and up to three spaces of Markdown indentation. TOML fences in block comments
must be moved to line docs. Documentation-content attributes (including
`include_str!`) are rejected to prevent hidden fences; metadata attributes such
as `doc(hidden)` remain allowed. Missing declarations and malformed fences fail. Adding an example
requires declaring its context and features alongside it. The type reference
for composite tools and parameter overrides links to the canonical module
example rather than keeping a second copy.

| Source | Examples / context | Optional features |
| --- | --- | --- |
| [Configuration](../src/config.rs) | One complete structure example; proxy settings, transports, backend middleware, separate allowlist/denylist alternatives, routing, three auth alternatives, security and Redis store fragments | `discovery`, `websocket`, `oauth`, `redis-cache`, as declared per fence |
| [Discovery](../src/discovery.rs) | Two proxy-setting alternatives, each with explicit listen settings and a backend context | `discovery` |
| [Aliases](../src/alias.rs), [cache](../src/cache.rs), [canary](../src/canary.rs), [failover](../src/failover.rs), [filter](../src/filter.rs), [injection](../src/inject.rs), [mirror](../src/mirror.rs), [outlier](../src/outlier.rs), [retry](../src/retry.rs) | Backend fragments appended to the shared proxy context | None |
| [Composite](../src/composite.rs) | Proxy context followed by the three referenced backend namespaces | None |
| [Parameter overrides](../src/param_override.rs), [validation](../src/validation.rs) | Append to the existing gateway recipe | None |
| [Introspection](../src/introspection.rs), [RBAC](../src/rbac.rs) | Gateway recipe followed by the dedicated management-token context | `oauth` |
| [Telemetry](../src/telemetry.rs) | Append tracing settings to the gateway recipe | `otel` |

Shared context sources are intentionally small and are verified through every
composition that uses them:

- [Proxy identity/listener](../tests/fixtures/documentation/proxy.toml) for
  fragments that declare their own backends.
- [API backend](../tests/fixtures/documentation/backend.toml) for fragments
  declaring their own proxy settings or referring to the `api` namespace.
- [Composite backends](../tests/fixtures/documentation/search-backends.toml)
  supplying the exact `github`, `jira` and `docs` namespaces.
- [Management credential](../tests/fixtures/documentation/admin.toml) for JWT/OAuth
  examples. An ordinary caller token does not grant management authority.
- [Import fixture](../tests/fixtures/documentation/import.mcp.json), copied only
  for the proxy-settings example that explicitly requests it.
- [Existing gateway recipe](../examples/gateway.toml), reused for global settings
  and per-backend parameter overrides without duplicating a full configuration.

## What the checks establish

The check exercises strict parsing, required configuration fields, feature
admission, import loading and required-secret preparation through the actual
CLI. Negative controls remove required proxy identity/listener context, pass a
backend fragment alone and insert an unknown field; each must fail with its
specific typed parse error and a CLI rejection with redacted diagnostics.
The feature matrix also rejects configurations requiring
absent features, rather than accepting any failure as a successful test.

`--check` does not launch the illustrative backend commands, connect to their
URLs, fetch JWKS, contact Redis or verify that a remote tool exists. These
examples describe configuration shape, not runnable third-party deployments.
Behavior claims remain covered by the configured application suites listed in
[CONTRIBUTING.md](../CONTRIBUTING.md#configured-application-contracts).

The maintainer owns these contexts with the adjacent rustdoc and regression
suite. No renderer, parser or network dependency is added: the checker uses the
repository's existing Rust test dependencies and CLI. Update the owning example
and its declared context together when changing a field or feature requirement.
