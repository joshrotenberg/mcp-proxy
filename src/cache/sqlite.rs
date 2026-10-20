//! Lazy SQLite I/O runs only on bounded blocking workers, never on Tokio workers.
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub(crate) struct Store {
    path: String,
    scope: String,
    kind: &'static str,
    ttl: Duration,
    capacity: u64,
    conn: Arc<Mutex<Option<Connection>>>,
    permit: Arc<tokio::sync::Semaphore>,
}
impl Store {
    pub fn new(
        path: String,
        scope: String,
        kind: &'static str,
        ttl: Duration,
        capacity: u64,
    ) -> Self {
        Self {
            path,
            scope,
            kind,
            ttl,
            capacity,
            conn: Default::default(),
            permit: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection, &Self, i64) -> rusqlite::Result<T> + Send + 'static,
    ) -> Option<T> {
        // Hold the permit inside the job even if its waiter times out/cancels.
        let permit = self.permit.clone().try_acquire_owned().map_err(|_| {
            tracing::debug!(target:"mcp_proxy::cache",stage="admission", "SQLite cache bypassed");
        }).ok()?;
        let store = self.clone();
        let started = std::time::Instant::now();
        let job = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            tracing::debug!(target:"mcp_proxy::cache",stage="worker_start",elapsed_ms=started.elapsed().as_millis() as u64,"SQLite cache operation");
            let mut guard = store.conn.lock().ok()?;
            if guard.is_none() {
                let conn = Connection::open(&store.path)
                    .map_err(|error| trace_error("open", &error))
                    .ok()?;
                conn.busy_timeout(Duration::from_millis(25)).ok()?;
                conn.execute_batch(
                    "PRAGMA journal_mode=WAL;
                    PRAGMA synchronous=NORMAL;
                    BEGIN IMMEDIATE;
                    CREATE TABLE IF NOT EXISTS mcp_cache_v2 (
                    scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL,
                    value TEXT NOT NULL, expires_at INTEGER NOT NULL, sequence INTEGER NOT NULL,
                    PRIMARY KEY(scope, kind, key));
                    CREATE INDEX IF NOT EXISTS mcp_cache_v2_expiry ON mcp_cache_v2(expires_at);
                    CREATE INDEX IF NOT EXISTS mcp_cache_v2_order ON mcp_cache_v2(scope, sequence);
                    COMMIT;",
                )
                .map_err(|error| trace_error("schema", &error))
                .ok()?;
                tracing::debug!(target:"mcp_proxy::cache",stage="schema_ready",elapsed_ms=started.elapsed().as_millis() as u64,"SQLite cache operation");
                *guard = Some(conn);
            }
            let conn = guard.as_mut()?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64;
            // Also reclaim expired rows from inactive generations, on every operation.
            conn.execute("DELETE FROM mcp_cache_v2 WHERE expires_at <= ?1", [now])
                .map_err(|error| trace_error("prune", &error))
                .ok()?;
            let result = operation(conn, &store, now)
                .map_err(|error| trace_error("query", &error))
                .ok();
            tracing::debug!(target:"mcp_proxy::cache",stage="complete",elapsed_ms=started.elapsed().as_millis() as u64,"SQLite cache operation");
            result
        });
        match tokio::time::timeout(Duration::from_millis(200), job).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                tracing::debug!(target:"mcp_proxy::cache",stage="worker_failure","SQLite cache bypassed");
                None
            }
            Err(_) => {
                tracing::debug!(target:"mcp_proxy::cache",stage="deadline",elapsed_ms=started.elapsed().as_millis() as u64,"SQLite cache bypassed");
                None
            }
        }
    }
    pub async fn get(&self, key: &str) -> Option<String> {
        let key = key.to_owned();
        self.run(move |conn, store, _| {
            conn.query_row(
                "SELECT value FROM mcp_cache_v2 WHERE scope=?1 AND kind=?2 AND key=?3",
                params![store.scope, store.kind, key],
                |row| row.get(0),
            )
        })
        .await
    }
    pub async fn insert(&self, key: String, value: String) {
        let _ = self.run(move |conn, store, now| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let sequence: i64 = tx.query_row("SELECT COALESCE(MAX(sequence), 0) + 1 FROM mcp_cache_v2 WHERE scope=?1", [&store.scope], |r| r.get(0))?;
            let expires = now.saturating_add(store.ttl.as_millis().min(i64::MAX as u128) as i64);
            tx.execute("INSERT OR REPLACE INTO mcp_cache_v2 VALUES (?1,?2,?3,?4,?5,?6)", params![store.scope,store.kind,key,value,expires,sequence])?;
            let capacity = store.capacity.min(i64::MAX as u64) as i64;
            tx.execute("DELETE FROM mcp_cache_v2 WHERE rowid IN (SELECT rowid FROM mcp_cache_v2 WHERE scope=?1 ORDER BY sequence DESC, kind, key LIMIT -1 OFFSET ?2)", params![store.scope,capacity])?;
            tx.commit()
        }).await;
    }
    pub async fn clear(&self) {
        let _ = self
            .run(|conn, store, _| {
                conn.execute(
                    "DELETE FROM mcp_cache_v2 WHERE scope=?1 AND kind=?2",
                    params![store.scope, store.kind],
                )
            })
            .await;
    }
    pub async fn count(&self) -> u64 {
        self.run(|conn, store, _| {
            conn.query_row(
                "SELECT COUNT(*) FROM mcp_cache_v2 WHERE scope=?1 AND kind=?2",
                params![store.scope, store.kind],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .unwrap_or(0) as u64
    }
}

fn trace_error(stage: &'static str, error: &rusqlite::Error) {
    if !matches!(error, rusqlite::Error::QueryReturnedNoRows) {
        tracing::debug!(target:"mcp_proxy::cache",stage,code=?error.sqlite_error_code(),"SQLite cache bypassed");
    }
}
