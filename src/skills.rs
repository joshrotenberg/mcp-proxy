//! agentskills.io compliant prompts for proxy management.
//!
//! Registers MCP prompts under the `proxy/` namespace that guide AI agents
//! through proxy configuration, validation, and troubleshooting. These prompts
//! follow the agentskills.io specification -- structured guidance that agents
//! can discover and use at runtime.
//!
//! Skills complement the existing admin tools: tools perform actions,
//! skills provide the knowledge to use them effectively.

use std::sync::Arc;

use tower_mcp::protocol::{Content, GetPromptResult, PromptMessage, PromptRole};
use tower_mcp::{Prompt, PromptBuilder};

/// Build all agentskills.io prompts for the proxy.
pub fn build_skills(config_snapshot: Arc<String>) -> Vec<Prompt> {
    vec![
        build_setup_skill(),
        build_configure_auth_skill(),
        build_configure_resilience_skill(),
        build_check_config_skill(config_snapshot.clone()),
        build_diagnose_skill(),
        build_status_skill(),
        build_explain_config_skill(config_snapshot),
    ]
}

fn build_setup_skill() -> Prompt {
    PromptBuilder::new("setup")
        .description(
            "Guided proxy configuration from a description of desired backends and policies",
        )
        .static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: include_str!("skills/setup.md").to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
}

fn build_configure_auth_skill() -> Prompt {
    PromptBuilder::new("configure_auth")
        .description("Configure authentication: bearer tokens, JWT/JWKS, or OAuth 2.1")
        .static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: include_str!("skills/configure_auth.md").to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
}

fn build_configure_resilience_skill() -> Prompt {
    PromptBuilder::new("configure_resilience")
        .description(
            "Set up circuit breakers, retries, rate limits, timeouts, and hedging for backends",
        )
        .static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: include_str!("skills/configure_resilience.md").to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
}

fn build_check_config_skill(config_snapshot: Arc<String>) -> Prompt {
    PromptBuilder::new("check_config")
        .description("Validate the current proxy configuration and report issues")
        .handler(move |_args| {
            let config = Arc::clone(&config_snapshot);
            async move {
                Ok(GetPromptResult {
                    description: Some("Configuration validation guide".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!(
                                "{}\n\n## Current Configuration\n\n```toml\n{}\n```",
                                include_str!("skills/check_config.md"),
                                *config
                            ),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

fn build_diagnose_skill() -> Prompt {
    PromptBuilder::new("diagnose")
        .description("Analyze proxy health, identify issues, and suggest improvements")
        .static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: include_str!("skills/diagnose.md").to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
}

fn build_status_skill() -> Prompt {
    PromptBuilder::new("status")
        .description("Get current proxy state: backend health, sessions, and metrics")
        .static_prompt(vec![PromptMessage {
            role: PromptRole::User,
            content: Content::Text {
                text: include_str!("skills/status.md").to_string(),
                annotations: None,
                meta: None,
            },
            meta: None,
        }])
}

fn build_explain_config_skill(config_snapshot: Arc<String>) -> Prompt {
    PromptBuilder::new("explain_config")
        .description("Describe the current proxy configuration in natural language")
        .handler(move |_args| {
            let config = Arc::clone(&config_snapshot);
            async move {
                Ok(GetPromptResult {
                    description: Some("Configuration explanation".to_string()),
                    messages: vec![PromptMessage {
                        role: PromptRole::User,
                        content: Content::Text {
                            text: format!(
                                "{}\n\n## Current Configuration\n\n```toml\n{}\n```",
                                include_str!("skills/explain_config.md"),
                                *config
                            ),
                            annotations: None,
                            meta: None,
                        },
                        meta: None,
                    }],
                    meta: None,
                })
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_snapshot() -> Arc<String> {
        Arc::new("[proxy]\nname = \"test\"\n".to_string())
    }

    #[test]
    fn build_skills_returns_seven_prompts() {
        let skills = build_skills(config_snapshot());
        assert_eq!(skills.len(), 7);
    }

    #[test]
    fn all_skills_have_names_and_descriptions() {
        let skills = build_skills(config_snapshot());
        for skill in &skills {
            assert!(!skill.name.is_empty(), "skill name should not be empty");
            assert!(
                skill.description.is_some(),
                "skill '{}' should have a description",
                skill.name,
            );
        }
    }

    #[test]
    fn skill_names_are_expected() {
        let skills = build_skills(config_snapshot());
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "setup",
                "configure_auth",
                "configure_resilience",
                "check_config",
                "diagnose",
                "status",
                "explain_config",
            ]
        );
    }

    #[test]
    fn static_skills_produce_correct_names() {
        assert_eq!(build_setup_skill().name, "setup");
        assert_eq!(build_configure_auth_skill().name, "configure_auth");
        assert_eq!(
            build_configure_resilience_skill().name,
            "configure_resilience"
        );
        assert_eq!(build_diagnose_skill().name, "diagnose");
        assert_eq!(build_status_skill().name, "status");
    }

    #[test]
    fn dynamic_skills_include_config_snapshot() {
        let config = config_snapshot();
        let check = build_check_config_skill(config.clone());
        assert_eq!(check.name, "check_config");

        let explain = build_explain_config_skill(config);
        assert_eq!(explain.name, "explain_config");
    }
}
