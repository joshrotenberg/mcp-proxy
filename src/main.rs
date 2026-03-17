use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use mcp_proxy::Proxy;
use mcp_proxy::config::ProxyConfig;

#[derive(Parser)]
#[command(name = "mcp-proxy", about = "Standalone MCP proxy")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "proxy.toml")]
    config: PathBuf,
    /// Validate the config file and exit without starting the server
    #[arg(long)]
    check: bool,
    /// Import backends from a .mcp.json file (merges with config file backends)
    #[arg(long, value_name = "PATH")]
    import_mcp_json: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = ProxyConfig::load(&cli.config)?;

    // Import backends from .mcp.json if specified
    if let Some(ref mcp_json_path) = cli.import_mcp_json {
        let mcp_json = mcp_proxy::mcp_json::McpJsonConfig::load(mcp_json_path)?;
        let imported = mcp_json.into_backends()?;
        let count = imported.len();
        for backend in imported {
            // Skip if a backend with this name already exists in the TOML config
            if config.backends.iter().any(|b| b.name == backend.name) {
                eprintln!(
                    "  Skipping '{}' from .mcp.json (already defined in config)",
                    backend.name
                );
                continue;
            }
            config.backends.push(backend);
        }
        eprintln!(
            "Imported {} backends from {}",
            count,
            mcp_json_path.display()
        );
    }

    if cli.check {
        // Check for unset env vars before resolving them
        let env_warnings = config.check_env_vars();
        config.resolve_env_vars();
        let result = print_config_summary(&config);
        if !env_warnings.is_empty() {
            println!();
            println!("  Warnings:");
            for warning in &env_warnings {
                println!("    - {}", warning);
            }
        }
        return result;
    }

    config.resolve_env_vars();

    init_logging(&config);

    tracing::info!(
        name = %config.proxy.name,
        version = %config.proxy.version,
        backends = config.backends.len(),
        "Starting MCP proxy"
    );

    let hot_reload = config.proxy.hot_reload;

    let proxy = Proxy::from_config(config).await?;

    if hot_reload {
        proxy.enable_hot_reload(cli.config.clone());
    }

    proxy.serve().await
}

fn print_config_summary(config: &ProxyConfig) -> Result<()> {
    println!("Config OK");
    println!();
    println!(
        "  Proxy:    {} v{}",
        config.proxy.name, config.proxy.version
    );
    println!(
        "  Listen:   {}:{}",
        config.proxy.listen.host, config.proxy.listen.port
    );
    println!("  Backends: {}", config.backends.len());

    for backend in &config.backends {
        let transport = match backend.transport {
            mcp_proxy::config::TransportType::Stdio => "stdio",
            mcp_proxy::config::TransportType::Http => "http",
            mcp_proxy::config::TransportType::Websocket => "websocket",
        };

        let mut features = Vec::new();
        if backend.timeout.is_some() {
            features.push("timeout");
        }
        if backend.rate_limit.is_some() {
            features.push("rate-limit");
        }
        if backend.circuit_breaker.is_some() {
            features.push("circuit-breaker");
        }
        if backend.retry.is_some() {
            features.push("retry");
        }
        if backend.hedging.is_some() {
            features.push("hedging");
        }
        if backend.concurrency.is_some() {
            features.push("concurrency-limit");
        }
        if backend.outlier_detection.is_some() {
            features.push("outlier-detection");
        }
        if backend.cache.is_some() {
            features.push("cache");
        }
        if !backend.expose_tools.is_empty() || !backend.hide_tools.is_empty() {
            features.push("filter");
        }
        if !backend.aliases.is_empty() {
            features.push("alias");
        }
        if backend.canary_of.is_some() {
            features.push("canary");
        }
        if backend.mirror_of.is_some() {
            features.push("mirror");
        }

        let features_str = if features.is_empty() {
            String::new()
        } else {
            format!(" [{}]", features.join(", "))
        };
        println!("    - {} ({}){}", backend.name, transport, features_str);
    }

    let auth_str = match &config.auth {
        Some(mcp_proxy::config::AuthConfig::Bearer {
            tokens,
            scoped_tokens,
        }) => {
            let total = tokens.len() + scoped_tokens.len();
            let scoped = if scoped_tokens.is_empty() {
                String::new()
            } else {
                format!(", {} scoped", scoped_tokens.len())
            };
            format!("bearer ({} tokens{})", total, scoped)
        }
        #[cfg(feature = "oauth")]
        Some(mcp_proxy::config::AuthConfig::Jwt { .. }) => "jwt/jwks".to_string(),
        #[cfg(not(feature = "oauth"))]
        Some(mcp_proxy::config::AuthConfig::Jwt { .. }) => {
            "jwt/jwks (feature disabled)".to_string()
        }
        #[cfg(feature = "oauth")]
        Some(mcp_proxy::config::AuthConfig::OAuth {
            token_validation, ..
        }) => format!("oauth 2.1 ({token_validation:?})"),
        #[cfg(not(feature = "oauth"))]
        Some(mcp_proxy::config::AuthConfig::OAuth { .. }) => {
            "oauth 2.1 (feature disabled)".to_string()
        }
        None => "none".to_string(),
    };
    println!("  Auth:     {}", auth_str);

    if let Some(ref rl) = config.proxy.rate_limit {
        println!("  Rate limit: {} req/{}s", rl.requests, rl.period_seconds);
    }
    if config.cache.backend != "memory" {
        println!(
            "  Cache:    {} ({})",
            config.cache.backend,
            config.cache.url.as_deref().unwrap_or("n/a")
        );
    }
    if config.proxy.hot_reload {
        println!("  Hot reload: enabled");
    }
    if config.performance.coalesce_requests {
        println!("  Request coalescing: enabled");
    }
    if config.observability.audit {
        println!("  Audit logging: enabled");
    }
    if config.observability.metrics.enabled {
        println!("  Metrics: enabled");
    }

    Ok(())
}

fn init_logging(config: &ProxyConfig) {
    let env_filter = format!(
        "tower_mcp={level},mcp_proxy={level}",
        level = config.observability.log_level
    );

    #[cfg(feature = "otel")]
    if config.observability.tracing.enabled {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_otlp::WithExportConfig;
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&config.observability.tracing.endpoint)
            .build()
            .expect("building OTLP span exporter");

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name(config.observability.tracing.service_name.clone())
                    .build(),
            )
            .build();

        let tracer = provider.tracer("mcp-proxy");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        let fmt_layer = if config.observability.json_logs {
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .boxed()
        } else {
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .boxed()
        };

        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(&env_filter))
            .with(fmt_layer)
            .with(otel_layer)
            .init();

        tracing::info!(
            endpoint = %config.observability.tracing.endpoint,
            service_name = %config.observability.tracing.service_name,
            "OpenTelemetry tracing enabled"
        );
        return;
    }

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr);

    if config.observability.json_logs {
        subscriber.json().init();
    } else {
        subscriber.init();
    }
}
