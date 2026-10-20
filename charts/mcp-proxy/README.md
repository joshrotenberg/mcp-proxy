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
Run `./scripts/check-chart.sh` from the repository root with Helm 3, Python 3.11+
and PyYAML installed (`python3 -m pip install PyYAML==6.0.3`). It checks rendered
ports, authenticated probes, value bounds and config/Secret rollout triggers.
After `cargo build --locked`, add `--binary target/debug/mcp-proxy` to start the
actual CLI from rendered nondefault-port configuration and run probes and a full
MCP session. CI runs both checks. These are local render/application tests;
they do not replace installation and `helm test mcp-proxy` against your cluster.

## Ports and configuration

`service.port` is the client-facing Kubernetes Service port. `containerPort`
defaults to 8080 and names the application's `http` port; the Service target and
both probes use that named port. For example:

```sh
helm upgrade --install mcp-proxy ./charts/mcp-proxy \
  --set service.port=9090 --set containerPort=8181 -f my-values.yaml
```

Clients use Service port 9090; the default configuration binds the application
on 8181. Changing only `service.port` leaves the pod template and listener
unchanged. Both port values must be integers from 1 to 65535.

The `config` string is evaluated as a Helm template. The default uses
`port = {{ .Values.containerPort }}` under `[proxy.listen]`; retain that expression
in custom configuration to keep the listener, container and probes aligned.
An existing literal TOML listener remains supported, but you must set
`containerPort` to the same value and bind a pod-reachable host such as `0.0.0.0`.
The chart does not parse or rewrite arbitrary custom TOML. Since 0.2.2, literal
Helm delimiters in TOML strings must be escaped, for example
`{{ "{{" }}` to render a literal `{{`. Only supply trusted Helm values/templates.

## Credential rotation and rollouts

The pod template hashes both the rendered ConfigMap and the chart-owned Secret
from `secrets`. A config-only edit or Secret value addition/change/removal changes
the relevant checksum during `helm upgrade`, triggering a Deployment rollout.
The checksum annotations `checksum/config` and `checksum/secret` are reserved;
`podAnnotations` cannot override them. Merely creating `secrets` does not inject
credentials: reference its keys through `env[].valueFrom.secretKeyRef`, using
the generated name `<chart-fullname>-secrets` (for example
`mcp-proxy-secrets` with release name `mcp-proxy`).

Externally managed Secrets referenced by `env` are outside the chart's checksum.
Kubernetes does not update existing environment variables when a Secret changes.
After rotating an external Secret, use your Secret controller's documented
restart integration, update a revision annotation in `podAnnotations` through
Helm, or explicitly restart the Deployment:

```sh
kubectl rollout restart deployment/mcp-proxy
kubectl rollout status deployment/mcp-proxy
```

Adjust the namespace and Deployment name for your installation. The same applies
to out-of-band edits of a chart-owned Secret; prefer changing `secrets` through
Helm so the checksum and desired Secret stay consistent. Environment references
are resolved at application startup; a mounted config update cannot refresh an
existing process's environment. Updating credentials can interrupt sessions;
multiple-replica routing is covered separately by #292.

## Health probes

The `/livez` and `/readyz` endpoints return only `ok` and require no credentials.
Readiness means the proxy finished connecting backends and constructing its
router; backend failures after startup do not trigger pod restarts or remove
otherwise usable replicas. Use the authenticated `/admin/health` endpoint to
monitor detailed backend health. Management requires a dedicated
`security.admin_token`; ordinary/scoped proxy tokens never grant admin access,
and omitting the management credential disables all admin endpoints.

The chart probes, Helm smoke test, and Docker healthcheck use process probes, so
admin tokens need not be placed in probe headers.

## Multiple replicas

MCP sessions live in one process. The chart keeps `replicaCount: 1` by default;
its Service now defaults to `sessionAffinity: ClientIP` and
`sessionAffinityTimeoutSeconds: 10800` (integer range 1–86400). For direct
Service traffic with a stable visible source IP:

```yaml
replicaCount: 2
service:
  sessionAffinity: ClientIP
  sessionAffinityTimeoutSeconds: 10800
```

POST, GET/SSE and DELETE must reach the session owner even across new TCP
connections. `autoscaling.enabled` can change the replica count but does not
share sessions. A response cache in Redis does not share sessions either.
An ingress controller may bypass this Service and balance directly over pods;
configure and test affinity there. Many clients behind NAT can land on one pod,
and changing source IPs/affinity expiry can break sessions. If a verified external
router handles affinity, `service.sessionAffinity: None` disables the Service
setting; it also omits `sessionAffinityConfig`. Chart 0.2.3 adds this default,
while keeping one replica and the same listener/Service ports.

Read the [session routing and recovery guide](https://docs.rs/mcp-proxy/latest/mcp_proxy/#multiple-replicas-and-sessions)
and the tested [NGINX example](../../examples/nginx-session-affinity.conf) before
using replicas/HPA. In particular, pod loss/scale-in/rollouts require a new MCP
session; transport failure or JSON-RPC `-32005` is not permission to replay a
possibly executed tool call. Set termination grace above the configured drain
timeout plus cleanup, and test recovery through your actual ingress/client.
