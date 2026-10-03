#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
chart_path="$repo_root/charts/mcp-proxy"
helm lint "$chart_path" --strict
chart_tmp="$(mktemp -d)"
trap 'rm -rf "$chart_tmp"' EXIT
helm template health-check "$chart_path" > "$chart_tmp/default.yaml"
cat > "$chart_tmp/auth-values.yaml" <<'VALUES'
config: |
  [proxy]
  name = "authenticated-proxy"
  [proxy.listen]
  host = "0.0.0.0"
  port = 8080
  [security]
  admin_token = "${ADMIN_TOKEN}"
  [[backends]]
  name = "api"
  transport = "http"
  url = "http://api:8080"
env:
  - name: ADMIN_TOKEN
    valueFrom:
      secretKeyRef:
        name: admin-credentials
        key: token
VALUES
helm lint "$chart_path" --strict -f "$chart_tmp/auth-values.yaml"
helm template health-check "$chart_path" -f "$chart_tmp/auth-values.yaml" > "$chart_tmp/auth.yaml"
python3 - "$repo_root" "$chart_tmp" <<'PY'
import pathlib, sys, tomllib
repo, rendered = map(pathlib.Path, sys.argv[1:])
version = tomllib.loads((repo / 'Cargo.toml').read_text())['package']['version']
for name in ('default', 'auth'):
    text = (rendered / f'{name}.yaml').read_text()
    assert f'image: "ghcr.io/joshrotenberg/mcp-proxy:{version}"' in text
    assert 'path: /livez' in text and 'path: /readyz' in text
    assert '/admin/health' not in text
assert 'name: ADMIN_TOKEN' in (rendered / 'auth.yaml').read_text()
print('Chart versions and authenticated deployment probes validated')
PY
