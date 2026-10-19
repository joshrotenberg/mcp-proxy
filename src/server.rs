//! Standalone HTTP serving with owned connection/HTTP2 tasks and a drain deadline.
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::sync::watch;
use tokio::task::JoinSet;

// Hyper may spawn HTTP/2 stream tasks. Own those too, rather than delegating to
// untracked TokioExecutor tasks that could outlive a cancelled connection.
#[derive(Clone)]
struct Executor(Arc<Mutex<Option<JoinSet<()>>>>);
impl<F> hyper::rt::Executor<F> for Executor
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    fn execute(&self, future: F) {
        let mut tasks = self.0.lock().expect("HTTP stream tasks");
        if let Some(tasks) = &mut *tasks {
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                let _ = future.await;
            });
        }
    }
}

struct StreamOwner(Executor);
impl Drop for StreamOwner {
    fn drop(&mut self) {
        // Also runs if serving is cancelled before normal cleanup.
        self.0.0.lock().expect("HTTP stream tasks").take();
    }
}

pub(crate) async fn serve(
    mut listener: tokio::net::TcpListener,
    router: axum::Router,
    signal: impl Future<Output = ()>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let executor = Executor(Arc::new(Mutex::new(Some(JoinSet::new()))));
    let _stream_owner = StreamOwner(executor.clone());
    let mut connections = JoinSet::new();
    let (drain, _) = watch::channel(false);
    tokio::pin!(signal);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut signal => break Ok(()),
            completed = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = completed { tracing::warn!(%error, "HTTP connection task failed"); }
            }
            // Reuse axum's transient accept-error retry/backoff policy.
            accepted = axum::serve::Listener::accept(&mut listener) => {
                    let (stream, _) = accepted;
                    let service = TowerToHyperService::new(router.clone());
                    let mut draining = drain.subscribe();
                    let executor = executor.clone();
                    connections.spawn(async move {
                        let builder = Builder::new(executor);
                        let connection = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                        tokio::pin!(connection);
                        let result = tokio::select! {
                            result = &mut connection => result,
                            _ = async { let _ = draining.wait_for(|draining| *draining).await; } => {
                                connection.as_mut().graceful_shutdown();
                                connection.await
                            }
                        };
                        if let Err(error) = result { tracing::debug!(%error, "HTTP connection closed"); }
                    });
            }
        }
    };
    drop(listener);
    drain.send_replace(true);
    if tokio::time::timeout(timeout, async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!("Shutdown drain deadline reached; cancelling HTTP connections");
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let streams = executor.0.lock().expect("HTTP stream tasks").take();
    if let Some(mut streams) = streams {
        streams.abort_all();
        while streams.join_next().await.is_some() {}
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn deadline_and_cancel_drop_http1_and_http2_handlers_and_stream_tasks() {
        for h2 in [false, true] {
            for cancel in [false, true] {
                let active = Arc::new(AtomicUsize::new(0));
                let started = Arc::new(tokio::sync::Notify::new());
                let count = active.clone();
                let ready = started.clone();
                let router = axum::Router::new().route(
                    "/wait",
                    axum::routing::get(move || {
                        let count = count.clone();
                        let ready = ready.clone();
                        async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            let _active = Active(count);
                            ready.notify_one();
                            std::future::pending::<&'static str>().await
                        }
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/wait", listener.local_addr().unwrap());
                let (stop, stopped) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(serve(
                    listener,
                    router,
                    async {
                        let _ = stopped.await;
                    },
                    Duration::from_millis(20),
                ));
                let builder = reqwest::Client::builder();
                let http = if h2 {
                    builder.http2_prior_knowledge()
                } else {
                    builder.http1_only()
                }
                .build()
                .unwrap();
                let request = tokio::spawn(async move { http.get(url).send().await });
                tokio::time::timeout(Duration::from_secs(2), started.notified())
                    .await
                    .unwrap();
                if cancel {
                    server.abort();
                    assert!(server.await.unwrap_err().is_cancelled());
                } else {
                    stop.send(()).unwrap();
                    tokio::time::timeout(Duration::from_secs(1), server)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                }
                tokio::time::timeout(Duration::from_secs(1), async {
                    while active.load(Ordering::SeqCst) != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("HTTP handler outlived the server");
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), request)
                        .await
                        .unwrap()
                        .unwrap()
                        .is_err()
                );
            }
        }
    }
}
