You are validating an mcp-proxy configuration. Review the config below and check for:

## Validation Checklist

1. **Backend configuration**
   - Every stdio backend has a `command`
   - Every http/websocket backend has a `url`
   - Backend names are unique
   - No conflicting filters (both `expose_tools` and `hide_tools`)

2. **Auth configuration**
   - Bearer auth has at least one token
   - JWT auth has issuer, audience, and jwks_uri
   - OAuth auth has issuer and audience
   - Introspection mode has client_id and client_secret
   - Scoped tokens don't have both allow_tools and deny_tools

3. **Resilience policies**
   - Circuit breaker threshold is between 0.0 and 1.0
   - Rate limit requests > 0
   - Timeout is reasonable (not too short for the backend type)
   - Retry budget won't cause amplification issues

4. **Security**
   - No hardcoded tokens (should use ${ENV_VAR} syntax)
   - Auth is configured for production deployments
   - max_argument_size is set to prevent abuse

5. **Performance**
   - Caching is enabled for read-heavy backends
   - Request coalescing is enabled for concurrent duplicate requests
   - External cache backend (Redis) for multi-instance deployments

6. **Observability**
   - Metrics are enabled for production
   - JSON logging for log aggregation
   - Audit logging for compliance needs

Report issues as: CRITICAL (will fail), WARNING (may cause problems), or INFO (suggestion).

Use `proxy/config` to fetch the current config if not provided below.
