//! Check each declared rustdoc configuration in its exact documented context.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[derive(Debug)]
struct Snippet {
    line: usize,
    text: String,
    contexts: Vec<String>,
    features: Vec<String>,
    import: Option<String>,
}

fn feature_enabled(feature: &str) -> Result<bool> {
    Ok(match feature {
        "oauth" => cfg!(feature = "oauth"),
        "metrics" => cfg!(feature = "metrics"),
        "otel" => cfg!(feature = "otel"),
        "websocket" => cfg!(feature = "websocket"),
        "discovery" => cfg!(feature = "discovery"),
        "redis-cache" => cfg!(feature = "redis-cache"),
        "sqlite-cache" => cfg!(feature = "sqlite-cache"),
        other => bail!("unknown documented feature: {other}"),
    })
}

fn declaration(text: &str, line: usize) -> Result<Snippet> {
    let marker = text
        .strip_prefix("<!-- doc-config: ")
        .and_then(|s| s.strip_suffix(" -->"))
        .context("TOML fence needs an adjacent doc-config declaration")?;
    let mut parts = marker.split_whitespace();
    let mode = parts.next().context("missing configuration mode")?;
    let contexts = if mode == "complete" {
        Vec::new()
    } else {
        let paths = mode
            .strip_prefix("fragment=")
            .context("expected complete or fragment=PATH[,PATH]")?;
        ensure!(!paths.is_empty(), "empty composition context");
        paths.split(',').map(str::to_owned).collect()
    };
    let features = parts
        .next()
        .and_then(|s| s.strip_prefix("features="))
        .context("declare required features, or features=none")?;
    let features: Vec<String> = if features == "none" {
        Vec::new()
    } else {
        features.split(',').map(str::to_owned).collect()
    };
    for feature in &features {
        feature_enabled(feature)?;
    }
    let import = parts
        .next()
        .map(|s| {
            s.strip_prefix("import=")
                .map(str::to_owned)
                .context("unknown declaration field")
        })
        .transpose()?;
    ensure!(parts.next().is_none(), "extra declaration fields");
    Ok(Snippet {
        line,
        text: String::new(),
        contexts,
        features,
        import,
    })
}

fn doc_line(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let text = line
        .strip_prefix("//!")
        .or_else(|| line.strip_prefix("///").filter(|s| !s.starts_with('/')))?;
    Some(text.strip_prefix(' ').unwrap_or(text))
}

// Keep the source scanner deliberately small: TOML examples must use line docs.
// Reject block/attribute forms with fence openers instead of silently skipping them.
fn toml_fence(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    let character = trimmed.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let length = trimmed.chars().take_while(|c| *c == character).count();
    if length < 3 {
        return None;
    }
    let (fence, info) = trimmed.split_at(length);
    info.trim()
        .starts_with("toml")
        .then_some((fence, info.trim()))
}

fn snippets(source: &str) -> Result<Vec<Snippet>> {
    // Content attributes can hide fences in multiline strings or include_str!.
    // Require line docs for all such content instead of parsing Rust strings.
    // Metadata attributes such as #[doc(hidden)] remain allowed.
    let content_attribute = regex::Regex::new(r"#\s*!?\s*\[\s*doc\s*=").unwrap();
    ensure!(
        !content_attribute.is_match(source),
        "documentation-content attributes must use //! or /// line docs"
    );
    let mut found = Vec::new();
    let mut previous = "";
    let mut active: Option<(Snippet, String, usize)> = None;
    for (number, source_line) in source.lines().enumerate() {
        let Some(text) = doc_line(source_line) else {
            ensure!(active.is_none(), "TOML fence leaves doc comments");
            let raw = source_line.trim_start();
            let block = raw
                .strip_prefix("/*!")
                .or_else(|| raw.strip_prefix("/**"))
                .or_else(|| raw.strip_prefix('*'))
                .unwrap_or(raw);
            ensure!(
                toml_fence(block).is_none(),
                "TOML fences must use //! or /// line docs"
            );
            previous = "";
            continue;
        };
        if let Some((snippet, fence, indent)) = active.as_mut() {
            let trimmed = text.trim();
            if trimmed.len() >= fence.len()
                && trimmed.chars().all(|c| c == fence.chars().next().unwrap())
            {
                found.push(active.take().unwrap().0);
            } else {
                let remove = text.len() - text.trim_start_matches(' ').len();
                snippet.text.push_str(&text[remove.min(*indent)..]);
                snippet.text.push('\n');
            }
        } else if let Some((fence, info)) = toml_fence(text) {
            let indent = text.len() - text.trim_start().len();
            ensure!(
                indent <= 3,
                "TOML fences support at most three spaces of indentation"
            );
            ensure!(
                info == "toml",
                "unsupported TOML fence attributes at line {}",
                number + 1
            );
            active = Some((
                declaration(previous.trim(), number + 1)?,
                fence.to_owned(),
                indent,
            ));
        }
        previous = text;
    }
    ensure!(active.is_none(), "unclosed TOML fence");
    Ok(found)
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn inventory() -> Vec<(PathBuf, Snippet)> {
    let mut files = Vec::new();
    collect(&root().join("src"), &mut files).unwrap();
    files.sort();
    files
        .into_iter()
        .flat_map(|file| {
            let source = std::fs::read_to_string(&file).unwrap();
            snippets(&source)
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()))
                .into_iter()
                .map(move |snippet| (file.clone(), snippet))
        })
        .collect()
}

fn compose(snippet: &Snippet) -> String {
    let mut text = String::new();
    for context in &snippet.contexts {
        text.push_str(&std::fs::read_to_string(root().join(context)).unwrap());
        text.push('\n');
    }
    text.push_str(&snippet.text);
    text
}

fn references(value: &toml::Value, names: &mut BTreeSet<String>) {
    match value {
        toml::Value::String(s) if s.contains("${") => {
            let name = s
                .strip_prefix("${")
                .and_then(|s| s.strip_suffix('}'))
                .expect("documented secret references must occupy the whole field");
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            names.insert(name.into());
        }
        toml::Value::Array(values) => values.iter().for_each(|v| references(v, names)),
        toml::Value::Table(values) => values.values().for_each(|v| references(v, names)),
        _ => {}
    }
}

async fn cli(text: &str, import: Option<&str>) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, text).unwrap();
    if let Some(import) = import {
        std::fs::copy(root().join(import), dir.path().join(".mcp.json")).unwrap();
    }
    let value: toml::Value = toml::from_str(text).unwrap();
    // These fields are passed literally, not expanded by configuration preparation.
    // A successful --check must not make a misleading interpolation example pass.
    if let Some(backends) = value.get("backends").and_then(toml::Value::as_array) {
        for backend in backends {
            for field in ["args", "command", "url"] {
                if let Some(value) = backend.get(field) {
                    assert!(!value.to_string().contains("${"), "literal backend {field}");
                }
            }
        }
    }
    let mut names = BTreeSet::new();
    references(&value, &mut names);
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"));
    command.args(["--config", path.to_str().unwrap(), "--check"]);
    for name in names {
        command.env(&name, format!("rustdoc-{}", name.to_ascii_lowercase()));
    }
    command.kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("rustdoc CLI check deadline")
        .unwrap()
}

#[tokio::test]
async fn every_rustdoc_configuration_has_a_checked_context_and_feature_boundary() {
    let entries = inventory();
    assert!(entries.len() >= 28, "rustdoc TOML inventory shrank");
    for (file, snippet) in entries {
        let label = format!("{}:{}", file.display(), snippet.line);
        let text = compose(&snippet);
        let output = cli(&text, snippet.import.as_deref()).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let missing: Vec<_> = snippet
            .features
            .iter()
            .filter(|f| !feature_enabled(f).unwrap())
            .collect();
        if missing.is_empty() {
            assert!(output.status.success(), "{label}: {stderr}");
            assert!(
                String::from_utf8_lossy(&output.stdout).starts_with("Config OK\n"),
                "{label}: missing CLI success marker"
            );
        } else {
            assert!(!output.status.success(), "{label}: absent feature accepted");
            assert!(
                missing
                    .iter()
                    .any(|f| stderr.contains(&format!("requires the '{f}' feature"))),
                "{label}: expected absent feature {missing:?}, got {stderr}"
            );
        }
    }
}

async fn assert_parse_rejected(text: &str, reason: &str) {
    // The CLI deliberately redacts parse details. Establish the exact structural
    // reason separately using only this synthetic configuration, then require
    // the real CLI to reject it through its safe diagnostic path too.
    let error = toml::from_str::<mcp_proxy::ProxyConfig>(text).unwrap_err();
    assert!(
        error.to_string().contains(reason),
        "expected {reason}, got {error}"
    );
    let output = cli(text, None).await;
    assert!(!output.status.success(), "invalid configuration accepted");
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid TOML configuration"));
}

#[tokio::test]
async fn required_context_and_stale_fields_fail_real_cli_preparation() {
    let complete = inventory()
        .into_iter()
        .find(|(_, snippet)| snippet.contexts.is_empty())
        .expect("complete rustdoc configuration")
        .1;
    let base: toml::Value = toml::from_str(&compose(&complete)).unwrap();
    for field in ["name", "listen"] {
        let mut value = base.clone();
        value["proxy"].as_table_mut().unwrap().remove(field);
        assert_parse_rejected(
            &toml::to_string(&value).unwrap(),
            &format!("missing field `{field}`"),
        )
        .await;
    }
    let mut value = base.clone();
    value["proxy"]
        .as_table_mut()
        .unwrap()
        .insert("stale_setting".into(), true.into());
    assert_parse_rejected(
        &toml::to_string(&value).unwrap(),
        "unknown field `stale_setting`",
    )
    .await;

    // A fragment must not become a misleading standalone configuration.
    let fragment = inventory()
        .into_iter()
        .find(|(path, _)| path.ends_with("retry.rs"))
        .unwrap()
        .1;
    assert_parse_rejected(&fragment.text, "missing field `proxy`").await;
}

#[test]
fn scanner_requires_declarations_for_inner_and_outer_doc_fences() {
    let declaration = "<!-- doc-config: complete features=none -->";
    for prefix in ["//!", "///", "    ///"] {
        for fence in ["```", "````", "~~~", "~~~~"] {
            let source = format!(
                "{prefix} {declaration}\n{prefix} {fence}toml\n{prefix} [proxy]\n{prefix} {fence}\n"
            );
            let found = snippets(&source).unwrap();
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].text, "[proxy]\n");
            assert!(snippets(&source.replace(declaration, "unlabeled fragment")).is_err());
            assert!(snippets(&source.replace("features=none", "features=typo")).is_err());
            assert!(snippets(source.trim_end().strip_suffix(fence).unwrap()).is_err());
        }
    }
}

#[test]
fn scanner_handles_indented_fences_and_rejects_unsupported_doc_forms() {
    let source = "//! <!-- doc-config: complete features=none -->\n//!   ````toml\n//!   [proxy]\n//!   `````\n";
    assert_eq!(snippets(source).unwrap()[0].text, "[proxy]\n");
    assert!(
        snippets("#[doc(hidden)]\n#![doc(alias = \"example\")]")
            .unwrap()
            .is_empty()
    );
    for source in [
        "/*!\n```toml\n[proxy]\n```\n*/",
        "/** ```toml\n[proxy]\n``` */",
        "#[doc = \"```toml\"]",
        "#[doc =\n\"```toml\\n[proxy]\\n```\"]",
        "#![doc =\n\"```toml\\n[proxy]\\n```\"]",
        "#\n!\n[\ndoc\n=\ninclude_str!(\"config.md\")]",
        "#[doc = include_str!(\"config.md\")]",
        "#![doc = \"```toml\\n[proxy]\\n```\"]",
        "#[doc = \"``` toml\\n[proxy]\\n```\"]",
        "# ! [ doc = \"~~~~ toml\\n[proxy]\\n~~~~\"]",
        "//! <!-- doc-config: complete features=none -->\n//!     ```toml\n//! [proxy]\n//! ```",
    ] {
        assert!(
            snippets(source).is_err(),
            "unsupported form was skipped: {source}"
        );
    }
}
