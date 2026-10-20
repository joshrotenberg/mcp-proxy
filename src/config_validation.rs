//! Offline structural/runtime bounds shared by every configuration entry point.
use anyhow::{Result, ensure};
use std::collections::HashSet;

use crate::config::{
    AuthConfig, BackendCacheConfig, CacheBackendConfig, NameFilter, ProxyConfig, TransportType,
};

// Finite operational limits avoid timer/expiry overflow and eager allocations
// in middleware dependencies. These are configuration limits, not request quotas.
const MAX_SECONDS: u64 = 31_536_000;
const MAX_COUNT: usize = 1_000_000;

fn seconds(value: u64, field: &str, zero_allowed: bool) -> Result<()> {
    ensure!(zero_allowed || value > 0, "{field} must be > 0");
    ensure!(
        value <= MAX_SECONDS,
        "{field} must be between {} and {MAX_SECONDS} seconds",
        if zero_allowed { 0 } else { 1 }
    );
    Ok(())
}
fn count(value: usize, field: &str) -> Result<()> {
    ensure!(
        (1..=MAX_COUNT).contains(&value),
        "{field} must be > 0 and <= {MAX_COUNT}"
    );
    Ok(())
}
fn nonempty(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && !value.chars().any(char::is_control),
        "{field} must be nonempty and contain no control characters"
    );
    Ok(())
}
fn local(value: &str, separator: &str, field: &str) -> Result<()> {
    nonempty(value, field)?;
    ensure!(
        !value.chars().any(char::is_whitespace) && !value.contains(separator),
        "{field} must not contain the namespace separator or whitespace"
    );
    Ok(())
}
fn endpoint(value: &str, schemes: &[&str], field: &str) -> Result<()> {
    // Never include the value or parser error: URLs can carry credentials.
    let valid = value.trim() == value
        && !value.chars().any(char::is_control)
        && url::Url::parse(value)
            .is_ok_and(|u| schemes.contains(&u.scheme()) && u.has_host() && u.fragment().is_none());
    ensure!(
        valid,
        "{field} must be an absolute URL with a supported scheme and host, without a fragment"
    );
    Ok(())
}

pub(crate) fn cache_backend(config: &CacheBackendConfig) -> Result<()> {
    match config.backend.as_str() {
        "memory" => {}
        "redis" => {
            let url = config.url.as_deref().ok_or_else(|| {
                anyhow::anyhow!("cache.url is required when cache.backend = \"redis\"")
            })?;
            // Redis parses all its supported URL forms (including Unix sockets)
            // without connecting. Do not narrow its existing transport contract.
            #[cfg(feature = "redis-cache")]
            redis::Client::open(url)
                .map_err(|_| anyhow::anyhow!("cache.url is not a valid Redis connection URL"))?;
            #[cfg(not(feature = "redis-cache"))]
            {
                let _ = url;
                anyhow::bail!("cache.backend = \"redis\" requires the 'redis-cache' feature");
            }
        }
        "sqlite" => {
            let path = config.url.as_deref().ok_or_else(|| {
                anyhow::anyhow!("cache.url is required when cache.backend = \"sqlite\"")
            })?;
            nonempty(path, "cache.url")?;
            #[cfg(not(feature = "sqlite-cache"))]
            anyhow::bail!("cache.backend = \"sqlite\" requires the 'sqlite-cache' feature");
        }
        _ => anyhow::bail!("unknown cache backend, expected \"memory\", \"redis\", or \"sqlite\""),
    }
    if let Some(generation) = &config.generation {
        nonempty(generation, "cache.generation")?;
    }
    Ok(())
}

pub(crate) fn cache_policy(config: &BackendCacheConfig) -> Result<()> {
    seconds(
        config.resource_ttl_seconds,
        "backend cache.resource_ttl_seconds",
        true,
    )?;
    seconds(
        config.tool_ttl_seconds,
        "backend cache.tool_ttl_seconds",
        true,
    )?;
    ensure!(
        (1..=MAX_COUNT as u64).contains(&config.max_entries),
        "backend cache.max_entries must be > 0 and <= {MAX_COUNT}"
    );
    Ok(())
}

pub(crate) fn validate(config: &ProxyConfig) -> Result<()> {
    nonempty(&config.proxy.name, "proxy.name")?;
    nonempty(&config.proxy.version, "proxy.version")?;
    nonempty(&config.proxy.separator, "proxy.separator")?;
    ensure!(
        !config.proxy.separator.chars().any(char::is_whitespace),
        "proxy.separator must contain no whitespace"
    );
    let host = &config.proxy.listen.host;
    let ip = host.parse::<std::net::IpAddr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|s| s.parse::<std::net::Ipv6Addr>().is_ok());
    let dns = host.strip_suffix('.').unwrap_or(host);
    ensure!(
        ip || (dns.len() <= 253
            && dns.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })),
        "proxy.listen.host must be an IP address or DNS hostname without a port"
    );
    seconds(
        config.proxy.shutdown_timeout_seconds,
        "proxy.shutdown_timeout_seconds",
        true,
    )?;
    cache_backend(&config.cache)?;
    if let Some(rate) = &config.proxy.rate_limit {
        count(rate.requests, "proxy.rate_limit.requests")?;
        seconds(
            rate.period_seconds,
            "proxy.rate_limit.period_seconds",
            false,
        )?;
    }
    count(
        config.performance.coalesce.max_entries,
        "coalesce.max_entries",
    )?;
    count(
        config.performance.coalesce.max_waiters,
        "coalesce.max_waiters",
    )?;

    for backend in &config.backends {
        local(&backend.name, &config.proxy.separator, "backend.name")?;
        ensure!(
            backend.name != "proxy",
            "backend.name 'proxy' is reserved for management and discovery"
        );
        match backend.transport {
            TransportType::Stdio => {
                let command = backend.command.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "backend '{}': stdio transport requires 'command'",
                        backend.name
                    )
                })?;
                nonempty(command, "backend.command")?;
                ensure!(
                    !backend.args.iter().any(|arg| arg.contains('\0')),
                    "backend.args must contain no NUL bytes"
                );
                for (key, value) in &backend.env {
                    ensure!(
                        !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
                        "backend.env contains an invalid process environment entry"
                    );
                }
            }
            TransportType::Http | TransportType::Websocket => {
                let url = backend.url.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "backend '{}': {} transport requires 'url'",
                        backend.name,
                        if matches!(backend.transport, TransportType::Http) {
                            "http"
                        } else {
                            "websocket"
                        }
                    )
                })?;
                let schemes: &[&str] = if matches!(backend.transport, TransportType::Http) {
                    &["http", "https"]
                } else {
                    &["ws", "wss"]
                };
                endpoint(url, schemes, "backend.url")?;
            }
        }
        // Check every list, including filters hidden by canary/failover policy.
        for patterns in [
            &backend.expose_tools,
            &backend.hide_tools,
            &backend.expose_resources,
            &backend.hide_resources,
            &backend.expose_prompts,
            &backend.hide_prompts,
        ] {
            NameFilter::allow_list(patterns.iter().cloned()).map_err(|_| {
                anyhow::anyhow!(
                    "backend '{}': invalid capability filter pattern",
                    backend.name
                )
            })?;
        }
        if let Some(timeout) = &backend.timeout {
            seconds(timeout.seconds, "backend.timeout.seconds", false)?;
        }
        if let Some(rate) = &backend.rate_limit {
            count(rate.requests, "backend.rate_limit.requests")?;
            seconds(
                rate.period_seconds,
                "backend.rate_limit.period_seconds",
                false,
            )?;
        }
        if let Some(concurrency) = &backend.concurrency {
            count(
                concurrency.max_concurrent,
                "backend.concurrency.max_concurrent",
            )?;
        }
        if let Some(breaker) = &backend.circuit_breaker {
            count(
                breaker.minimum_calls,
                "backend.circuit_breaker.minimum_calls",
            )?;
            count(
                breaker.permitted_calls_in_half_open,
                "backend.circuit_breaker.permitted_calls_in_half_open",
            )?;
            seconds(
                breaker.wait_duration_seconds,
                "backend.circuit_breaker.wait_duration_seconds",
                false,
            )?;
        }
        if let Some(outlier) = &backend.outlier_detection {
            ensure!(
                outlier.consecutive_errors > 0,
                "backend.outlier_detection.consecutive_errors must be > 0"
            );
            ensure!(
                outlier.max_ejection_percent <= 100,
                "backend.outlier_detection.max_ejection_percent must be 0-100"
            );
            seconds(
                outlier.base_ejection_seconds,
                "backend.outlier_detection.base_ejection_seconds",
                false,
            )?;
            seconds(
                outlier.interval_seconds,
                "backend.outlier_detection.interval_seconds",
                false,
            )?;
        }
        if let Some(hedge) = &backend.hedging {
            ensure!(
                hedge.max_hedges <= 16,
                "backend.hedging.max_hedges must be 0-16"
            );
            ensure!(
                hedge.delay_ms <= MAX_SECONDS * 1000,
                "backend.hedging.delay_ms must be <= {}",
                MAX_SECONDS * 1000
            );
        }
        if let Some(cache) = &backend.cache {
            cache_policy(cache)?;
        }

        let mut sources = HashSet::new();
        let mut targets = HashSet::new();
        for alias in &backend.aliases {
            local(&alias.from, &config.proxy.separator, "backend.aliases.from")?;
            local(&alias.to, &config.proxy.separator, "backend.aliases.to")?;
            ensure!(
                sources.insert(&alias.from) && targets.insert(&alias.to),
                "backend aliases must have unique sources and targets"
            );
        }
        for rule in &backend.inject_args {
            nonempty(&rule.tool, "backend.inject_args.tool")?;
        }
        for rule in &backend.param_overrides {
            for (from, to) in &rule.rename {
                nonempty(from, "backend.param_overrides.rename source")?;
                nonempty(to, "backend.param_overrides.rename target")?;
                ensure!(
                    from == to || !rule.rename.contains_key(to),
                    "backend.param_overrides.rename must not chain or cycle"
                );
            }
        }
    }

    // Multi-character separators can overlap across a name/separator boundary
    // even when neither name contains the separator. Include internal routing.
    let mut prefixes: Vec<_> = config
        .backends
        .iter()
        .map(|b| format!("{}{}", b.name, config.proxy.separator))
        .collect();
    prefixes.push(format!("proxy{}", config.proxy.separator));
    prefixes.sort();
    prefixes.dedup(); // Duplicate names retain the existing dedicated diagnostic.
    for pair in prefixes.windows(2) {
        ensure!(
            !pair[1].starts_with(&pair[0]),
            "backend namespace prefixes must not overlap each other or the reserved proxy namespace"
        );
    }

    // Mirror/canary maps store one destination per source; duplicates otherwise
    // silently overwrite each other. Failover deliberately supports many peers.
    for field in ["mirror_of", "canary_of"] {
        let mut seen = HashSet::new();
        for backend in &config.backends {
            let source = if field == "mirror_of" {
                &backend.mirror_of
            } else {
                &backend.canary_of
            };
            if let Some(source) = source {
                ensure!(
                    seen.insert(source),
                    "multiple backends declare {field} for the same source"
                );
            }
        }
    }
    for backend in &config.backends {
        let mut edges = HashSet::new();
        let mut current = backend;
        while let Some(parent) = current
            .failover_for
            .as_ref()
            .or(current.canary_of.as_ref())
            .or(current.mirror_of.as_ref())
        {
            for (field, target) in [
                ("mirror_of", &current.mirror_of),
                ("failover_for", &current.failover_for),
                ("canary_of", &current.canary_of),
            ] {
                ensure!(
                    target.as_ref() != Some(&current.name),
                    "{field} cannot reference itself"
                );
            }
            ensure!(
                edges.insert(&current.name),
                "backend routing references must not form a cycle"
            );
            let Some(next) = config.backends.iter().find(|b| &b.name == parent) else {
                break;
            };
            current = next;
        }
        ensure!(
            [
                &backend.failover_for,
                &backend.canary_of,
                &backend.mirror_of
            ]
            .into_iter()
            .filter(|v| v.is_some())
            .count()
                <= 1,
            "backend must select only one routing role"
        );
        if backend.canary_of.is_some()
            && let Some(primary) = config
                .backends
                .iter()
                .find(|b| Some(&b.name) == backend.canary_of.as_ref())
        {
            ensure!(
                (1..=100).contains(&primary.weight),
                "canary primary weight must be 1-100"
            );
        }
    }
    for composite in &config.composite_tools {
        nonempty(&composite.name, "composite_tools.name")?;
        ensure!(
            !composite
                .name
                .starts_with(&format!("proxy{}", config.proxy.separator))
                && !config.backends.iter().any(|b| composite
                    .name
                    .starts_with(&format!("{}{}", b.name, config.proxy.separator))),
            "composite_tools.name collides with a backend or reserved namespace"
        );
        for tool in &composite.tools {
            ensure!(
                tool.strip_prefix(&format!("proxy{}", config.proxy.separator))
                    .is_some_and(|local| !local.trim().is_empty())
                    || config.backends.iter().any(|b| tool
                        .strip_prefix(&format!("{}{}", b.name, config.proxy.separator))
                        .is_some_and(|local| !local.trim().is_empty())),
                "composite_tools target must reference a configured backend or built-in proxy tool"
            );
        }
    }

    match &config.auth {
        Some(AuthConfig::Jwt { jwks_uri, .. }) => {
            endpoint(jwks_uri, &["http", "https"], "auth.jwks_uri")?;
        }
        Some(AuthConfig::OAuth {
            jwks_uri,
            introspection_endpoint,
            client_id,
            ..
        }) => {
            if let Some(uri) = jwks_uri {
                endpoint(uri, &["http", "https"], "auth.jwks_uri")?;
            }
            if let Some(uri) = introspection_endpoint {
                endpoint(uri, &["http", "https"], "auth.introspection_endpoint")?;
            }
            if let Some(id) = client_id {
                nonempty(id, "auth.client_id")?;
            }
        }
        _ => {}
    }
    if config.observability.tracing.enabled {
        endpoint(
            &config.observability.tracing.endpoint,
            &["http", "https"],
            "observability.tracing.endpoint",
        )?;
        nonempty(
            &config.observability.tracing.service_name,
            "observability.tracing.service_name",
        )?;
    }
    tracing_subscriber::EnvFilter::try_new(&config.observability.log_level)
        .map_err(|_| anyhow::anyhow!("observability.log_level must be a valid tracing filter"))?;
    Ok(())
}

pub(crate) fn validate_features(config: &ProxyConfig) -> Result<()> {
    let _ = config;
    // The forwarding middleware is compiled with OAuth, even for plain bearer
    // auth. Its complete transport/security contract is tracked in #288.
    #[cfg(not(feature = "oauth"))]
    ensure!(
        !config.backends.iter().any(|backend| backend.forward_auth),
        "backend.forward_auth requires the 'oauth' feature"
    );
    #[cfg(not(feature = "oauth"))]
    match &config.auth {
        Some(AuthConfig::Jwt { .. } | AuthConfig::OAuth { .. }) => {
            anyhow::bail!("JWT/OAuth auth requires the 'oauth' feature")
        }
        Some(AuthConfig::Bearer { scoped_tokens, .. }) if !scoped_tokens.is_empty() => {
            anyhow::bail!("per-token tool scoping requires the 'oauth' feature")
        }
        _ => {}
    }
    #[cfg(not(feature = "discovery"))]
    ensure!(
        !config.proxy.tool_discovery,
        "proxy.tool_discovery requires the 'discovery' feature"
    );
    #[cfg(not(feature = "otel"))]
    ensure!(
        !config.observability.tracing.enabled,
        "observability.tracing requires the 'otel' feature"
    );
    #[cfg(not(feature = "metrics"))]
    ensure!(
        !config.observability.metrics.enabled,
        "observability.metrics requires the 'metrics' feature"
    );
    Ok(())
}
