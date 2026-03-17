//! OAuth 2.1 token introspection and authorization server discovery.
//!
//! Provides:
//! - [`AuthServerMetadata`]: RFC 8414 authorization server metadata discovery
//! - [`IntrospectionValidator`]: RFC 7662 token introspection as a [`TokenValidator`]
//! - [`FallbackValidator`]: Try JWT validation first, fall back to introspection

use std::sync::Arc;

use tower_mcp::oauth::OAuthError;
use tower_mcp::oauth::token::{TokenClaims, TokenValidator};

// ---------------------------------------------------------------------------
// RFC 8414: Authorization Server Metadata
// ---------------------------------------------------------------------------

/// Discovered authorization server metadata (RFC 8414).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AuthServerMetadata {
    /// The authorization server's issuer identifier.
    pub issuer: String,
    /// URL of the authorization server's JWK Set document.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// URL of the token introspection endpoint (RFC 7662).
    #[serde(default)]
    pub introspection_endpoint: Option<String>,
    /// URL of the token endpoint.
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// URL of the authorization endpoint.
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    /// Supported scopes.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// Supported response types.
    #[serde(default)]
    pub response_types_supported: Vec<String>,
    /// Supported grant types.
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    /// Supported token endpoint auth methods.
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

/// Discover authorization server metadata from an issuer URL.
///
/// Fetches `{issuer}/.well-known/oauth-authorization-server` per RFC 8414.
/// Falls back to `{issuer}/.well-known/openid-configuration` for OIDC providers.
pub async fn discover_auth_server(issuer: &str) -> anyhow::Result<AuthServerMetadata> {
    let client = reqwest::Client::new();
    let issuer = issuer.trim_end_matches('/');

    // Try RFC 8414 first
    let rfc8414_url = format!("{issuer}/.well-known/oauth-authorization-server");
    if let Ok(resp) = client.get(&rfc8414_url).send().await
        && resp.status().is_success()
        && let Ok(metadata) = resp.json::<AuthServerMetadata>().await
    {
        tracing::info!(
            issuer = %metadata.issuer,
            jwks_uri = ?metadata.jwks_uri,
            introspection = ?metadata.introspection_endpoint,
            "Discovered auth server metadata (RFC 8414)"
        );
        return Ok(metadata);
    }

    // Fall back to OpenID Connect discovery
    let oidc_url = format!("{issuer}/.well-known/openid-configuration");
    let resp = client
        .get(&oidc_url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("failed to discover auth server at {oidc_url}: {e}"))?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "auth server discovery failed: {} returned {}",
            oidc_url,
            resp.status()
        );
    }

    let metadata = resp
        .json::<AuthServerMetadata>()
        .await
        .map_err(|e| anyhow::anyhow!("failed to parse auth server metadata: {e}"))?;

    tracing::info!(
        issuer = %metadata.issuer,
        jwks_uri = ?metadata.jwks_uri,
        introspection = ?metadata.introspection_endpoint,
        "Discovered auth server metadata (OIDC)"
    );

    Ok(metadata)
}

// ---------------------------------------------------------------------------
// RFC 7662: Token Introspection Validator
// ---------------------------------------------------------------------------

/// Token validator using RFC 7662 token introspection.
///
/// Calls the authorization server's introspection endpoint to validate
/// opaque (non-JWT) tokens. Requires OAuth client credentials.
#[derive(Clone)]
pub struct IntrospectionValidator {
    inner: Arc<IntrospectionState>,
}

struct IntrospectionState {
    introspection_endpoint: String,
    client_id: String,
    client_secret: String,
    expected_audience: Option<String>,
    http_client: reqwest::Client,
}

/// RFC 7662 introspection response.
#[derive(Debug, serde::Deserialize)]
struct IntrospectionResponse {
    /// Whether the token is active.
    active: bool,
    /// Token subject.
    #[serde(default)]
    sub: Option<String>,
    /// Token issuer.
    #[serde(default)]
    iss: Option<String>,
    /// Token audience.
    #[serde(default)]
    aud: Option<serde_json::Value>,
    /// Token expiration.
    #[serde(default)]
    exp: Option<u64>,
    /// Space-delimited scopes.
    #[serde(default)]
    scope: Option<String>,
    /// Client ID.
    #[serde(default)]
    client_id: Option<String>,
}

impl IntrospectionValidator {
    /// Create a new introspection validator.
    pub fn new(introspection_endpoint: &str, client_id: &str, client_secret: &str) -> Self {
        Self {
            inner: Arc::new(IntrospectionState {
                introspection_endpoint: introspection_endpoint.to_string(),
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
                expected_audience: None,
                http_client: reqwest::Client::new(),
            }),
        }
    }

    /// Set the expected audience for validation.
    pub fn expected_audience(mut self, audience: &str) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("no other references")
            .expected_audience = Some(audience.to_string());
        self
    }
}

impl TokenValidator for IntrospectionValidator {
    async fn validate_token(&self, token: &str) -> Result<TokenClaims, OAuthError> {
        let resp = self
            .inner
            .http_client
            .post(&self.inner.introspection_endpoint)
            .basic_auth(&self.inner.client_id, Some(&self.inner.client_secret))
            .form(&[("token", token)])
            .send()
            .await
            .map_err(|e| OAuthError::InvalidToken {
                description: format!("introspection request failed: {e}"),
            })?;

        if !resp.status().is_success() {
            return Err(OAuthError::InvalidToken {
                description: format!("introspection endpoint returned {}", resp.status()),
            });
        }

        let introspection: IntrospectionResponse =
            resp.json().await.map_err(|e| OAuthError::InvalidToken {
                description: format!("invalid introspection response: {e}"),
            })?;

        if !introspection.active {
            return Err(OAuthError::InvalidToken {
                description: "token is not active".to_string(),
            });
        }

        // Validate audience if configured
        if let Some(expected_aud) = &self.inner.expected_audience {
            let aud_matches = match &introspection.aud {
                Some(serde_json::Value::String(s)) => s == expected_aud,
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| s == expected_aud)),
                _ => true, // No audience in response; don't reject
            };
            if !aud_matches {
                return Err(OAuthError::InvalidAudience);
            }
        }

        Ok(TokenClaims {
            sub: introspection.sub,
            iss: introspection.iss,
            aud: None,
            exp: introspection.exp,
            scope: introspection.scope,
            client_id: introspection.client_id,
            extra: std::collections::HashMap::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Fallback Validator: JWT first, then introspection
// ---------------------------------------------------------------------------

/// Token validator that tries JWT validation first and falls back to introspection.
///
/// Useful when the authorization server issues both JWTs and opaque tokens.
/// JWT validation is preferred (no network call) but introspection handles
/// opaque tokens that can't be decoded as JWTs.
#[derive(Clone)]
pub struct FallbackValidator<J: TokenValidator> {
    jwt_validator: J,
    introspection_validator: IntrospectionValidator,
}

impl<J: TokenValidator> FallbackValidator<J> {
    /// Create a fallback validator that tries `jwt_validator` first,
    /// then `introspection_validator` if JWT validation fails.
    pub fn new(jwt_validator: J, introspection_validator: IntrospectionValidator) -> Self {
        Self {
            jwt_validator,
            introspection_validator,
        }
    }
}

impl<J: TokenValidator> TokenValidator for FallbackValidator<J> {
    async fn validate_token(&self, token: &str) -> Result<TokenClaims, OAuthError> {
        // Try JWT first (fast, no network call)
        match self.jwt_validator.validate_token(token).await {
            Ok(claims) => Ok(claims),
            Err(_jwt_err) => {
                // Fall back to introspection
                self.introspection_validator.validate_token(token).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_introspection_validator_creation() {
        let validator = IntrospectionValidator::new(
            "https://auth.example.com/oauth/introspect",
            "client-id",
            "client-secret",
        )
        .expected_audience("mcp-proxy");

        assert_eq!(
            validator.inner.introspection_endpoint,
            "https://auth.example.com/oauth/introspect"
        );
        assert_eq!(
            validator.inner.expected_audience.as_deref(),
            Some("mcp-proxy")
        );
    }

    #[test]
    fn test_fallback_validator_creation() {
        let jwt = IntrospectionValidator::new("https://example.com/introspect", "id", "secret");
        let introspection =
            IntrospectionValidator::new("https://example.com/introspect", "id", "secret");
        let _fallback = FallbackValidator::new(jwt, introspection);
    }
}
