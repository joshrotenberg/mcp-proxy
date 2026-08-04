//! Every example config must stay valid against the current schema.
//!
//! Walks examples/ recursively and runs each .toml through [`ProxyConfig::load`].
//! Builds without an optional feature skip examples that fail with the
//! documented "requires the '<feature>' feature" error, so this test holds at
//! every feature point (default, --no-default-features, --all-features).

use std::path::{Path, PathBuf};

use mcp_proxy::ProxyConfig;

fn collect_tomls(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read examples dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_tomls(&path, out);
        } else if path.extension().is_some_and(|e| e == "toml") {
            out.push(path);
        }
    }
}

#[test]
fn example_configs_are_valid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut tomls = Vec::new();
    collect_tomls(&dir, &mut tomls);
    tomls.sort();
    assert!(
        !tomls.is_empty(),
        "no example configs found under {}",
        dir.display()
    );

    let mut failures = Vec::new();
    for path in &tomls {
        if let Err(err) = ProxyConfig::load(path) {
            let msg = format!("{err:#}");
            // A build without an optional feature rejects configs that use it
            // with the documented error; that is the contract, not drift.
            if msg.contains("requires the '") && msg.contains("' feature") {
                continue;
            }
            failures.push(format!("{}: {msg}", path.display()));
        }
    }
    assert!(
        failures.is_empty(),
        "invalid example configs:\n{}",
        failures.join("\n")
    );
}
