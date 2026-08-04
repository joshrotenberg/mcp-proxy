//! Property tests for config parsing and validation (#213).
//!
//! Configs are generated as TOML strings and driven through
//! [`ProxyConfig::parse`], the same path `--check` and startup use. Case
//! counts are capped to keep the suite fast.

use mcp_proxy::ProxyConfig;
use proptest::prelude::*;

/// A lowercase identifier that is safe to embed in TOML without escaping.
fn ident() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,11}"
}

fn minimal_backend(name: &str) -> String {
    format!(
        r#"
[[backends]]
name = "{name}"
transport = "stdio"
command = "echo"
"#
    )
}

fn base_config(proxy_name: &str, port: u16) -> String {
    format!(
        r#"
[proxy]
name = "{proxy_name}"
[proxy.listen]
host = "127.0.0.1"
port = {port}
"#
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Parse never panics, whatever the input. Errors are fine; panics are not.
    #[test]
    fn parse_never_panics_on_arbitrary_input(s in ".{0,256}") {
        let _ = ProxyConfig::parse(&s);
    }

    /// Parse never panics on TOML-shaped input either: a valid prefix with an
    /// arbitrary tail exercises deeper parser states.
    #[test]
    fn parse_never_panics_on_toml_shaped_input(
        name in ident(),
        port in 1u16..,
        tail in ".{0,200}",
    ) {
        let toml = format!("{}{}{tail}", base_config(&name, port), minimal_backend("b"));
        let _ = ProxyConfig::parse(&toml);
    }

    /// A generated valid config round-trips: parse -> serialize -> parse ->
    /// serialize is a fixed point (compared via the second serialization).
    #[test]
    fn valid_config_roundtrips(
        proxy_name in ident(),
        port in 1u16..,
        backend_count in 1usize..4,
        timeout_secs in 1u64..600,
    ) {
        let mut toml = base_config(&proxy_name, port);
        for i in 0..backend_count {
            toml.push_str(&minimal_backend(&format!("b{i}")));
            toml.push_str(&format!("[backends.timeout]\nseconds = {timeout_secs}\n"));
        }
        let parsed = ProxyConfig::parse(&toml).expect("generated config is valid");
        let ser1 = toml::to_string(&parsed).expect("config serializes");
        let reparsed = ProxyConfig::parse(&ser1).expect("serialized config parses");
        let ser2 = toml::to_string(&reparsed).expect("config re-serializes");
        prop_assert_eq!(ser1, ser2);
    }

    /// mirror_of, canary_of, and failover_for referencing an unknown backend
    /// are rejected, whatever the names involved.
    #[test]
    fn unknown_backend_references_are_rejected(
        primary in ident(),
        ghost in ident(),
        role in 0usize..3,
    ) {
        prop_assume!(primary != ghost);
        let field = ["mirror_of", "canary_of", "failover_for"][role];
        let extra = if field == "mirror_of" { "mirror_percent = 10\n" } else { "" };
        let toml = format!(
            "{}{}{}{field} = \"{ghost}\"\n{extra}",
            base_config("p", 8080),
            minimal_backend(&primary),
            minimal_backend("secondary"),
        );
        let err = ProxyConfig::parse(&toml).expect_err("unknown reference must be rejected");
        prop_assert!(
            format!("{err:#}").contains("unknown backend"),
            "unexpected error: {err:#}"
        );
    }

    /// Self-references are rejected for all three roles.
    #[test]
    fn self_references_are_rejected(name in ident(), role in 0usize..3) {
        let field = ["mirror_of", "canary_of", "failover_for"][role];
        let extra = if field == "mirror_of" { "mirror_percent = 10\n" } else { "" };
        let toml = format!(
            "{}{}{field} = \"{name}\"\n{extra}",
            base_config("p", 8080),
            minimal_backend(&name),
        );
        let err = ProxyConfig::parse(&toml).expect_err("self reference must be rejected");
        prop_assert!(
            format!("{err:#}").contains("itself"),
            "unexpected error: {err:#}"
        );
    }

    /// The documented canary weight bound (1-100) is enforced.
    #[test]
    fn canary_weight_out_of_bounds_is_rejected(weight in 101u32..10_000) {
        let toml = format!(
            "{}{}{}canary_of = \"primary\"\nweight = {weight}\n",
            base_config("p", 8080),
            minimal_backend("primary"),
            minimal_backend("canary"),
        );
        let result = ProxyConfig::parse(&toml);
        prop_assert!(
            result.is_err(),
            "weight {weight} is outside the documented 1-100 range and must be rejected"
        );
    }

    /// The documented mirror_percent bound (0-100) is enforced.
    #[test]
    fn mirror_percent_out_of_bounds_is_rejected(percent in 101u32..10_000) {
        let toml = format!(
            "{}{}{}mirror_of = \"primary\"\nmirror_percent = {percent}\n",
            base_config("p", 8080),
            minimal_backend("primary"),
            minimal_backend("mirror"),
        );
        let result = ProxyConfig::parse(&toml);
        prop_assert!(
            result.is_err(),
            "mirror_percent {percent} is outside 0-100 and must be rejected"
        );
    }

    /// Duplicate backend names are rejected.
    #[test]
    fn duplicate_backend_names_are_rejected(name in ident()) {
        let toml = format!(
            "{}{}{}",
            base_config("p", 8080),
            minimal_backend(&name),
            minimal_backend(&name),
        );
        let result = ProxyConfig::parse(&toml);
        prop_assert!(result.is_err(), "duplicate backend name '{}' must be rejected", name);
    }
}
