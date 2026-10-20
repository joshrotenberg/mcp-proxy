#!/usr/bin/env bash
# Finite, instrumented smoke checks; generated corpus entries stay outside Git.
set -euo pipefail
cd "$(dirname "$0")/.."
toolchain=nightly-2026-10-19
if ! cargo +"$toolchain" fuzz --version | grep -Fxq 'cargo-fuzz 0.13.2'; then
    echo 'Install cargo-fuzz 0.13.2 (see CONTRIBUTING.md).' >&2
    exit 1
fi
# cargo-fuzz 0.13.2 has no --locked option. Fetch the committed lock explicitly,
# run offline, and reject a lock change instead of silently resolving updates.
cargo +"$toolchain" fetch --manifest-path fuzz/Cargo.toml --locked
lock_before=$(sha256sum fuzz/Cargo.lock)
# Prebuilt cargo-fuzz may itself target musl. Its compiled-in default must not
# select static libc for ASan; use the installed Rust compiler's actual host.
fuzz_host=$(rustc +"$toolchain" -vV | sed -n 's/^host: //p')
test -n "$fuzz_host"
corpus=$(mktemp -d)
trap 'rm -rf "$corpus"' EXIT
for target in config_parse request_transform; do
    mkdir "$corpus/$target"
    cp "fuzz/seeds/$target/"* "$corpus/$target/"
    CARGO_NET_OFFLINE=true cargo +"$toolchain" fuzz run "$target" "$corpus/$target" --dev --target "$fuzz_host" -- \
        -runs=256 -max_len=4096 -max_total_time=30 -timeout=5 \
        -rss_limit_mb=1024 -seed=1
    test "$lock_before" = "$(sha256sum fuzz/Cargo.lock)"
done
