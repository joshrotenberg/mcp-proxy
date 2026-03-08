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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = ProxyConfig::load(&cli.config)?;
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
