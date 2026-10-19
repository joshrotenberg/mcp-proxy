# Contributing

mcp-proxy is maintained with an intentionally stable scope. Bug fixes, security
updates, dependency maintenance, documentation corrections, and MCP protocol
compatibility work are welcome. Before investing in a substantial new feature,
please open an issue that explains the concrete deployment need and why it
belongs in the proxy rather than in a reusable tower-mcp layer.

## Development

The project uses Rust 2024 and has a minimum supported Rust version of 1.90.
Use conventional commit prefixes such as `fix:`, `feat:`, `docs:`, `test:`, and
`chore:`.

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib --all-features
cargo test --test '*' --all-features
cargo doc --no-deps --all-features
cargo test --doc --all-features
```

Every behavior change should include tests. Public APIs need doc comments, and
doc examples must continue to pass `cargo test --doc --all-features`.

Please keep pull requests focused on one concern and explain the user-visible
reason for the change.

The all-feature integration suite requires a working Docker daemon and the
`redis:7-alpine` image. Run `docker info` and `docker pull redis:7-alpine` before
testing. Redis startup failures fail the suite rather than counting as passing
tests. SQLite uses temporary files. The outlier recovery regression runs in the
normal suite and takes approximately 1.2 seconds. Watched-file CLI regressions
also require Python 3 for their minimal stdio MCP fixture; no Python packages or
external MCP servers are needed.

## Temporary upstream fix and release prerequisite

The current development tree pins `tower-mcp` and `tower-mcp-types` to an exact
Git revision for the credential-safe HTTP and JWKS diagnostic fixes in
[tower-mcp#1586](https://github.com/joshrotenberg/tower-mcp/pull/1586).
Published version 0.24.0 does not contain that fix.

Embedding applications that pass `tower-mcp` types across the proxy API must
use the same Git revision for their `tower-mcp`/`tower-mcp-types` dependencies.
A registry dependency has a different Rust crate identity, even at version
0.24.0. The upgrade from the previously exposed 0.23 types requires a minor
release of this pre-1.0 library.

Before any release, replace these Git pins with published crate versions that
contain the fix and rerun every required check. Cargo resolves versioned Git
dependencies against the registry when publishing, so retaining `version =
"0.24.0"` with a Git pin would lose the fix in the published crate. Release
publication is on hold until this prerequisite and release approval are met.

## Coverage and minimum Rust version

CI checks all targets with default, all, and no default features on Rust 1.90:

```sh
cargo +1.90 check --locked --all-targets
cargo +1.90 check --locked --all-targets --all-features
cargo +1.90 check --locked --all-targets --no-default-features
```

To reproduce the coverage report, prepare Docker/Redis as above, then run:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.1 --locked
mkdir -p target/coverage
cargo llvm-cov --locked --all-features --lib --tests --lcov --output-path target/coverage/lcov.info
cargo llvm-cov report --html --output-dir target/coverage
cargo llvm-cov report > target/coverage/summary.txt
```

The CI `coverage-report` artifact contains LCOV, an HTML report, and a text
summary. It measures unit and integration tests; stable Rust doctests run in the
separate test matrix and are not included in this coverage report. Line coverage
is a guide to missing tests, not proof of correctness. No percentage gate is set
until there is a representative baseline and a reason for the chosen threshold.

## Configured application contracts

Keep fast middleware/unit tests, but use `Proxy::from_config` or the executable
for claims about configuration wiring, authorization, transport, and lifecycle.
`tests/e2e.rs` and most of `tests/http_e2e.rs` assemble upstream services manually;
they test composition, not whether the configured application installs that stack.
A passing initialize request alone does not prove a usable MCP session.

Reusable fixtures live under `tests/support/` (included by integration binaries):

- `http.rs`: bound local server, five-second HTTP client, and explicit MCP
  initialize → initialized (empty 202) → list/call helpers. Requests have distinct
  IDs; assertions verify HTTP status, JSON-RPC envelope, protocol, result, and
  successful session termination. An expired session currently returns an HTTP
  200 JSON-RPC session-not-found error. Wire traces record methods, session IDs,
  elapsed times and response statuses, excluding tokens and argument bodies.
- `app.rs`: configured embedding and actual CLI ownership, bounded readiness,
  recent stderr on failure, and explicit shutdown. Only `/readyz` is polled;
  initialization and MCP calls are never retried. Early CLI exit drains stderr
  before reporting the failure.
- `backend.rs` and `fixtures/session_backend.py`: local HTTP, optional WebSocket,
  and Python 3 stdio peers with exact echo results and a call journal. Stdio and
  HTTP require the initialized notification before discovery/calls. Assert the
  journal is empty after policy rejection, then verify exact transformed
  arguments and one backend execution after success.
- `oauth.rs`: local discovery/JWKS/introspection authority with test-only signing
  keys and controlled invalid-issuer, audience, response-size and delay modes.

| Contract | Configured regression suite |
|---|---|
| Complete bearer session, direct/search, exact injected arguments, hidden-tool denial with zero calls, DELETE | `configured_sessions.rs`: embedded and CLI × stdio/HTTP/WebSocket (feature gated) |
| CLI OAuth introspection and verified audience; full JWT/opaque/fallback policies | `configured_sessions.rs`, `oauth_sessions.rs` |
| Authentication and process probes followed by list/call | `health_probes.rs` |
| Management authority, credential-bound sessions; search/composite target authority | `management_authority.rs`, `search_invocation.rs` |
| Watched add/replace continuity for stdio/HTTP/WebSocket; rejected policy revisions | `reload_runtime.rs` |
| Controlled pending work, deadlines, recovery without replay | `coalesce_runtime.rs` |
| Cancellation during initialization, SIGTERM with pending work/SSE, short drain, child reap and embedded owner cleanup | `shutdown_runtime.rs` |
| CLI log flags/privacy and configured resilience/cache/metric outcomes | `cli_logging.rs`, `outcome_runtime.rs` |

Run `cargo test --test configured_sessions --all-features` for the new contract
matrix, and repeat with `--no-default-features` to exercise bearer sessions
without OAuth/discovery/WebSocket. CI also runs default features. Startup,
readiness, notification and fixture cleanup failures must fail the test: do not
skip them, retry sessions internally, or relax timeouts to hide stalls. The
previous intermittent watched-HTTP initialization stall remains an investigation
unless evidence identifies its cause; a passing repetition alone is not a fix.
