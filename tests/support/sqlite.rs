//! Real SQLite files for logical cache contracts, isolated from disk sync latency.

pub fn directory() -> tempfile::TempDir {
    // Residency assertions need an available store. On Linux, durable-filesystem
    // syncs can legitimately exceed the production fail-open I/O budget in CI.
    // tmpfs still exercises SQLite files, independent connections and locking.
    #[cfg(target_os = "linux")]
    if std::path::Path::new("/dev/shm").is_dir() {
        return tempfile::Builder::new()
            .prefix("mcp-proxy-sqlite-")
            .tempdir_in("/dev/shm")
            .expect("logical SQLite fixtures require a writable /dev/shm");
    }
    tempfile::tempdir().expect("logical SQLite fixtures require a temporary directory")
}
