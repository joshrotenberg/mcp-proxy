//! Hot reload: watch the config file for changes and manage backends dynamically.
//!
//! Supports adding, removing, and reconnecting plain, unreferenced backends.
//! Startup policy is frozen: unsupported whole revisions are rejected before
//! mutation, preserving startup middleware and its shared state. Fingerprints
//! detect supported connection changes. A replacement is initialized before it
//! takes over its name, and a backend counts as applied only once it serves, so
//! failures keep the previous backend and are retried with capped backoff.
//! While the watcher runs, the file owns the backend names it declares.

use std::collections::HashMap;
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
        let connections = policy.connections().child();
        let worker_connections = connections.clone();
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
                    worker_connections,
                ) => {},
            }
            connections.stop();
            connections.closed().await;
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
    connections: crate::lifecycle::Connections,
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
    // Fingerprints of what is actually serving. A backend enters this map
    // only once it is applied, so a failed step is retried on the next pass.
    let mut applied = policy.fingerprints();
    let mut desired = applied.clone();
    let mut desired_backends: Vec<BackendConfig> = Vec::new();
    let mut desired_revision = 0u64;
    let mut applied_revision = 0u64;
    let mut retry: Option<(tokio::time::Instant, u32)> = None;
    // Reconcile any edit between construction and watcher registration as well.
    let _ = initial.try_send(());
    drop(initial);
    tracing::info!(path = %config_path.display(), "Hot reload watching config file");

    // Waiting yields this runtime to HTTP, WebSocket and stdio client drivers.
    loop {
        let retry_deadline = retry.map(|(deadline, _)| deadline);
        let retrying = tokio::select! {
            event = rx.recv() => match event {
                Some(()) => false,
                None => break,
            },
            () = sleep_until(retry_deadline) => true,
        };

        let attempt = if retrying {
            tracing::info!("Retrying backends that failed to apply");
            retry.map_or(0, |(_, attempt)| attempt)
        } else {
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
            let fingerprints: HashMap<String, String> = new_config
                .backends
                .iter()
                .map(|b| (b.name.clone(), config_fingerprint(b)))
                .collect();
            if fingerprints != desired {
                desired_revision += 1;
                desired = fingerprints;
            }
            desired_backends = new_config.backends;
            // A file event starts a fresh retry schedule.
            0
        };
        policy.record("pending", false, false);

        let guard = policy.lock_mutations().await;
        policy.set_file_names(desired.keys().cloned());
        let mut changed = false;
        let mut failed = Vec::new();

        // Remove backends that are no longer in config
        let removed: Vec<String> = applied
            .keys()
            .filter(|name| !desired.contains_key(*name))
            .cloned()
            .collect();
        for name in removed {
            tracing::info!(backend = %name, "Removing backend via hot reload");
            if proxy.remove_backend(&name).await {
                tracing::info!(backend = %name, "Backend removed");
            } else {
                tracing::warn!(backend = %name, "Backend not found for removal");
            }
            applied.remove(&name);
            changed = true;
        }

        // Add new backends and replace changed ones. A replacement is
        // connected and initialized before it takes over, so a failure leaves
        // the previous backend serving.
        for backend in &desired_backends {
            let fingerprint = &desired[&backend.name];
            let previous = applied.get(&backend.name);
            if previous == Some(fingerprint) {
                continue;
            }
            let replacing = previous.is_some();
            tracing::info!(
                backend = %backend.name,
                transport = ?backend.transport,
                replacing,
                "Applying backend via hot reload"
            );
            match apply_backend(&proxy, backend, &connections).await {
                Ok(()) => {
                    applied.insert(backend.name.clone(), fingerprint.clone());
                    changed = true;
                    if replacing {
                        tracing::info!(backend = %backend.name, "Backend replaced");
                    } else {
                        tracing::info!(backend = %backend.name, "Backend added via hot reload");
                    }
                }
                Err(e) => {
                    failed.push(backend.name.clone());
                    tracing::error!(
                        backend = %backend.name,
                        error = %e,
                        replacing,
                        "Failed to apply backend via hot reload; previous state retained"
                    );
                }
            }
        }
        drop(guard);

        // Re-index discovery if enabled
        #[cfg(feature = "discovery")]
        if changed && let Some((ref index, ref schemas, ref separator)) = discovery_index {
            let mut proxy_clone = proxy.clone();
            crate::discovery::reindex_with_schemas(index, schemas, &mut proxy_clone, separator)
                .await;
        }
        #[cfg(not(feature = "discovery"))]
        let _ = changed;

        if failed.is_empty() {
            applied_revision = desired_revision;
            retry = None;
            policy.record("applied", true, false);
        } else {
            let delay = retry_delay(attempt);
            tracing::warn!(
                failed = ?failed,
                retry_in_ms = delay.as_millis() as u64,
                "Some backends failed to apply; retrying"
            );
            retry = Some((
                tokio::time::Instant::now() + delay,
                attempt.saturating_add(1),
            ));
            policy.record("failed", false, false);
        }
        policy.record_revisions(desired_revision, applied_revision, failed);
    }
}

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Delay before retry number `attempt` (from 0): doubling from one second,
/// capped at one minute.
fn retry_delay(attempt: u32) -> Duration {
    RETRY_INITIAL
        .saturating_mul(1u32 << attempt.min(6))
        .min(RETRY_MAX)
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

// Resolve and validate the entire revision before any backend mutation.
fn load_runtime_config(path: &std::path::Path) -> anyhow::Result<ProxyConfig> {
    let mut config = ProxyConfig::load(path)?;
    config.prepare()?;
    Ok(config)
}

/// Generate a fingerprint for a backend config to detect changes.
/// Uses canonical JSON serialization for a stable, content-based comparison.
fn config_fingerprint(backend: &BackendConfig) -> String {
    let mut value = serde_json::to_value(backend).expect("backend object");
    value.sort_all_objects();
    serde_json::to_string(&value).expect("backend fingerprint")
}

/// Connect, initialize and publish an admitted policy-free backend using
/// startup transport builders.
///
/// The backend takes over its name only once it is initialized: an existing
/// backend under that name keeps serving if any step fails.
async fn apply_backend(
    proxy: &McpProxy,
    backend: &BackendConfig,
    connections: &crate::lifecycle::Connections,
) -> anyhow::Result<()> {
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

            let transport = tower_mcp::client::StdioClientTransport::spawn_command(&mut cmd)
                .await?
                .close_timeout(Duration::from_millis(100));

            proxy
                .replace_backend(&backend.name, connections.wrap(transport))
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
                .replace_backend(&backend.name, connections.wrap(transport))
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
                .replace_backend(&backend.name, connections.wrap(transport))
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
    use std::collections::HashSet;

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
    fn retry_delay_doubles_from_one_second_and_caps_at_one_minute() {
        let delays: Vec<u64> = (0..9).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }

    #[test]
    fn startup_fingerprints_match_reload_fingerprints_for_the_same_config() {
        // A mismatch would replace every unchanged backend on the first edit.
        let mut config = ProxyConfig::parse(
            r#"
            [proxy]
            name = "fingerprints"
            [proxy.listen]
            [[backends]]
            name = "api"
            transport = "http"
            url = "http://localhost:1"
            [backends.headers]
            X-Second = "2"
            X-First = "1"
            [[backends]]
            name = "tool"
            transport = "stdio"
            command = "echo"
            args = ["a", "b"]
            [backends.env]
            B = "2"
            A = "1"
            "#,
        )
        .unwrap();
        config.prepare().unwrap();
        let policy = ReloadPolicy::new(&config);
        let reload: HashMap<String, String> = config
            .backends
            .iter()
            .map(|b| (b.name.clone(), config_fingerprint(b)))
            .collect();
        assert_eq!(policy.fingerprints(), reload);
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
