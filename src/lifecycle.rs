//! Ownership and cancellation of a configured proxy's runtime resources.
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tower_mcp::client::ClientTransport;
use tower_mcp::error::{Error, Result};

/// An explicit shutdown handle for a standalone or embedded proxy.
///
/// Obtain it with [`crate::Proxy::shutdown_handle`] before consuming the proxy.
/// Stop accepting and drain host requests first, then call [`Self::shutdown`].
/// Shutdown closes MCP sessions and managed backend transports, cancels and joins
/// health/reload workers, and prevents new requests through the proxy router.
/// It is idempotent across clones. Dropping the last proxy/router/handle owner
/// cancels resources without waiting; retain a handle to await cleanup explicitly.
#[derive(Clone)]
pub struct ShutdownHandle(pub(crate) Arc<Runtime>);

impl ShutdownHandle {
    /// Stop managed resources and wait for their cleanup. Requires Tokio.
    /// Cancelling a waiter leaves cleanup owned; another clone can await it.
    ///
    /// Pending backend operations are interrupted, not replayed. Remote work
    /// may already have had side effects. The embedding host owns its listener
    /// and connection tasks; this method does not stop unrelated host routes.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.shutdown().await
    }
}

/// Cancellation shared with connection builders, without retaining the owner.
#[derive(Clone)]
pub(crate) struct Connections {
    stop: tokio_util::sync::CancellationToken,
    active: Vec<watch::Sender<usize>>,
    registration: Arc<Mutex<()>>,
}
impl Connections {
    pub fn new() -> Self {
        Self {
            stop: tokio_util::sync::CancellationToken::new(),
            active: vec![watch::channel(0).0],
            registration: Arc::new(Mutex::new(())),
        }
    }
    pub fn child(&self) -> Self {
        let mut active = self.active.clone();
        active.push(watch::channel(0).0);
        Self {
            stop: self.stop.child_token(),
            active,
            registration: self.registration.clone(),
        }
    }
    pub fn stopped(&self) -> bool {
        self.stop.is_cancelled()
    }
    pub fn stop(&self) {
        let _guard = self.registration.lock().expect("transport registration");
        self.stop.cancel();
    }
    pub fn wrap<T: ClientTransport>(&self, transport: T) -> ManagedTransport<T> {
        let _guard = self.registration.lock().expect("transport registration");
        for active in &self.active {
            active.send_modify(|active| *active += 1);
        }
        let mut managed = ManagedTransport {
            inner: Some(transport),
            lease: Some(Lease(self.clone())),
            stopped: self.stop.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        if self.stopped() {
            managed.start_close();
        }
        managed
    }
    pub(crate) async fn closed(&self) {
        let mut active = self.active.last().expect("transport counter").subscribe();
        let _ = active.wait_for(|active| *active == 0).await;
    }
}
struct Lease(Connections);
impl Drop for Lease {
    fn drop(&mut self) {
        for active in &self.0.active {
            active.send_modify(|active| *active -= 1);
        }
    }
}

// A completed stop future must win even when the underlying transport is ready.
async fn interrupt<F: Future>(
    stop: &tokio_util::sync::CancellationToken,
    future: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        result = future => Some(result),
    }
}

pub(crate) struct ManagedTransport<T: ClientTransport> {
    // Drop the transport before releasing the completion lease.
    inner: Option<T>,
    lease: Option<Lease>,
    stopped: tokio_util::sync::CancellationToken,
    runtime: tokio::runtime::Handle,
}
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
impl<T: ClientTransport> ManagedTransport<T> {
    fn start_close(&mut self) -> Option<tokio::sync::oneshot::Receiver<Result<()>>> {
        let mut inner = self.inner.take()?;
        let lease = self.lease.take();
        let (done, finished) = tokio::sync::oneshot::channel();
        // Keep cleanup and its completion lease alive if the client/constructor
        // or caller awaiting close is cancelled. The registry waits for it.
        self.runtime.spawn(async move {
            let result = tokio::time::timeout(CLOSE_TIMEOUT, inner.close())
                .await
                .unwrap_or_else(|_| {
                    Err(Error::Transport("Transport close deadline reached".into()))
                });
            drop(inner);
            drop(lease);
            let _ = done.send(result);
        });
        Some(finished)
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(finished) = self.start_close() {
            finished
                .await
                .unwrap_or_else(|_| Err(Error::Transport("Transport cleanup interrupted".into())))
        } else {
            Ok(())
        }
    }
}
impl<T: ClientTransport> Drop for ManagedTransport<T> {
    fn drop(&mut self) {
        self.start_close();
    }
}
#[async_trait::async_trait]
impl<T: ClientTransport> ClientTransport for ManagedTransport<T> {
    async fn send(&mut self, message: &str) -> Result<()> {
        let Some(inner) = &mut self.inner else {
            return Err(Error::Transport("Proxy shut down".into()));
        };
        match interrupt(&self.stopped, inner.send(message)).await {
            Some(result) => result,
            None => {
                let _ = self.stop().await;
                Err(Error::Transport("Proxy shut down".into()))
            }
        }
    }
    async fn recv(&mut self) -> Result<Option<String>> {
        let Some(inner) = &mut self.inner else {
            return Ok(None);
        };
        match interrupt(&self.stopped, inner.recv()).await {
            Some(result) => result,
            None => {
                let _ = self.stop().await;
                Ok(None)
            }
        }
    }
    fn is_connected(&self) -> bool {
        !self.stopped.is_cancelled()
            && self
                .inner
                .as_ref()
                .is_some_and(ClientTransport::is_connected)
    }
    async fn close(&mut self) -> Result<()> {
        self.stop().await
    }
    async fn reset_session(&mut self) {
        if let Some(inner) = &mut self.inner
            && interrupt(&self.stopped, inner.reset_session())
                .await
                .is_none()
        {
            let _ = self.stop().await;
        }
    }
    async fn end_session(&mut self) {
        if let Some(inner) = &mut self.inner
            && interrupt(&self.stopped, inner.end_session())
                .await
                .is_none()
        {
            let _ = self.stop().await;
        }
    }
    async fn cancel_request(&mut self, id: &tower_mcp::protocol::RequestId) -> Result<()> {
        let Some(inner) = &mut self.inner else {
            return Err(Error::Transport("Proxy shut down".into()));
        };
        match interrupt(&self.stopped, inner.cancel_request(id)).await {
            Some(result) => result,
            None => {
                let _ = self.stop().await;
                Err(Error::Transport("Proxy shut down".into()))
            }
        }
    }
    fn supports_session_recovery(&self) -> bool {
        !self.stopped.is_cancelled()
            && self
                .inner
                .as_ref()
                .is_some_and(ClientTransport::supports_session_recovery)
    }
}

enum ShutdownJob {
    Running(tokio::task::JoinHandle<std::result::Result<(), String>>),
    Complete(std::result::Result<(), String>),
}

pub(crate) struct Runtime {
    pub connections: Connections,
    pub watcher: Mutex<Option<crate::reload::ConfigWatcher>>,
    pub health: Mutex<Option<crate::admin::HealthChecker>>,
    pub proxy: Mutex<Option<tower_mcp::proxy::McpProxy>>,
    pub sessions: Mutex<Option<tower_mcp::SessionHandle>>,
    shutdown: tokio::sync::Mutex<Option<ShutdownJob>>,
}
impl Runtime {
    pub fn new(connections: Connections) -> Arc<Self> {
        Arc::new(Self {
            connections,
            watcher: Mutex::new(None),
            health: Mutex::new(None),
            proxy: Mutex::new(None),
            sessions: Mutex::new(None),
            shutdown: tokio::sync::Mutex::new(None),
        })
    }
    pub(crate) async fn shutdown(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut job = self.shutdown.lock().await;
        if job.is_none() {
            let runtime = self.clone();
            *job = Some(ShutdownJob::Running(tokio::spawn(async move {
                runtime.cleanup().await.map_err(|error| error.to_string())
            })));
        }
        // Keep the JoinHandle in shared state while awaiting it. Cancelling a
        // caller leaves cleanup running and another caller can still join it.
        let result = match job.as_mut().expect("shutdown job") {
            ShutdownJob::Complete(result) => result.clone(),
            ShutdownJob::Running(task) => {
                let result = match task.await {
                    Ok(result) => result,
                    Err(_) => Err("Proxy cleanup task failed".to_owned()),
                };
                *job = Some(ShutdownJob::Complete(result.clone()));
                result
            }
        };
        result.map_err(anyhow::Error::msg)
    }
    async fn cleanup(&self) -> anyhow::Result<()> {
        self.connections.stop();
        let watcher = self.watcher.lock().expect("config watcher owner").take();
        let health = self.health.lock().expect("health checker owner").take();
        // Signal both before waiting for either worker.
        if let Some(health) = &health {
            health.cancel();
        }
        let mut error = None;
        if let Some(watcher) = watcher
            && let Err(e) = watcher.shutdown().await
        {
            error = Some(e);
        }
        if let Some(health) = health
            && let Err(e) = health.shutdown().await
        {
            error = Some(e);
        }
        let sessions = self.sessions.lock().expect("session owner").clone();
        if let Some(sessions) = sessions {
            for session in sessions.list_sessions().await {
                sessions.terminate_session(&session.id).await;
            }
        }
        self.connections.closed().await;
        let proxy = self.proxy.lock().expect("backend owner").clone();
        if let Some(proxy) = proxy {
            for namespace in proxy.backend_namespaces() {
                proxy.remove_backend(&namespace).await;
            }
        }
        error.map_or(Ok(()), Err)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.connections.stop();
        self.watcher.get_mut().expect("config watcher owner").take();
        self.health.get_mut().expect("health checker owner").take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Transport {
        closed: Arc<AtomicBool>,
        release: Arc<tokio::sync::Notify>,
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl ClientTransport for Transport {
        async fn send(&mut self, _: &str) -> Result<()> {
            std::future::pending().await
        }
        async fn recv(&mut self) -> Result<Option<String>> {
            std::future::pending().await
        }
        fn is_connected(&self) -> bool {
            true
        }
        async fn close(&mut self) -> Result<()> {
            self.started.notify_one();
            self.release.notified().await;
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn shutdown_cleanup_remains_owned_when_its_waiter_is_cancelled() {
        let runtime = Runtime::new(Connections::new());
        let closed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let mut transport = runtime.connections.wrap(Transport {
            closed: closed.clone(),
            release: release.clone(),
            started: started.clone(),
        });
        let driver = tokio::spawn(async move { transport.recv().await });
        let owner = runtime.clone();
        let waiter = tokio::spawn(async move { owner.shutdown().await });
        started.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), runtime.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(closed.load(Ordering::SeqCst));
        assert!(driver.await.unwrap().unwrap().is_none());
        runtime.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn transport_close_survives_caller_cancellation_and_child_scope_does_not_stop_parent() {
        let root = Connections::new();
        let scope = root.child();
        let closed = Arc::new(AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let mut transport = scope.wrap(Transport {
            started: started.clone(),
            closed: closed.clone(),
            release: release.clone(),
        });
        let close = tokio::spawn(async move { transport.close().await });
        // Observe close starting before cancelling its caller.
        started.notified().await;
        close.abort();
        let _ = close.await;
        scope.stop();
        assert!(!root.stopped());
        assert!(!closed.load(Ordering::SeqCst));
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), scope.closed())
            .await
            .unwrap();
        root.closed().await;
        assert!(closed.load(Ordering::SeqCst));
        root.stop();
        let rejected = root.wrap(Transport {
            started,
            closed: closed.clone(),
            release,
        });
        assert!(!rejected.is_connected());
        root.closed().await;
    }
}
