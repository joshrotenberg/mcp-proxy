//! Documentation is executable input: strict CLI preparation and real sessions.
#[allow(dead_code)]
#[path = "support/app.rs"]
mod app;
#[allow(dead_code)]
#[path = "support/backend.rs"]
mod backend;
#[path = "support/http.rs"]
mod http;
#[cfg(feature = "oauth")]
#[path = "support/oauth.rs"]
mod oauth;

use app::App;
use backend::Backend;
use http::{Session, success};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const BASE: &str = "[proxy]\nname='documentation'\n[proxy.listen]\n";
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).into()
}
fn collect(dir: &Path, extension: &str, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, extension, files);
        } else if path.extension().is_some_and(|e| e == extension) {
            files.push(path);
        }
    }
}
fn markdowns() -> Vec<PathBuf> {
    let mut files = vec![root().join("README.md"), root().join("CONTRIBUTING.md")];
    for dir in ["docs", "src/skills", "charts", "examples/docker-compose"] {
        collect(&root().join(dir), "md", &mut files);
    }
    files.sort();
    files
}
struct Snippet {
    line: usize,
    text: String,
    fragment: bool,
}
fn snippets(source: &str) -> Vec<Snippet> {
    let mut found = Vec::new();
    let mut lines = source.lines().enumerate().peekable();
    let mut previous = "";
    while let Some((line, text)) = lines.next() {
        if text == "```toml" {
            let mut body = Vec::new();
            let mut closed = false;
            for (_, text) in lines.by_ref() {
                if text == "```" {
                    closed = true;
                    break;
                }
                body.push(text);
            }
            assert!(closed, "unclosed TOML fence at line {}", line + 1);
            found.push(Snippet {
                line: line + 1,
                text: body.join("\n"),
                fragment: previous == "<!-- config-fragment -->",
            });
        }
        previous = text;
    }
    found
}
// Process-local fixture values only; do not mutate this test process's environment.
fn references(value: &toml::Value, vars: &mut std::collections::BTreeSet<String>) {
    match value {
        toml::Value::String(s) if s.contains("${") => {
            let name = s.strip_prefix("${").and_then(|s| s.strip_suffix('}'));
            let name = name.expect("documented references must occupy the whole field");
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            vars.insert(name.into());
        }
        toml::Value::Array(values) => values.iter().for_each(|v| references(v, vars)),
        toml::Value::Table(values) => values.values().for_each(|v| references(v, vars)),
        _ => {}
    }
}
async fn check(label: &str, text: &str) {
    let value: toml::Value = toml::from_str(text).unwrap_or_else(|e| panic!("{label}: {e}"));
    // No fake environment expansion in arguments or URLs: the executable passes these literally.
    if let Some(backends) = value.get("backends").and_then(toml::Value::as_array) {
        for backend in backends {
            for field in ["args", "command", "url"] {
                if let Some(value) = backend.get(field) {
                    assert!(
                        !value.to_string().contains("${"),
                        "{label}: literal {field}"
                    );
                }
            }
        }
    }
    let mut vars = Default::default();
    references(&value, &mut vars);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, text).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-proxy"));
    command.args(["--config", path.to_str().unwrap(), "--check"]);
    for name in vars {
        command.env(
            &name,
            format!("documentation-{}", name.to_ascii_lowercase()),
        );
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("documented CLI check deadline")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // A reduced binary must explicitly reject a missing feature, not another invalid field.
        let feature = [
            "oauth",
            "metrics",
            "otel",
            "websocket",
            "discovery",
            "redis-cache",
            "sqlite-cache",
        ]
        .into_iter()
        .find(|feature| stderr.contains(&format!("requires the '{feature}' feature")));
        if let Some(feature) = feature {
            let enabled = match feature {
                "oauth" => cfg!(feature = "oauth"),
                "metrics" => cfg!(feature = "metrics"),
                "otel" => cfg!(feature = "otel"),
                "websocket" => cfg!(feature = "websocket"),
                "discovery" => cfg!(feature = "discovery"),
                "redis-cache" => cfg!(feature = "redis-cache"),
                "sqlite-cache" => cfg!(feature = "sqlite-cache"),
                _ => unreachable!(),
            };
            assert!(!enabled, "{label}: enabled feature was rejected: {stderr}");
            return;
        }
        panic!("{label}: CLI preparation failed: {stderr}");
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("Config OK\n"),
        "{label}"
    );
}

#[tokio::test]
async fn documented_configs_pass_real_cli_preparation() {
    let mut count = 0;
    for file in markdowns() {
        let source = std::fs::read_to_string(&file).unwrap();
        for snippet in snippets(&source) {
            let label = format!("{}:{}", file.display(), snippet.line);
            let value: toml::Value = toml::from_str(&snippet.text).expect(&label);
            if value.get("dependencies").is_some() {
                assert_eq!(file, root().join("README.md"));
                let minor_version = env!("CARGO_PKG_VERSION")
                    .split('.')
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(".");
                assert_eq!(
                    value["dependencies"]["mcp-proxy"].as_str(),
                    Some(minor_version.as_str()),
                    "README dependency must follow the package minor version"
                );
                assert!(!snippet.fragment);
                continue;
            }
            assert!(!snippet.fragment || file == root().join("README.md"));
            let text = if snippet.fragment {
                assert!(
                    value.get("proxy").is_none(),
                    "fragment duplicates the shared base"
                );
                if value.get("backends").is_none() {
                    format!(
                        "{BASE}[[backends]]\nname='files'\ntransport='http'\nurl='http://127.0.0.1:9000/mcp'\n{}",
                        snippet.text
                    )
                } else {
                    format!("{BASE}{}", snippet.text)
                }
            } else {
                snippet.text
            };
            check(&label, &text).await;
            count += 1;
        }
    }
    let mut files = vec![root().join("config.example.toml")];
    collect(&root().join("examples"), "toml", &mut files);
    for file in files {
        check(
            &file.display().to_string(),
            &std::fs::read_to_string(&file).unwrap(),
        )
        .await;
        count += 1;
    }
    assert!(
        count >= 40,
        "documentation inventory unexpectedly shrank: {count}"
    );
    eprintln!("checked {count} documented configurations/fragments through the CLI");
}

#[test]
fn local_markdown_link_paths_exist() {
    // Inline destinations only, excluding images/HTML, external URLs and anchor verification.
    let links = regex::Regex::new(r"\]\(([^\s)]+)(?:\s+[^)]*)?\)").unwrap();
    let mut checked = 0;
    for file in markdowns() {
        let source = std::fs::read_to_string(&file).unwrap();
        for found in links.captures_iter(&source) {
            let target = found[1].trim_matches(['<', '>']);
            if target.starts_with('#') || target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            let path = target.split('#').next().unwrap();
            assert!(
                file.parent().unwrap().join(path).exists(),
                "{}: {target}",
                file.display()
            );
            checked += 1;
        }
    }
    assert!(
        checked > 50,
        "local link inventory unexpectedly shrank: {checked}"
    );
}

fn readme_config(auth_type: Option<&str>) -> mcp_proxy::ProxyConfig {
    let source = std::fs::read_to_string(root().join("README.md")).unwrap();
    let snippet = snippets(&source)
        .into_iter()
        .find(|s| {
            let value: toml::Value = toml::from_str(&s.text).unwrap();
            match auth_type {
                Some(kind) => {
                    value
                        .get("auth")
                        .and_then(|v| v.get("type"))
                        .and_then(toml::Value::as_str)
                        == Some(kind)
                }
                None => {
                    value
                        .get("proxy")
                        .and_then(|v| v.get("name"))
                        .and_then(toml::Value::as_str)
                        == Some("my-proxy")
                }
            }
        })
        .expect("README workflow config");
    mcp_proxy::ProxyConfig::parse(&snippet.text).unwrap()
}
fn port(config: &mut mcp_proxy::ProxyConfig) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    config.proxy.listen.port = port;
    port
}
async fn assert_echo(session: &Session, backend: &Backend) {
    let listed = session.rpc("tools/list", json!({})).await;
    let mut names: Vec<_> = success(&listed)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["files/blocked", "files/echo"]);
    assert!(backend.calls().is_empty());
    let reply = session
        .call("files/echo", json!({"documented":"workflow"}))
        .await;
    let content = &success(&reply)["content"];
    assert_eq!(content.as_array().unwrap().len(), 1);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(content[0]["text"].as_str().unwrap()).unwrap(),
        json!({"documented":"workflow"})
    );
    assert_eq!(
        backend.calls(),
        vec![json!({"name":"echo","arguments":{"documented":"workflow"}})]
    );
}
#[tokio::test]
async fn readme_quickstart_completes_local_cli_session() {
    let backend = Backend::new("stdio").await;
    let mut config = readme_config(None);
    // Preserve documented topology/config; substitute the external npm peer with a strict local peer.
    let peer: mcp_proxy::config::BackendConfig =
        toml::from_str(&format!("name='files'\n{}", backend.config)).unwrap();
    config.backends[0].command.clone_from(&peer.command);
    config.backends[0].args.clone_from(&peer.args);
    let port = port(&mut config);
    let mut app = App::cli(&toml::to_string(&config).unwrap(), port).await;
    let session = Session::start(&app.url, "local-client").await;
    assert_echo(&session, &backend).await;
    let response = http::client()
        .get(format!("{}/admin/health", app.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    session.terminate().await;
    app.shutdown().await;
}
#[cfg(feature = "oauth")]
#[tokio::test]
async fn readme_jwt_roles_and_management_are_separate() {
    let mut authority = oauth::Authority::new().await;
    let backend = Backend::new("stdio").await;
    let mut config = readme_config(Some("jwt"));
    let mcp_proxy::config::AuthConfig::Jwt {
        issuer,
        audience,
        jwks_uri,
        ..
    } = config.auth.as_mut().unwrap()
    else {
        unreachable!()
    };
    issuer.clone_from(&authority.state.issuer);
    *audience = oauth::RESOURCE.into();
    *jwks_uri = format!("{}/keys", authority.server.url);
    config.backends[0] = toml::from_str(&format!("name='files'\n{}", backend.config)).unwrap();
    let port = port(&mut config);
    let mut app = App::cli_with_env(
        &toml::to_string(&config).unwrap(),
        port,
        &[("ADMIN_TOKEN", "documentation-admin")],
    )
    .await;
    let mut claims = authority.claims("operator");
    claims["scope"] = json!("mcp:read");
    let reader = Session::start(&app.url, &authority.jwt(&claims)).await;
    let listed = reader.rpc("tools/list", json!({})).await;
    assert!(success(&listed)["tools"].as_array().unwrap().is_empty());
    let rejected = reader.call("files/blocked", json!({})).await;
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert!(backend.calls().is_empty());
    reader.terminate().await;
    claims["scope"] = json!("mcp:admin");
    let token = authority.jwt(&claims);
    let ordinary = Session::start(&app.url, &token).await;
    assert_echo(&ordinary, &backend).await;
    for (token, status) in [
        (token.as_str(), reqwest::StatusCode::UNAUTHORIZED),
        ("documentation-admin", reqwest::StatusCode::OK),
    ] {
        let response = http::client()
            .get(format!("{}/admin/config", app.url))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    ordinary.terminate().await;
    app.shutdown().await;
    authority.server.stop().await;
}

#[test]
fn mixed_write_recipes_keep_independent_calls_separate() {
    for name in ["production-gateway", "enterprise-hub", "compliance"] {
        let source = std::fs::read_to_string(root().join(format!("examples/{name}.toml"))).unwrap();
        let config: toml::Value = toml::from_str(&source).unwrap();
        assert_eq!(
            config["performance"]["coalesce_requests"].as_bool(),
            Some(false),
            "{name}: global coalescing suppresses independent writes"
        );
        assert_eq!(
            config["auth"]["role_mapping"]["default_deny"].as_bool(),
            Some(true),
            "{name}: unmapped roles must fail closed"
        );
    }
}
