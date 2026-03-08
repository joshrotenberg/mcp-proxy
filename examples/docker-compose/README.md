# Docker Compose Example

A minimal docker-compose setup with the gateway proxying to an HTTP MCP backend.

## Services

- **gateway** -- mcp-proxy on port 8080, configured to proxy to the everything backend
- **everything** -- the MCP "everything" demo server (tools, resources, prompts) on port 3001

## Usage

```bash
docker compose up --build
```

The gateway is available at `http://localhost:8080/mcp` (MCP HTTP transport).

Admin endpoints:

```bash
# List backends
curl http://localhost:8080/admin/backends

# Health check
curl http://localhost:8080/admin/health

# Prometheus metrics
curl http://localhost:8080/admin/metrics
```

## Customizing

Edit `gateway.toml` to add more backends, enable auth, adjust rate limits, etc. See
[`config.example.toml`](../../config.example.toml) for the full configuration reference.
