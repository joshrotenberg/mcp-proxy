//! OAuth 2.1 token introspection and authorization server discovery.
//!
//! This module implements two complementary OAuth standards for token
//! validation in environments where local JWT verification alone is
//! insufficient:
//!
//! - **RFC 8414** -- Authorization server metadata discovery. The proxy
//!   fetches the issuer's `.well-known/oauth-authorization-server` (or
//!   `.well-known/openid-configuration` as a fallback) to auto-discover
//!   the JWKS URI, introspection endpoint, and other server capabilities.
//!   See [`discover_auth_server`] and [`AuthServerMetadata`].
//!
//! - **RFC 7662** -- Token introspection. The proxy calls the authorization
//!   server's introspection endpoint with client credentials to validate
//!   opaque (non-JWT) tokens at request time. See [`IntrospectionValidator`].
//!
//! # Validation strategies
//!
//! The [`FallbackValidator`] combines both approaches: it attempts fast,
//! local JWT validation first (no network call), and falls back to
//! introspection only when JWT decoding fails. This is the recommended
//! strategy when the authorization server issues both JWT and opaque tokens.
//!
//! | Strategy | Type | Network per request | Use when |
//! |---|---|---|---|
//! | JWT only | [`TokenValidator`] | No | All tokens are JWTs |
//! | Introspection only | [`IntrospectionValidator`] | Yes | Opaque tokens, real-time revocation |
//! | Both (fallback) | [`FallbackValidator`] | Sometimes | Mixed token types |
//!
//! The strategy is selected via the `token_validation` field in the auth
//! config: `"jwt"` (default), `"introspection"`, or `"both"`.
//!
//! # Configuration example
//!
//! ```toml
//! [auth]
//! type = "oauth"
//! issuer = "https://auth.example.com"
//! audience = "https://mcp.example.com/mcp"
//! client_id = "mcp-proxy-client"
//! client_secret = "${OAUTH_CLIENT_SECRET}"
//! token_validation = "both"
//!
//! # Optional: override auto-discovered endpoints
//! # jwks_uri = "https://auth.example.com/custom/jwks"
//! # introspection_endpoint = "https://auth.example.com/custom/introspect"
//! ```
//!
//! When `token_validation` is `"introspection"` or `"both"`, `client_id` and
//! `client_secret` are required (the proxy authenticates to the introspection
//! endpoint using HTTP Basic auth with these credentials).
//!
//! # Discovery flow
//!
//! [`discover_auth_server`] performs metadata discovery in two steps:
//!
//! 1. Insert `/.well-known/oauth-authorization-server` before the issuer path
//!    (RFC 8414).
//! 2. If that fails, fall back to `{issuer}/.well-known/openid-configuration`
//!    (OpenID Connect Discovery).
//!
//! The returned [`AuthServerMetadata`] provides the JWKS URI and introspection
//! endpoint used to construct validators at proxy startup. Discovery runs only
//! for strategy-required endpoints that were not explicitly configured. A
//! discovered issuer must exactly match the configured identifier. Each HTTP
//! operation has a five-second deadline, redirects are refused, and discovery/
//! introspection response bodies are limited to 64 KiB.

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
/// Uses the path-aware RFC 8414 well-known URL, then falls back to the issuer's
/// `/.well-known/openid-configuration` URL. Returned issuer identity must match
/// exactly; an issuer mismatch never triggers fallback. Each request, including
/// its body, has a five-second deadline and a 64 KiB body limit.
pub async fn discover_auth_server(issuer: &str) -> anyhow::Result<AuthServerMetadata> {
    let parsed = crate::config::validate_oauth_identity(issuer, "auth.issuer")?;
    let client = auth_http_client();
    let mut rfc8414 = parsed.clone();
    rfc8414.set_path(&format!(
        "/.well-known/oauth-authorization-server{}",
        parsed.path().trim_end_matches('/')
    ));
    let oidc = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let mut last_error = None;
    for endpoint in [rfc8414.as_str(), &oidc] {
        let response = match client.get(endpoint).send().await {
            Ok(response) => response,
            Err(_) => {
                last_error = Some(anyhow::anyhow!("auth server discovery request failed"));
                continue;
            }
        };
        if !response.status().is_success() {
            last_error = Some(anyhow::anyhow!(
                "auth server discovery failed: HTTP {}",
                response.status()
            ));
            continue;
        }
        let metadata: AuthServerMetadata = match bounded_json(response).await {
            Ok(metadata) => metadata,
            Err(_) => {
                last_error = Some(anyhow::anyhow!("failed to parse auth server metadata"));
                continue;
            }
        };
        // A different issuer is a trust failure, not a reason to try another URL.
        if metadata.issuer != issuer {
            anyhow::bail!("discovered authorization server issuer does not match auth.issuer");
        }
        tracing::info!(
            has_jwks = metadata.jwks_uri.is_some(),
            has_introspection = metadata.introspection_endpoint.is_some(),
            "Discovered auth server metadata"
        );
        return Ok(metadata);
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("auth server discovery failed")))
}

// Each discovery/introspection operation has a deadline, including body reads.
// Redirects are refused so credentials cannot be forwarded to a redirect target.
pub(crate) fn auth_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("static OAuth HTTP client configuration")
}

async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, ()> {
    const MAX_BYTES: usize = 64 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BYTES as u64)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > MAX_BYTES.saturating_sub(bytes.len()) {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
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
    expected_issuer: Option<String>,
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
                expected_issuer: None,
                http_client: auth_http_client(),
            }),
        }
    }

    /// Bind introspection claims to this configured issuer.
    ///
    /// A returned `iss` must match exactly. When RFC 7662 omits this optional
    /// claim, the configured trusted introspection authority supplies it for
    /// stable session ownership.
    pub fn expected_issuer(mut self, issuer: &str) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("no other references")
            .expected_issuer = Some(issuer.to_string());
        self
    }

    /// Set the expected audience for validation.
    pub fn expected_audience(mut self, audience: &str) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("no other references")
            .expected_audience = Some(audience.to_string());
        self
    }
}

/// Check whether an introspection response's `aud` satisfies the configured
/// expected audience.
///
/// When no audience is expected, validation always passes. When an audience is
/// expected, the response `aud` must contain it (as a string, or as an element
/// of a string array). If an audience is expected but the response omits `aud`
/// (or carries an unexpected type), this fails closed rather than accept a
/// token that may have been issued for a different resource.
fn audience_matches(expected: Option<&str>, aud: Option<&serde_json::Value>) -> bool {
    let Some(expected) = expected else {
        return true; // No expected audience configured; don't reject.
    };
    match aud {
        Some(serde_json::Value::String(s)) => s == expected,
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s == expected)),
        _ => false,
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
            .map_err(|_| OAuthError::InvalidToken {
                description: "introspection request failed".to_string(),
            })?;

        if !resp.status().is_success() {
            return Err(OAuthError::InvalidToken {
                description: format!("introspection endpoint returned {}", resp.status()),
            });
        }

        let introspection: IntrospectionResponse =
            bounded_json(resp)
                .await
                .map_err(|_| OAuthError::InvalidToken {
                    description: "invalid introspection response".to_string(),
                })?;

        if !introspection.active {
            return Err(OAuthError::InvalidToken {
                description: "token is not active".to_string(),
            });
        }

        // Validate audience if configured
        if !audience_matches(
            self.inner.expected_audience.as_deref(),
            introspection.aud.as_ref(),
        ) {
            return Err(OAuthError::InvalidAudience);
        }

        let aud = introspection
            .aud
            .map(serde_json::from_value::<tower_mcp::oauth::token::TokenAudience>)
            .transpose()
            .map_err(|_| OAuthError::InvalidAudience)?;
        if let Some(expected) = &self.inner.expected_issuer
            && introspection
                .iss
                .as_ref()
                .is_some_and(|issuer| issuer != expected)
        {
            return Err(OAuthError::InvalidToken {
                description: "introspection issuer does not match".into(),
            });
        }
        if introspection.exp.is_some_and(|exp| {
            exp <= std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }) {
            return Err(OAuthError::ExpiredToken);
        }
        Ok(TokenClaims {
            sub: introspection.sub,
            iss: introspection
                .iss
                .or_else(|| self.inner.expected_issuer.clone()),
            aud,
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
    fn test_audience_missing_when_expected_is_rejected() {
        // expected_audience set + response has no `aud` -> rejected (fail closed)
        assert!(!audience_matches(Some("mcp-proxy"), None));
        // An unexpected `aud` type is likewise rejected.
        assert!(!audience_matches(
            Some("mcp-proxy"),
            Some(&serde_json::Value::Null)
        ));
    }

    #[test]
    fn test_audience_match_when_expected_is_accepted() {
        // expected_audience set + response `aud` string matches -> accepted
        assert!(audience_matches(
            Some("mcp-proxy"),
            Some(&serde_json::json!("mcp-proxy"))
        ));
        // expected_audience set + response `aud` array contains it -> accepted
        assert!(audience_matches(
            Some("mcp-proxy"),
            Some(&serde_json::json!(["other", "mcp-proxy"]))
        ));
        // expected_audience set + response `aud` does not match -> rejected
        assert!(!audience_matches(
            Some("mcp-proxy"),
            Some(&serde_json::json!("someone-else"))
        ));
    }

    #[test]
    fn test_audience_not_expected_accepts_missing_aud() {
        // expected_audience NOT set + response has no `aud` -> accepted (unchanged)
        assert!(audience_matches(None, None));
        // No expected audience: any `aud` value is fine.
        assert!(audience_matches(None, Some(&serde_json::json!("anything"))));
    }

    #[test]
    fn test_fallback_validator_creation() {
        let jwt = IntrospectionValidator::new("https://example.com/introspect", "id", "secret");
        let introspection =
            IntrospectionValidator::new("https://example.com/introspect", "id", "secret");
        let _fallback = FallbackValidator::new(jwt, introspection);
    }
}
