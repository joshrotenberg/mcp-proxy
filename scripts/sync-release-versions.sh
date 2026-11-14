#!/usr/bin/env bash
# Align files that CI checks against the package version.
#
# release-plz bumps Cargo.toml, Cargo.lock and CHANGELOG.md. These files are
# also checked against the package version and are updated here:
#
#   README.md                   `mcp-proxy = "X.Y"` dependency example
#                               (tests/documentation.rs)
#   charts/mcp-proxy/Chart.yaml `appVersion`, the default image tag
#                               (scripts/check-chart.py)
#   fuzz/Cargo.lock             the path dependency's recorded version
#                               (the fuzz job builds with --locked)
#
# Usage: scripts/sync-release-versions.sh [repo-root]
# Idempotent: running it twice makes no further changes.
set -euo pipefail

root="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "could not read a release version from Cargo.toml (got '$version')" >&2
  exit 1
fi
minor="${version%.*}"

# Portable in-place edit (GNU and BSD sed differ on -i).
replace() {
  local file="$1" expr="$2" tmp
  tmp=$(mktemp)
  sed -E "$expr" "$file" > "$tmp"
  if ! cmp -s "$file" "$tmp"; then
    cat "$tmp" > "$file"
    echo "updated $file"
  fi
  rm -f "$tmp"
}

replace README.md "s/^mcp-proxy = \"[0-9]+\.[0-9]+\"$/mcp-proxy = \"$minor\"/"
replace charts/mcp-proxy/Chart.yaml "s/^appVersion: \"[^\"]*\"$/appVersion: \"$version\"/"

before=$(cksum < fuzz/Cargo.lock)
cargo update --quiet --manifest-path fuzz/Cargo.toml --package mcp-proxy
if [[ "$(cksum < fuzz/Cargo.lock)" != "$before" ]]; then
  echo "updated fuzz/Cargo.lock"
fi

grep -q "^mcp-proxy = \"$minor\"$" README.md || {
  echo "README.md has no 'mcp-proxy = \"$minor\"' dependency line" >&2
  exit 1
}
grep -q "^appVersion: \"$version\"$" charts/mcp-proxy/Chart.yaml || {
  echo "charts/mcp-proxy/Chart.yaml appVersion is not $version" >&2
  exit 1
}
