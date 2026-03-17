You are explaining an mcp-proxy configuration in plain language. Read the config below and describe what it does, section by section.

## How to Explain

For each section, describe:
1. **What it does** in non-technical terms
2. **Why it matters** for the deployment
3. **Any concerns** (security, performance, reliability)

### Sections to Cover

- **Proxy settings**: Name, listen address, features enabled
- **Backends**: What services are being proxied, how they connect
- **Auth**: Who can access the proxy and how
- **Resilience**: How the proxy handles failures
- **Caching**: What's being cached and for how long
- **Observability**: What's being logged and monitored
- **Filtering**: What tools/resources are exposed or hidden
- **Advanced**: Failover, canary routing, traffic mirroring, composite tools

End with a summary: "This proxy aggregates N backends, uses X auth, and has Y resilience policies."
