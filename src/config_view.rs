//! Allowlisted configuration diagnostics shared by HTTP and MCP introspection.
//!
//! New fields are redacted until explicitly reviewed here. URLs, arbitrary
//! maps, free text, commands, and argument values are deliberately excluded.

use serde_json::Value;

use crate::config::ProxyConfig;

const REDACTED: &str = "[REDACTED]";

// These paths identify diagnostic leaves, not whole configuration structs.
// `[]` visits records in arrays; unknown children of those records stay private.
const PUBLIC_PATHS: &[&str] = &[
    "proxy.name",
    "proxy.version",
    "proxy.separator",
    "proxy.listen.host",
    "proxy.listen.port",
    "proxy.shutdown_timeout_seconds",
    "proxy.hot_reload",
    "proxy.rate_limit.requests",
    "proxy.rate_limit.period_seconds",
    "proxy.tool_discovery",
    "proxy.tool_exposure",
    "auth.type",
    "auth.token_validation",
    "auth.required_scopes",
    "auth.scoped_tokens[].allow_tools",
    "auth.scoped_tokens[].deny_tools",
    "auth.roles[].name",
    "auth.roles[].allow_tools",
    "auth.roles[].deny_tools",
    "auth.role_mapping.claim",
    "auth.role_mapping.default_deny",
    "security.max_argument_size",
    "performance.coalesce_requests",
    "cache.backend",
    "cache.prefix",
    "observability.audit",
    "observability.log_level",
    "observability.json_logs",
    "observability.metrics.enabled",
    "observability.tracing.enabled",
    "observability.tracing.service_name",
    "observability.access_log.enabled",
    "composite_tools[].name",
    "composite_tools[].tools",
    "composite_tools[].strategy",
    "backends[].name",
    "backends[].transport",
    "backends[].forward_auth",
    "backends[].timeout.seconds",
    "backends[].circuit_breaker.failure_rate_threshold",
    "backends[].circuit_breaker.minimum_calls",
    "backends[].circuit_breaker.wait_duration_seconds",
    "backends[].circuit_breaker.permitted_calls_in_half_open",
    "backends[].rate_limit.requests",
    "backends[].rate_limit.period_seconds",
    "backends[].concurrency.max_concurrent",
    "backends[].retry.max_retries",
    "backends[].retry.initial_backoff_ms",
    "backends[].retry.max_backoff_ms",
    "backends[].retry.budget_percent",
    "backends[].retry.min_retries_per_sec",
    "backends[].outlier_detection.consecutive_errors",
    "backends[].outlier_detection.interval_seconds",
    "backends[].outlier_detection.base_ejection_seconds",
    "backends[].outlier_detection.max_ejection_percent",
    "backends[].hedging.delay_ms",
    "backends[].hedging.max_hedges",
    "backends[].mirror_of",
    "backends[].mirror_percent",
    "backends[].cache.resource_ttl_seconds",
    "backends[].cache.tool_ttl_seconds",
    "backends[].cache.max_entries",
    "backends[].aliases[].from",
    "backends[].aliases[].to",
    "backends[].inject_args[].tool",
    "backends[].inject_args[].overwrite",
    "backends[].param_overrides[].tool",
    "backends[].param_overrides[].hide",
    "backends[].expose_tools",
    "backends[].hide_tools",
    "backends[].expose_prompts",
    "backends[].hide_prompts",
    "backends[].hide_destructive",
    "backends[].read_only_only",
    "backends[].failover_for",
    "backends[].priority",
    "backends[].canary_of",
    "backends[].weight",
];

/// Render diagnostics, never a reusable configuration file or secret backup.
pub(crate) fn redacted_config_toml(config: &ProxyConfig) -> String {
    // Never include serialization errors: serializers can quote input values.
    serde_json::to_value(config)
        .ok()
        .map(|value| project(&value, ""))
        .and_then(|view| toml::to_string_pretty(&view).ok())
        .unwrap_or_else(|| "# Redacted configuration unavailable\n".to_string())
}

fn project(value: &Value, path: &str) -> Value {
    if PUBLIC_PATHS.contains(&path) {
        // A public leaf changing into a structured value also fails closed.
        return match value {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => value.clone(),
            Value::Array(values) if values.iter().all(Value::is_string) => value.clone(),
            _ => Value::String(REDACTED.into()),
        };
    }

    match value {
        Value::Object(fields)
            if path.is_empty()
                || PUBLIC_PATHS
                    .iter()
                    .any(|p| p.starts_with(&format!("{path}."))) =>
        {
            Value::Object(
                fields
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| {
                        let child = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        // Configuration keys are path segments, never path
                        // expressions. A future renamed/flattened field must
                        // not impersonate a public path such as `proxy.name`.
                        let projected = if key.contains(['.', '[', ']']) {
                            Value::String(REDACTED.into())
                        } else {
                            project(value, &child)
                        };
                        (key.clone(), projected)
                    })
                    .collect(),
            )
        }
        Value::Array(values)
            if PUBLIC_PATHS
                .iter()
                .any(|p| p.starts_with(&format!("{path}[]."))) =>
        {
            Value::Array(
                values
                    .iter()
                    .map(|v| project(v, &format!("{path}[]")))
                    .collect(),
            )
        }
        _ => Value::String(REDACTED.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_fields_and_arbitrary_maps_are_redacted_by_default() {
        let input = serde_json::json!({
            "future_secret": {"innocent": "sentinel-future-root"},
            "proxy.name": "sentinel-dotted-field",
            "backends[].name": "sentinel-array-field",
            "proxy": {"name": "diagnostics", "future": "sentinel-future-proxy"},
            "backends": [{"name": "api", "future": "sentinel-future-backend",
                "headers": {"ordinary": "sentinel-header"}}],
        });
        let view = project(&input, "");
        assert_eq!(view["proxy"]["name"], "diagnostics");
        assert_eq!(view["backends"][0]["name"], "api");
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("sentinel-"));
        assert_eq!(view["backends"][0]["headers"], REDACTED);
        assert_eq!(view["future_secret"], REDACTED);
    }

    #[test]
    fn public_leaves_cannot_accidentally_expose_new_nested_fields() {
        let view = project(&serde_json::json!({"credential": "sentinel"}), "proxy.name");
        assert_eq!(view, REDACTED);
    }

    #[test]
    fn oauth_and_jwt_credentials_and_urls_are_never_diagnostic_fields() {
        let mut config = ProxyConfig::parse(
            "[proxy]\nname = 'diagnostics'\n[proxy.listen]\n\
             [[backends]]\nname = 'local'\ntransport = 'stdio'\ncommand = 'sentinel-command'\n",
        )
        .unwrap();
        for auth in [
            serde_json::json!({"type": "jwt", "issuer": "sentinel-issuer",
                "audience": "sentinel-audience", "jwks_uri": "sentinel-jwks"}),
            serde_json::json!({"type": "oauth", "issuer": "sentinel-issuer",
                "audience": "sentinel-audience", "client_id": "sentinel-client-id",
                "client_secret": "sentinel-client-secret", "jwks_uri": "sentinel-jwks",
                "introspection_endpoint": "sentinel-introspection"}),
        ] {
            config.auth = Some(serde_json::from_value(auth).unwrap());
            let text = redacted_config_toml(&config);
            assert!(!text.contains("sentinel-"));
            assert!(text.contains("diagnostics"));
            assert!(text.contains(REDACTED));
            let parsed: toml::Value = toml::from_str(&text).unwrap();
            assert_eq!(parsed["auth"]["issuer"].as_str(), Some(REDACTED));
        }
    }
}
