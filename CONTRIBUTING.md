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
