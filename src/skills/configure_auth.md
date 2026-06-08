You are helping the user configure authentication for their mcp-proxy. Guide them through choosing and setting up the right auth method.

## Auth Options

### 1. Bearer Tokens (Simple)
Best for: development, internal tools, small teams.

```toml
[auth]
type = "bearer"
tokens = ["${API_TOKEN}"]
```

With per-token tool scoping:
```toml
[auth]
type = "bearer"

[[auth.scoped_tokens]]
token = "${FRONTEND_TOKEN}"
allow_tools = ["files/read_file", "files/list_directory"]

[[auth.scoped_tokens]]
token = "${ADMIN_TOKEN}"
# Empty = all tools allowed
```

### 2. JWT/JWKS (Standard)
Best for: existing JWT infrastructure, manual endpoint configuration.

```toml
[auth]
type = "jwt"
issuer = "https://auth.example.com"
audience = "mcp-proxy"
jwks_uri = "https://auth.example.com/.well-known/jwks.json"

# Required with jwt/oauth: the admin API has no token fallback for these auth
# types, so an explicit admin token is mandatory (config validation enforces it).
[security]
admin_token = "${ADMIN_TOKEN}"
```

With RBAC roles:
```toml
[[auth.roles]]
name = "reader"
allow_tools = ["files/read_file"]

[[auth.roles]]
name = "admin"

[auth.role_mapping]
claim = "scope"
mapping = { "mcp:read" = "reader", "mcp:admin" = "admin" }
# default_deny = true   # gateway hardening: deny authenticated tokens whose
                        # scope is not in `mapping` (default false = pass-through)
```

For gateway deployments, set `default_deny = true` so that an authenticated
principal carrying a scope not listed in `mapping` is denied rather than passed
through with unrestricted access. It defaults to `false` for backwards
compatibility, and never affects requests with no token claims at all.

### 3. OAuth 2.1 (Enterprise)
Best for: enterprise IdP integration (Okta, Auth0, Azure AD, Google, Keycloak).
Auto-discovers JWKS and introspection endpoints from the issuer URL.

```toml
[auth]
type = "oauth"
issuer = "https://accounts.google.com"
audience = "mcp-proxy"

# Required with jwt/oauth (see note above).
[security]
admin_token = "${ADMIN_TOKEN}"
```

With token introspection (for opaque tokens):
```toml
[auth]
type = "oauth"
issuer = "https://auth.example.com"
audience = "mcp-proxy"
client_id = "my-client"
client_secret = "${OAUTH_SECRET}"
token_validation = "both"  # jwt first, introspection fallback
```

## Questions to Ask

1. Who needs to access the proxy? (just you, team, external clients)
2. Do you have an existing identity provider? (Google, Okta, Auth0, etc.)
3. Do different users need different tool access? (RBAC)
4. Are tokens JWTs or opaque? (determines validation strategy)
