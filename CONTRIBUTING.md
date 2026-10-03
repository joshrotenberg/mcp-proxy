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
normal suite and takes approximately 1.2 seconds.

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
