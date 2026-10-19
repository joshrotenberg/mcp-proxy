//! Strict runtime environment resolution without credential-bearing diagnostics.

use std::env::VarError;
use std::fmt;

use crate::config::{AuthConfig, ProxyConfig};

/// A redacted runtime configuration error. Environment names and values are
/// deliberately absent, including from the error's debug representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvResolutionError {
    /// Configuration field containing the invalid value or reference.
    pub field: String,
    /// Machine-readable reason for rejecting the field.
    pub kind: EnvResolutionErrorKind,
}

/// Reasons environment resolution or credential validation can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvResolutionErrorKind {
    /// A reference is malformed or embedded in another string.
    InvalidReference,
    /// The referenced variable is not set.
    Missing,
    /// The referenced variable is not valid Unicode.
    NonUnicode,
    /// The resolved reference or required secret is empty.
    Empty,
    /// The resolved value contains another unresolved reference.
    UnresolvedValue,
    /// A bearer credential contains whitespace or non-ASCII characters.
    InvalidBearer,
}

impl fmt::Display for EnvResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            EnvResolutionErrorKind::InvalidReference => {
                "expected a whole-value ${VAR_NAME} reference"
            }
            EnvResolutionErrorKind::Missing => "required environment variable is not set",
            EnvResolutionErrorKind::NonUnicode => "environment value is not valid Unicode",
            EnvResolutionErrorKind::Empty => "required value is empty",
            EnvResolutionErrorKind::UnresolvedValue => {
                "environment value contains an unresolved reference"
            }
            EnvResolutionErrorKind::InvalidBearer => {
                "bearer credential must contain only visible ASCII without whitespace"
            }
        };
        write!(f, "{}: {reason}", self.field)
    }
}

impl std::error::Error for EnvResolutionError {}

#[derive(Clone, Copy)]
enum Kind {
    Plain,
    Bearer,
    Secret,
}

struct Field<'a> {
    path: String,
    value: &'a mut String,
    kind: Kind,
}

fn fields(config: &mut ProxyConfig) -> Vec<Field<'_>> {
    let mut fields = Vec::new();
    for (i, backend) in config.backends.iter_mut().enumerate() {
        for (key, value) in &mut backend.env {
            fields.push(Field {
                path: format!("backends[{i}].env.{key}"),
                value,
                kind: Kind::Plain,
            });
        }
        if let Some(value) = &mut backend.bearer_token {
            fields.push(Field {
                path: format!("backends[{i}].bearer_token"),
                value,
                kind: Kind::Bearer,
            });
        }
        for (key, value) in &mut backend.headers {
            fields.push(Field {
                path: format!("backends[{i}].headers.{key}"),
                value,
                kind: Kind::Plain,
            });
        }
    }
    match &mut config.auth {
        Some(AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) => {
            for (i, value) in tokens.iter_mut().enumerate() {
                fields.push(Field {
                    path: format!("auth.tokens[{i}]"),
                    value,
                    kind: Kind::Bearer,
                });
            }
            for (i, token) in scoped_tokens.iter_mut().enumerate() {
                fields.push(Field {
                    path: format!("auth.scoped_tokens[{i}].token"),
                    value: &mut token.token,
                    kind: Kind::Bearer,
                });
            }
        }
        Some(AuthConfig::OAuth {
            client_secret: Some(value),
            ..
        }) => {
            fields.push(Field {
                path: "auth.client_secret".into(),
                value,
                kind: Kind::Secret,
            });
        }
        _ => {}
    }
    if let Some(value) = &mut config.security.admin_token {
        fields.push(Field {
            path: "security.admin_token".into(),
            value,
            kind: Kind::Bearer,
        });
    }
    fields
}

fn value(
    field: &Field<'_>,
    lookup: &impl Fn(&str) -> Result<String, VarError>,
) -> Result<String, EnvResolutionError> {
    let error = |kind| EnvResolutionError {
        field: field.path.clone(),
        kind,
    };
    let resolved = if field.value.contains("${") {
        let name = field
            .value
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .filter(|name| {
                !name.is_empty()
                    && name.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                    })
            })
            .ok_or_else(|| error(EnvResolutionErrorKind::InvalidReference))?;
        let resolved = lookup(name).map_err(|e| {
            error(match e {
                VarError::NotPresent => EnvResolutionErrorKind::Missing,
                VarError::NotUnicode(_) => EnvResolutionErrorKind::NonUnicode,
            })
        })?;
        if resolved.is_empty() {
            return Err(error(EnvResolutionErrorKind::Empty));
        }
        if resolved.contains("${") {
            return Err(error(EnvResolutionErrorKind::UnresolvedValue));
        }
        resolved
    } else {
        field.value.clone()
    };
    match field.kind {
        Kind::Bearer if resolved.is_empty() => return Err(error(EnvResolutionErrorKind::Empty)),
        Kind::Bearer if !resolved.bytes().all(|b| b.is_ascii_graphic()) => {
            return Err(error(EnvResolutionErrorKind::InvalidBearer));
        }
        Kind::Secret if resolved.trim().is_empty() => {
            return Err(error(EnvResolutionErrorKind::Empty));
        }
        _ => {}
    }
    Ok(resolved)
}

pub(crate) fn resolve(config: &mut ProxyConfig) -> Result<Vec<String>, EnvResolutionError> {
    resolve_with(config, |name| std::env::var(name))
}

fn resolve_with(
    config: &mut ProxyConfig,
    lookup: impl Fn(&str) -> Result<String, VarError>,
) -> Result<Vec<String>, EnvResolutionError> {
    let fields = fields(config);
    // Stage every substitution before changing any field.
    let resolved: Vec<_> = fields
        .iter()
        .map(|field| value(field, &lookup))
        .collect::<Result<_, _>>()?;
    Ok(fields
        .into_iter()
        .zip(resolved)
        .map(|(field, resolved)| std::mem::replace(field.value, resolved))
        .collect())
}

pub(crate) fn restore(config: &mut ProxyConfig, originals: Vec<String>) {
    // Validation never changes collection membership/order, so the field walk
    // visits the same slots, including HashMap entries, on rollback.
    for (field, original) in fields(config).into_iter().zip(originals) {
        *field.value = original;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProxyConfig {
        ProxyConfig::parse(
            r#"
[proxy]
name = "env-test"
[proxy.listen]
[security]
admin_token = "${ADMIN}"
[auth]
type = "bearer"
tokens = ["${TOKEN}"]
[[auth.scoped_tokens]]
token = "${SCOPED}"
allow_tools = ["api/read"]
[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:1"
bearer_token = "${BACKEND}"
headers = { X-API-Key = "${HEADER}", X-Empty = "" }
env = { API_TOKEN = "${ENV}", EMPTY_LITERAL = "" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolves_all_supported_fields_and_keeps_literals() {
        let mut config = config();
        let original = toml::to_string(&config).unwrap();
        let old = resolve_with(&mut config, |name| Ok(format!("resolved-{name}"))).unwrap();
        config.resolve_env_vars().unwrap(); // Idempotent and validates all resolved values.
        assert_eq!(
            config.security.admin_token.as_deref(),
            Some("resolved-ADMIN")
        );
        assert_eq!(
            config.backends[0].bearer_token.as_deref(),
            Some("resolved-BACKEND")
        );
        assert_eq!(config.backends[0].headers["X-API-Key"], "resolved-HEADER");
        assert_eq!(config.backends[0].headers["X-Empty"], "");
        assert_eq!(config.backends[0].env["API_TOKEN"], "resolved-ENV");
        assert_eq!(config.backends[0].env["EMPTY_LITERAL"], "");
        let Some(AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) = &config.auth
        else {
            panic!()
        };
        assert_eq!(tokens, &["resolved-TOKEN"]);
        assert_eq!(scoped_tokens[0].token, "resolved-SCOPED");
        restore(&mut config, old);
        assert_eq!(toml::to_string(&config).unwrap(), original);
    }

    #[test]
    fn resolution_errors_are_structured_redacted_and_atomic() {
        for (kind, mode) in [
            (EnvResolutionErrorKind::Missing, 0),
            (EnvResolutionErrorKind::NonUnicode, 1),
            (EnvResolutionErrorKind::Empty, 2),
            (EnvResolutionErrorKind::UnresolvedValue, 3),
            (EnvResolutionErrorKind::InvalidBearer, 4),
        ] {
            let mut config = config();
            let original = toml::to_string(&config).unwrap();
            let error = resolve_with(&mut config, |name| {
                if name != "ADMIN" {
                    return Ok(format!("secret-{name}"));
                }
                match mode {
                    0 => Err(VarError::NotPresent),
                    1 => Err(VarError::NotUnicode("sentinel-secret".into())),
                    2 => Ok(String::new()),
                    3 => Ok("sentinel-${NESTED}".into()),
                    _ => Ok("sentinel-secret\n".into()),
                }
            })
            .unwrap_err();
            assert_eq!(error.field, "security.admin_token");
            assert_eq!(error.kind, kind);
            assert!(!format!("{error:?} {error}").contains("sentinel"));
            assert_eq!(toml::to_string(&config).unwrap(), original);
        }
    }

    #[test]
    fn malformed_embedded_and_invalid_name_references_fail_closed() {
        for input in [
            "${}",
            "${1BAD}",
            "${SECRET-NAME}",
            "${SECRET",
            "Bearer ${SECRET}",
            "${SECRET}suffix",
        ] {
            let mut config = config();
            config.backends[0].bearer_token = Some(input.into());
            let error = resolve_with(&mut config, |name| {
                assert!(
                    !name.contains("SECRET"),
                    "invalid references must not be looked up"
                );
                Ok(format!("resolved-{name}"))
            })
            .unwrap_err();
            assert_eq!(error.kind, EnvResolutionErrorKind::InvalidReference);
            assert!(!format!("{error:?} {error}").contains("SECRET"));
        }
    }
}
