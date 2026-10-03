# Deploying mcp-proxy with Helm

Install from this checkout:

```sh
helm upgrade --install mcp-proxy ./charts/mcp-proxy -f my-values.yaml
```

Set `config` to your proxy TOML and supply referenced environment variables through
`env`, preferably using existing Kubernetes Secrets. The default configuration
has no backends; add at least one before running the proxy. See
[config.example.toml](../../config.example.toml) for available options.

The default image tag is `Chart.appVersion`. Release preparation must update
`appVersion` to match `Cargo.toml`, and bump the chart version whenever its
metadata or templates change. `image.tag` can explicitly select another release.
Run `./scripts/check-chart.sh` from the repository root to lint and render both
default and authenticated configurations.

The `/livez` and `/readyz` endpoints return only `ok` and require no credentials.
Readiness means the proxy finished connecting backends and constructing its
router; backend failures after startup do not trigger pod restarts or remove
otherwise usable replicas. Use the authenticated `/admin/health` endpoint to
monitor detailed backend health. Admin API authentication remains enforced when
`security.admin_token` or proxy bearer tokens are configured.

The chart probes, Helm smoke test, and Docker healthcheck use process probes, so
admin tokens need not be placed in probe headers.
