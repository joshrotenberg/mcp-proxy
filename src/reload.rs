//! Hot reload: watch the config file for changes and manage backends dynamically.
//!
//! Supports adding, removing, and reconnecting plain, unreferenced backends.
//! Startup policy is frozen: unsupported whole revisions are rejected before
//! mutation, preserving startup middleware and its shared state. Fingerprints
//! detect supported connection changes; management exposes last-attempt status.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use crate::reload_policy::ReloadPolicy;
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::process::Command;
use tower_mcp::proxy::McpProxy;

use crate::config::{BackendConfig, ProxyConfig, TransportType};

/// Owns a watcher and the runtime driving its reloaded backend transports.
///
/// Retain this handle for as long as the proxy uses those backends. Dropping it
/// cancels the watcher, releases the file watch, and stops that runtime without
/// blocking the caller. Use [`Self::shutdown`] after draining requests to wait
/// for the worker thread to exit. [`crate::Proxy`] retains its handle for you.
#[must_use = "dropping the handle stops the watcher and its reloaded transports"]
pub struct ConfigWatcher {
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWatcher {
    fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    /// Cancel the watcher and join its runtime thread without blocking Tokio.
    ///
    /// Drain proxy requests first: reloaded transport drivers stop with this
    /// runtime. Requires a Tokio runtime. Returns an error if the worker panics.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .map_err(|_| anyhow::anyhow!("joining config watcher task failed"))?
                .map_err(|_| anyhow::anyhow!("config watcher thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Spawn a watcher with its own runtime; retain the returned ownership handle.
///
/// It can be called without an active Tokio runtime. Drop the handle only when
/// finished with the proxy's reloaded backend transports. The initial resolved
/// file must describe the proxy as constructed; it becomes the admission baseline.
/// [`crate::Proxy`] instead captures its actual resolved startup configuration.
pub fn spawn_config_watcher(
    config_path: PathBuf,
    proxy: McpProxy,
    #[cfg(feature = "discovery")] discovery_index: Option<(
        crate::discovery::SharedDiscoveryIndex,
        String,
    )>,
) -> ConfigWatcher {
    spawn_config_watcher_with_schemas(
        config_path,
        proxy,
        #[cfg(feature = "discovery")]
        discovery_index
            .map(|(index, separator)| (index, crate::discovery::SchemaStore::new(), separator)),
    )
}

/// Spawn an owned watcher that also refreshes retained discovery tool schemas.
///
/// Keep the returned handle for the lifetime of the proxy's reloaded transports.
/// File events wait asynchronously so those transports keep making progress.
/// The initial resolved file must describe the constructed proxy's policy.
pub fn spawn_config_watcher_with_schemas(
    config_path: PathBuf,
    proxy: McpProxy,
    #[cfg(feature = "discovery")] discovery_index: Option<(
        crate::discovery::SharedDiscoveryIndex,
        crate::discovery::SchemaStore,
        String,
    )>,
) -> ConfigWatcher {
    let policy = load_runtime_config(&config_path).map(|config| ReloadPolicy::new(&config));
    spawn_config_watcher_with_policy(
        config_path,
        proxy,
        #[cfg(feature = "discovery")]
        discovery_index,
        policy,
    )
}

pub(crate) fn spawn_config_watcher_with_policy(
    config_path: PathBuf,
    proxy: McpProxy,
    #[cfg(feature = "discovery")] discovery_index: Option<(
        crate::discovery::SharedDiscoveryIndex,
        crate::discovery::SchemaStore,
        String,
    )>,
    policy: anyhow::Result<ReloadPolicy>,
) -> ConfigWatcher {
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let thread = std::thread::spawn(move || {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let Ok(policy) = policy else {
            tracing::error!("Cannot capture initial reload policy; hot reload disabled");
            return;
        };
        let worker_policy = policy.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("hot reload runtime");
        rt.block_on(async move {
            tokio::select! {
                biased;
                _ = cancelled => {},
                _ = watch_loop(
                    config_path,
                    proxy,
                    #[cfg(feature = "discovery")]
                    discovery_index,
                    worker_policy,
                ) => {},
            }
        });
        drop(rt);
        policy.stopped();
        tracing::info!("Config watcher stopped");
    });
    ConfigWatcher {
        cancel: Some(cancel),
        thread: Some(thread),
    }
}

async fn watch_loop(
    config_path: PathBuf,
    proxy: McpProxy,
    #[cfg(feature = "discovery")] discovery_index: Option<(
        crate::discovery::SharedDiscoveryIndex,
        crate::discovery::SchemaStore,
        String,
    )>,
    policy: ReloadPolicy,
) {
    // Only the fact of a write matters: each reload reads the newest revision.
    // One pending notification coalesces bursts without an unbounded event queue.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let initial = tx.clone();
    let callback = move |result: notify_debouncer_mini::DebounceEventResult| match result {
        Ok(events)
            if events
                .iter()
                .any(|e| matches!(e.kind, DebouncedEventKind::Any)) =>
        {
            let _ = tx.try_send(());
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(error = %error, "File watcher error"),
    };
    let mut debouncer = match new_debouncer(Duration::from_secs(2), callback) {
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

    policy.started();
    let mut backend_fingerprints = policy.fingerprints();
    // Reconcile any edit between construction and watcher registration as well.
    let _ = initial.try_send(());
    drop(initial);
    tracing::info!(path = %config_path.display(), "Hot reload watching config file");

    // Waiting yields this runtime to HTTP, WebSocket and stdio client drivers.
    while rx.recv().await.is_some() {
        tracing::info!("Config file changed, reloading backends");

        let new_config = match load_runtime_config(&config_path) {
            Ok(c) => c,
            Err(e) => {
                policy.record("invalid", false, false);
                tracing::warn!(error = %e, "Failed to parse updated config, skipping reload");
                continue;
            }
        };

        if let Err(error) = policy.check(&new_config) {
            policy.record("restart_required", false, true);
            tracing::warn!(%error, "Unsupported live config change, skipping reload");
            continue;
        }
        policy.record("pending", false, false);
        let mut failed = false;
        let new_fingerprints: HashMap<String, String> = new_config
            .backends
            .iter()
            .map(|b| (b.name.clone(), config_fingerprint(b)))
            .collect();

        let old_names: HashSet<&String> = backend_fingerprints.keys().collect();
        let new_names: HashSet<&String> = new_fingerprints.keys().collect();

        // Remove backends that are no longer in config
        for removed in old_names.difference(&new_names) {
            tracing::info!(backend = %removed, "Removing backend via hot reload");
            if proxy.remove_backend(removed).await {
                tracing::info!(backend = %removed, "Backend removed");
            } else {
                tracing::warn!(backend = %removed, "Backend not found for removal");
            }
        }

        // Add new backends
        for backend in &new_config.backends {
            if backend_fingerprints.contains_key(&backend.name) {
                // Existing backend -- check for modification
                let old_fp = &backend_fingerprints[&backend.name];
                let new_fp = &new_fingerprints[&backend.name];

                if old_fp != new_fp {
                    tracing::info!(
                        backend = %backend.name,
                        "Backend config changed, replacing via hot reload"
                    );

                    // Remove old, add new
                    proxy.remove_backend(&backend.name).await;
                    if let Err(e) = add_backend(&proxy, backend).await {
                        failed = true;
                        tracing::error!(
                            backend = %backend.name,
                            error = %e,
                            "Failed to replace backend via hot reload"
                        );
                    } else {
                        tracing::info!(backend = %backend.name, "Backend replaced");
                    }
                }
                continue;
            }

            tracing::info!(
                name = %backend.name,
                transport = ?backend.transport,
                "Adding new backend via hot reload"
            );

            if let Err(e) = add_backend(&proxy, backend).await {
                failed = true;
                tracing::error!(
                    backend = %backend.name,
                    error = %e,
                    "Failed to add backend via hot reload"
                );
            } else {
                tracing::info!(backend = %backend.name, "Backend added via hot reload");
            }
        }

        // Re-index discovery if enabled
        #[cfg(feature = "discovery")]
        if let Some((ref index, ref schemas, ref separator)) = discovery_index {
            let mut proxy_clone = proxy.clone();
            crate::discovery::reindex_with_schemas(index, schemas, &mut proxy_clone, separator)
                .await;
        }

        policy.record(if failed { "failed" } else { "applied" }, !failed, false);
        // Transactional replacement/failure retry is tracked separately in #277.
        backend_fingerprints = new_fingerprints;
    }
}

// Resolve and validate the entire revision before any backend mutation.
fn load_runtime_config(path: &std::path::Path) -> anyhow::Result<ProxyConfig> {
    let mut config = ProxyConfig::load(path)?;
    config.resolve_env_vars()?;
    Ok(config)
}

/// Generate a fingerprint for a backend config to detect changes.
/// Uses canonical JSON serialization for a stable, content-based comparison.
fn config_fingerprint(backend: &BackendConfig) -> String {
    let mut value = serde_json::to_value(backend).expect("backend object");
    value.sort_all_objects();
    serde_json::to_string(&value).expect("backend fingerprint")
}

/// Connect and add an admitted policy-free backend using startup transport builders.
async fn add_backend(proxy: &McpProxy, backend: &BackendConfig) -> anyhow::Result<()> {
    // Admission rejects every policy-bearing backend before any mutation.
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
            let transport = crate::proxy::build_http_transport(url, backend);

            proxy
                .add_backend(&backend.name, transport)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }
        #[cfg(feature = "websocket")]
        TransportType::Websocket => {
            let url = backend
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("websocket backend requires 'url'"))?;
            let transport = crate::ws_transport::WebSocketClientTransport::connect_with_headers(
                url,
                backend.bearer_token.as_deref(),
                &backend.headers,
            )
            .await?;

            proxy
                .add_backend(&backend.name, transport)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }
        #[cfg(not(feature = "websocket"))]
        TransportType::Websocket => {
            anyhow::bail!(
                "WebSocket transport requires the 'websocket' feature. \
                 Rebuild with: cargo install mcp-proxy --features websocket"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_backend(name: &str, url: &str) -> BackendConfig {
        // Parse from TOML to get all default values automatically
        let toml = format!(
            r#"
            name = "{name}"
            transport = "http"
            url = "{url}"
            "#,
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn fingerprints_ignore_header_and_environment_map_order() {
        let mut first = http_backend("api", "http://localhost:1");
        let mut second = http_backend("api", "http://localhost:1");
        for key in ["X-First", "X-Second"] {
            first.headers.insert(key.into(), "value".into());
        }
        for key in ["X-Second", "X-First"] {
            second.headers.insert(key.into(), "value".into());
        }
        for key in ["FIRST", "SECOND"] {
            first.env.insert(key.into(), "value".into());
        }
        for key in ["SECOND", "FIRST"] {
            second.env.insert(key.into(), "value".into());
        }
        assert_eq!(config_fingerprint(&first), config_fingerprint(&second));
    }

    #[test]
    fn test_config_fingerprint_stable() {
        let backend = http_backend("api", "http://localhost:8080");
        let fp1 = config_fingerprint(&backend);
        let fp2 = config_fingerprint(&backend);
        assert_eq!(fp1, fp2, "fingerprint should be stable across calls");
    }

    #[test]
    fn test_config_fingerprint_differs_on_url_change() {
        let b1 = http_backend("api", "http://localhost:8080");
        let b2 = http_backend("api", "http://localhost:9090");
        assert_ne!(
            config_fingerprint(&b1),
            config_fingerprint(&b2),
            "different URLs should produce different fingerprints"
        );
    }

    #[test]
    fn test_config_fingerprint_differs_on_name_change() {
        let b1 = http_backend("api", "http://localhost:8080");
        let b2 = http_backend("api2", "http://localhost:8080");
        assert_ne!(
            config_fingerprint(&b1),
            config_fingerprint(&b2),
            "different names should produce different fingerprints"
        );
    }

    #[test]
    fn test_config_fingerprint_differs_on_transport_change() {
        let b1 = http_backend("api", "http://localhost:8080");
        let b2: BackendConfig = toml::from_str(
            r#"
            name = "api"
            transport = "stdio"
            command = "echo"
            "#,
        )
        .unwrap();
        assert_ne!(
            config_fingerprint(&b1),
            config_fingerprint(&b2),
            "different transports should produce different fingerprints"
        );
    }

    #[test]
    fn test_config_fingerprint_differs_with_timeout() {
        let b1 = http_backend("api", "http://localhost:8080");
        let b2: BackendConfig = toml::from_str(
            r#"
            name = "api"
            transport = "http"
            url = "http://localhost:8080"
            [timeout]
            seconds = 30
            "#,
        )
        .unwrap();
        assert_ne!(
            config_fingerprint(&b1),
            config_fingerprint(&b2),
            "adding a timeout should change the fingerprint"
        );
    }

    #[test]
    fn test_fingerprint_map_detects_additions_and_removals() {
        let backends_v1 = [
            http_backend("api", "http://api:8080"),
            http_backend("db", "http://db:5432"),
        ];
        let backends_v2 = [
            http_backend("api", "http://api:8080"),
            http_backend("cache", "http://cache:6379"),
        ];

        let fp_v1: HashMap<String, String> = backends_v1
            .iter()
            .map(|b| (b.name.clone(), config_fingerprint(b)))
            .collect();
        let fp_v2: HashMap<String, String> = backends_v2
            .iter()
            .map(|b| (b.name.clone(), config_fingerprint(b)))
            .collect();

        let old_names: HashSet<&String> = fp_v1.keys().collect();
        let new_names: HashSet<&String> = fp_v2.keys().collect();

        let added: HashSet<_> = new_names.difference(&old_names).collect();
        let removed: HashSet<_> = old_names.difference(&new_names).collect();

        assert_eq!(added.len(), 1, "one backend should be added");
        assert!(added.contains(&&"cache".to_string()));
        assert_eq!(removed.len(), 1, "one backend should be removed");
        assert!(removed.contains(&&"db".to_string()));
    }

    #[test]
    fn test_fingerprint_map_detects_modifications() {
        let b_old = http_backend("api", "http://api:8080");
        let b_new = http_backend("api", "http://api:9090");

        let fp_old = config_fingerprint(&b_old);
        let fp_new = config_fingerprint(&b_new);

        assert_ne!(
            fp_old, fp_new,
            "modified backend should have a different fingerprint"
        );
    }

    // NOTE: Testing the full watch_loop is impractical in unit tests because
    // it requires real filesystem events and a running tokio runtime with
    // file watchers. The core logic (fingerprint computation and set
    // difference for add/remove/replace) is tested above via the extracted
    // config_fingerprint function and HashMap operations that mirror the
    // watch_loop implementation.
}
