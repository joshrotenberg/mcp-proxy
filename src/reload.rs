//! Hot reload: watch the config file for changes and add new backends dynamically.
//!
//! Only new backends are added. Removed or modified backends are logged as warnings
//! since the proxy currently supports add-only dynamic updates.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::process::Command;
use tower_mcp::proxy::McpProxy;

use crate::config::{BackendConfig, GatewayConfig, TransportType};

/// Spawn a background task that watches the config file and adds new backends.
pub fn spawn_config_watcher(config_path: PathBuf, proxy: McpProxy) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("hot reload runtime");
        rt.block_on(watch_loop(config_path, proxy));
    });
}

async fn watch_loop(config_path: PathBuf, proxy: McpProxy) {
    let (tx, rx) = std_mpsc::channel();

    let mut debouncer = match new_debouncer(Duration::from_secs(2), tx) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create file watcher, hot reload disabled");
            return;
        }
    };

    if let Err(e) = debouncer
        .watcher()
        .watch(&config_path, notify::RecursiveMode::NonRecursive)
    {
        tracing::error!(
            path = %config_path.display(),
            error = %e,
            "Failed to watch config file, hot reload disabled"
        );
        return;
    }

    tracing::info!(path = %config_path.display(), "Hot reload watching config file");

    // Track known backend names
    let mut known_backends: HashSet<String> = {
        if let Ok(config) = GatewayConfig::load(&config_path) {
            config.backends.iter().map(|b| b.name.clone()).collect()
        } else {
            HashSet::new()
        }
    };

    loop {
        // Block until a file event arrives (this is a std::sync channel)
        let events = match rx.recv() {
            Ok(Ok(events)) => events,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "File watcher error");
                continue;
            }
            Err(_) => {
                tracing::info!("File watcher channel closed, stopping hot reload");
                break;
            }
        };

        // Only process write events
        let has_write = events
            .iter()
            .any(|e| matches!(e.kind, DebouncedEventKind::Any));
        if !has_write {
            continue;
        }

        tracing::info!("Config file changed, checking for new backends");

        let mut new_config = match GatewayConfig::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse updated config, skipping reload");
                continue;
            }
        };
        new_config.resolve_env_vars();

        let new_names: HashSet<String> =
            new_config.backends.iter().map(|b| b.name.clone()).collect();

        // Detect removed backends
        for removed in known_backends.difference(&new_names) {
            tracing::warn!(
                backend = %removed,
                "Backend removed from config but cannot be removed at runtime (add-only)"
            );
        }

        // Detect modified backends (name exists but config may have changed)
        // We don't track config content, so we can't detect modifications.
        // Just log that existing backends are not updated.

        // Add new backends
        for backend in &new_config.backends {
            if known_backends.contains(&backend.name) {
                continue;
            }

            tracing::info!(
                name = %backend.name,
                transport = ?backend.transport,
                "Adding new backend via hot reload"
            );

            if let Err(e) = add_backend(&proxy, backend).await {
                tracing::error!(
                    backend = %backend.name,
                    error = %e,
                    "Failed to add backend via hot reload"
                );
            } else {
                tracing::info!(backend = %backend.name, "Backend added via hot reload");
                known_backends.insert(backend.name.clone());
            }
        }
    }
}

/// Connect and add a single backend to the proxy.
async fn add_backend(proxy: &McpProxy, backend: &BackendConfig) -> anyhow::Result<()> {
    match backend.transport {
        TransportType::Stdio => {
            let command = backend
                .command
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("stdio backend requires 'command'"))?;
            let args: Vec<&str> = backend.args.iter().map(|s| s.as_str()).collect();

            let mut cmd = Command::new(command);
            cmd.args(&args);
            for (key, value) in &backend.env {
                cmd.env(key, value);
            }

            let transport =
                tower_mcp::client::StdioClientTransport::spawn_command(&mut cmd).await?;
            proxy
                .add_backend(&backend.name, transport)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }
        TransportType::Http => {
            let url = backend
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("http backend requires 'url'"))?;
            let transport = tower_mcp::client::HttpClientTransport::new(url);
            proxy
                .add_backend(&backend.name, transport)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }
    }

    // Log warnings for per-backend middleware that won't be applied
    if backend.timeout.is_some()
        || backend.circuit_breaker.is_some()
        || backend.rate_limit.is_some()
        || backend.concurrency.is_some()
    {
        tracing::warn!(
            backend = %backend.name,
            "Per-backend middleware (timeout, circuit_breaker, rate_limit, concurrency) \
             is not applied to hot-reloaded backends"
        );
    }

    Ok(())
}
