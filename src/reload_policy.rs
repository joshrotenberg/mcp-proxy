//! Admission and last-attempt status for the supported live-reload contract.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::config::{BackendConfig, ProxyConfig};

const CONNECTION_FIELDS: &[&str] = &[
    "name",
    "transport",
    "command",
    "args",
    "url",
    "env",
    "bearer_token",
    "headers",
];
const RESTART: &str = "Restart required: this revision changes startup policy or a policy-bearing backend; active configuration retained";

#[derive(Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub(crate) struct ReloadStatus {
    pub watcher_active: bool,
    pub last_result: &'static str,
    pub applied: bool,
    pub restart_required: bool,
}

struct Inner {
    // Never serialize or Debug-format these credential-bearing snapshots.
    startup: Value,
    connections: crate::lifecycle::Connections,
    plain_policy: Value,
    status: Mutex<ReloadStatus>,
}

#[derive(Clone)]
pub(crate) struct ReloadPolicy(Arc<Inner>);

fn policy_projection(mut backend: Value) -> Value {
    for field in CONNECTION_FIELDS {
        backend
            .as_object_mut()
            .expect("backend object")
            .remove(*field);
    }
    backend
}
fn backends(config: &Value) -> HashMap<&str, &Value> {
    config["backends"]
        .as_array()
        .expect("backend array")
        .iter()
        .map(|backend| (backend["name"].as_str().expect("backend name"), backend))
        .collect()
}

impl ReloadPolicy {
    pub fn new(config: &ProxyConfig) -> Self {
        let plain: BackendConfig =
            serde_json::from_value(serde_json::json!({"name":"", "transport":"http"}))
                .expect("backend defaults");
        let mut startup = serde_json::to_value(config).expect("serializable config");
        startup.sort_all_objects();
        Self(Arc::new(Inner {
            startup,
            connections: crate::lifecycle::Connections::new(),
            plain_policy: policy_projection(
                serde_json::to_value(plain).expect("serializable backend"),
            ),
            status: Mutex::new(ReloadStatus {
                watcher_active: false,
                last_result: "startup",
                applied: true,
                restart_required: false,
            }),
        }))
    }
    pub fn connections(&self) -> &crate::lifecycle::Connections {
        &self.0.connections
    }
    pub fn status(&self) -> ReloadStatus {
        self.0.status.lock().expect("reload status").clone()
    }
    pub fn started(&self) {
        self.0.status.lock().expect("reload status").watcher_active = true;
    }
    pub fn stopped(&self) {
        let mut status = self.0.status.lock().expect("reload status");
        status.watcher_active = false;
        status.last_result = "stopped";
    }
    pub fn record(&self, result: &'static str, applied: bool, restart_required: bool) {
        let mut status = self.0.status.lock().expect("reload status");
        status.last_result = result;
        status.applied = applied;
        status.restart_required = restart_required;
    }
    pub fn fingerprints(&self) -> HashMap<String, String> {
        backends(&self.0.startup)
            .into_iter()
            .map(|(name, backend)| {
                (
                    name.to_string(),
                    serde_json::to_string(backend).expect("backend fingerprint"),
                )
            })
            .collect()
    }
    /// Protected backends must retain their startup services, handles and fleet state.
    pub fn check_backend_change(&self, name: &str) -> anyhow::Result<()> {
        // A plain backend can still be the target of startup routing/composites.
        let referenced = backends(&self.0.startup).values().any(|backend| {
            ["mirror_of", "failover_for", "canary_of"]
                .iter()
                .any(|field| backend[*field].as_str() == Some(name))
        });
        let prefix = format!(
            "{}{}",
            name,
            self.0.startup["proxy"]["separator"]
                .as_str()
                .expect("separator")
        );
        let composite_target = self.0.startup["composite_tools"]
            .as_array()
            .expect("composites")
            .iter()
            .any(|composite| {
                composite["tools"]
                    .as_array()
                    .expect("targets")
                    .iter()
                    .any(|tool| tool.as_str().is_some_and(|tool| tool.starts_with(&prefix)))
            });
        if referenced || composite_target {
            anyhow::bail!(RESTART);
        }
        if let Some(backend) = backends(&self.0.startup).get(name)
            && policy_projection((*backend).clone()) != self.0.plain_policy
        {
            anyhow::bail!(RESTART);
        }
        Ok(())
    }
    pub fn check(&self, desired: &ProxyConfig) -> anyhow::Result<()> {
        let desired = serde_json::to_value(desired).expect("serializable config");
        let mut startup_global = self.0.startup.clone();
        let mut desired_global = desired.clone();
        startup_global.as_object_mut().unwrap().remove("backends");
        desired_global.as_object_mut().unwrap().remove("backends");
        if startup_global != desired_global {
            anyhow::bail!(RESTART);
        }
        let old = backends(&self.0.startup);
        let new = backends(&desired);
        for (name, backend) in &old {
            if new.get(name).copied() != Some(*backend) {
                self.check_backend_change(name)?;
            }
        }
        for (name, backend) in new {
            if old.get(name).copied() != Some(backend) {
                self.check_backend_change(name)?;
                if policy_projection(backend.clone()) != self.0.plain_policy {
                    anyhow::bail!(RESTART);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config() -> ProxyConfig {
        ProxyConfig::parse("[proxy]\nname='admission'\n[proxy.listen]\n[[backends]]\nname='api'\ntransport='http'\nurl='http://localhost:9000'").unwrap()
    }
    fn typed(value: Value) -> ProxyConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn every_backend_policy_field_requires_restart_for_edits_and_new_backends() {
        let base = serde_json::to_value(config()).unwrap();
        let mutations = json!({
            "timeout":{"seconds":1}, "circuit_breaker":{"minimum_calls":1},
            "rate_limit":{"requests":1}, "concurrency":{"max_concurrent":1},
            "retry":{"max_retries":1}, "outlier_detection":{"consecutive_errors":1},
            "hedging":{"delay_ms":1}, "mirror_of":"api", "mirror_percent":80,
            "cache":{"tool_ttl_seconds":1}, "forward_auth":true,
            "aliases":[{"from":"ping","to":"pong"}], "default_args":{"x":1},
            "inject_args":[{"tool":"ping","args":{"x":1}}],
            "param_overrides":[{"tool":"ping","hide":["x"]}],
            "expose_tools":["ping"], "hide_tools":["ping"],
            "expose_resources":["fixture://x"], "hide_resources":["fixture://x"],
            "expose_prompts":["prompt"], "hide_prompts":["prompt"],
            "hide_destructive":true, "read_only_only":true,
            "failover_for":"api", "priority":1, "canary_of":"api", "weight":20
        });
        let expected: std::collections::BTreeSet<_> = base["backends"][0]
            .as_object()
            .unwrap()
            .keys()
            .filter(|name| !CONNECTION_FIELDS.contains(&name.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            expected,
            mutations.as_object().unwrap().keys().cloned().collect()
        );
        for (field, value) in mutations.as_object().unwrap() {
            let plain = config();
            let policy = ReloadPolicy::new(&plain);
            let mut desired = base.clone();
            desired["backends"][0][field] = value.clone();
            let protected = typed(desired.clone());
            assert!(policy.check(&protected).is_err(), "edit {field}");
            let mut added = base.clone();
            desired["backends"][0]["name"] = json!("new");
            added["backends"]
                .as_array_mut()
                .unwrap()
                .push(desired["backends"][0].clone());
            assert!(policy.check(&typed(added)).is_err(), "new backend {field}");
            let startup = ReloadPolicy::new(&protected);
            assert!(startup.check(&protected).is_ok(), "unchanged {field}");
            assert!(startup.check_backend_change("api").is_err());
            let mut changed = serde_json::to_value(&protected).unwrap();
            changed["backends"][0]["url"] = json!("http://localhost:9001");
            assert!(startup.check(&typed(changed)).is_err(), "reconnect {field}");
            let mut removed = serde_json::to_value(&protected).unwrap();
            removed["backends"] = json!([]);
            assert!(startup.check(&typed(removed)).is_err(), "remove {field}");
        }
    }

    #[test]
    fn every_global_section_is_frozen_and_errors_do_not_echo_values() {
        let plain = config();
        let base = serde_json::to_value(&plain).unwrap();
        let policy = ReloadPolicy::new(&plain);
        for section in base
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| key.as_str() != "backends")
        {
            let mut value = base.clone();
            match section.as_str() {
                "proxy" => value[section]["name"] = json!("sentinel-secret"),
                "auth" => value[section] = json!({"type":"bearer","tokens":["sentinel-secret"]}),
                "performance" => value[section]["coalesce_requests"] = json!(true),
                "security" => value[section]["admin_token"] = json!("sentinel-secret"),
                "cache" => value[section]["prefix"] = json!("sentinel-secret"),
                "observability" => value[section]["audit"] = json!(true),
                "composite_tools" => {
                    value[section] =
                        json!([{"name":"bundle","description":"","tools":["api/ping"]}])
                }
                other => panic!("new section {other} needs an admission test"),
            }
            let error = policy.check(&typed(value)).unwrap_err();
            assert!(error.to_string().contains("Restart required"));
            assert!(!format!("{error:#} {error:?}").contains("sentinel-secret"));
        }
    }

    #[test]
    fn connection_only_edits_and_plain_membership_changes_remain_live() {
        let plain = config();
        let policy = ReloadPolicy::new(&plain);
        let mut value = serde_json::to_value(&plain).unwrap();
        value["backends"][0]["url"] = json!("https://new.example.test");
        value["backends"][0]["headers"] = json!({"X-Key":"sentinel"});
        assert!(policy.check(&typed(value.clone())).is_ok());
        value["backends"][0]["name"] = json!("new");
        assert!(policy.check(&typed(value)).is_ok());
        let mut reversed = serde_json::to_value(plain).unwrap();
        let mut new = reversed["backends"][0].clone();
        new["name"] = json!("new");
        reversed["backends"].as_array_mut().unwrap().insert(0, new);
        assert!(policy.check(&typed(reversed)).is_ok());
    }

    #[test]
    fn plain_targets_of_routing_or_composites_are_protected() {
        for field in ["mirror_of", "failover_for", "canary_of", "composite_tools"] {
            let mut value = serde_json::to_value(config()).unwrap();
            if field == "composite_tools" {
                value[field] = json!([{"name":"bundle","description":"","tools":["api/ping"]}]);
            } else {
                let mut route = value["backends"][0].clone();
                route["name"] = json!("route");
                route[field] = json!("api");
                value["backends"].as_array_mut().unwrap().push(route);
            }
            let policy = ReloadPolicy::new(&typed(value));
            assert!(policy.check_backend_change("api").is_err(), "{field}");
            assert!(policy.check_backend_change("unrelated").is_ok());
        }
    }

    #[test]
    fn reload_status_never_exposes_the_startup_snapshot() {
        let mut config = config();
        config.security.admin_token = Some("sentinel-secret".into());
        let policy = ReloadPolicy::new(&config);
        policy.started();
        policy.record("restart_required", false, true);
        let status = serde_json::to_string(&policy.status()).unwrap();
        assert!(!status.contains("sentinel"));
        assert!(policy.status().watcher_active);
        assert!(!policy.status().applied);
        policy.stopped();
        assert!(!policy.status().watcher_active);
    }
}
