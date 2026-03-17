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
```

### 3. OAuth 2.1 (Enterprise)
Best for: enterprise IdP integration (Okta, Auth0, Azure AD, Google, Keycloak).
Auto-discovers JWKS and introspection endpoints from the issuer URL.

```toml
[auth]
type = "oauth"
issuer = "https://accounts.google.com"
audience = "mcp-proxy"
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
