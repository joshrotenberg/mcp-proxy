You are diagnosing issues with an mcp-proxy deployment. Use the available admin tools to investigate.

## Diagnostic Steps

### 1. Check Overall Health
Call `proxy/health_check` to see if any backends are unhealthy.
- If all healthy: issue is likely in auth, config, or client-side
- If some unhealthy: focus on those backends

### 2. Check Backend Status
Call `proxy/list_backends` for detailed backend info.
- Look for consecutive_failures > 0
- Check transport types match expectations
- Verify expected backends are present

### 3. Check Sessions
Call `proxy/session_count` to see active connections.
- Zero sessions when clients should be connected = connection issue
- Too many sessions = possible leak or misconfigured clients

### 4. Review Configuration
Call `proxy/config` for a redacted startup snapshot, not current live topology.
Verify private fields against the original file locally; never request secrets.
- Check listen address (0.0.0.0 vs 127.0.0.1)
- Verify auth settings match client expectations
- Check for overly aggressive rate limits or timeouts

## Common Issues

| Symptom | Likely Cause | Fix |
|---------|-------------|-----|
| 401 Unauthorized | Auth misconfigured | Check token/JWT/OAuth config |
| Backend unhealthy | Backend not running | Verify backend process/URL |
| Timeout errors | Timeout too short | Increase `[backends.timeout]` |
| Circuit breaker open | Backend failures | Diagnose backend outcomes; do not weaken protection blindly |
| Missing tools | Filter too strict | Check expose_tools/hide_tools |
| High latency | Backend work or contention | Measure first; cache only suitable read-only results |
| Connection refused | Wrong listen address | Use 0.0.0.0 for containers |

## Tools Available

- `proxy/list_backends` -- backend status and health
- `proxy/health_check` -- health summary
- `proxy/session_count` -- active sessions
- `proxy/config` -- current configuration
- `proxy/search_tools` -- find tools (if discovery enabled)
