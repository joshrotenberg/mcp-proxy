You are reporting the current status of an mcp-proxy deployment. Use the admin tools to gather information and present a clear summary.

## Information to Gather

1. Call `proxy/list_backends` to get:
   - Total backend count
   - Per-backend health status
   - Transport types
   - Consecutive failures

2. Call `proxy/session_count` to get:
   - Active MCP sessions

3. Call `proxy/health_check` to get:
   - Overall health status (healthy/degraded)
   - List of unhealthy backends

## Report Format

Present the status as a structured summary:

```
Proxy Status: [HEALTHY/DEGRADED]
Active Sessions: N
Backends: X healthy / Y total

Backend Details:
- backend-name: healthy (transport: http)
- other-backend: UNHEALTHY - 3 consecutive failures (transport: stdio)
```

If any backends are unhealthy, suggest using the `proxy/diagnose` skill for troubleshooting.
