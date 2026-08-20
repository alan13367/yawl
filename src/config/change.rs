use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::{
    Config, OPENAI_COMPLETIONS_API, ProviderConfig, expand_home_path, object_field,
    validate_provider_name,
};
use crate::error::Error;

/// One requested change to the global configuration.
pub(crate) enum ConfigChange {
    Reload,
    Model(String),
    MaxTokens(String),
    ReasoningEffort(String),
    HideReasoning(String),
    AutoCompact(String),
    CompactThreshold(String),
    ContextWindow {
        model: String,
        value: String,
    },
    SkillDirectory {
        action: SkillDirectoryAction,
        path: String,
    },
    Provider {
        name: String,
        base_url: String,
        api_key: Option<String>,
    },
    AnthropicBaseUrl(String),
    OpenAiBaseUrl(String),
}

#[derive(Clone, Copy)]
pub(crate) enum SkillDirectoryAction {
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigChangeEffect {
    Applied,
    Overridden,
    SkillDirectoryNotConfigured(PathBuf),
}

pub(crate) struct ConfigChangeOutcome {
    pub(crate) config: Config,
    pub(crate) effect: ConfigChangeEffect,
}

enum ValidatedChange {
    Reload,
    Model(String),
    MaxTokens(u32),
    ReasoningEffort {
        stored: String,
        effective: Option<String>,
    },
    HideReasoning(bool),
    AutoCompact(bool),
    CompactThreshold(f64),
    ContextWindow {
        model: String,
        window: u64,
    },
    SkillDirectories(Vec<PathBuf>),
    SkillDirectoryNotConfigured(PathBuf),
    Provider {
        name: String,
        base_url: String,
        api_key: Option<String>,
    },
    AnthropicBaseUrl(String),
    OpenAiBaseUrl(String),
}

impl Config {
    /// Applies one validated global change, then reloads the merged effective
    /// configuration. The result says whether a project value overrode it.
    pub(crate) fn change_global(&self, change: ConfigChange) -> Result<ConfigChangeOutcome, Error> {
        let change = ValidatedChange::parse(self, change)?;
        change.persist(self)?;
        let config = self.reload()?;
        let effect = match &change {
            ValidatedChange::SkillDirectoryNotConfigured(path) => {
                ConfigChangeEffect::SkillDirectoryNotConfigured(path.clone())
            }
            _ if change.is_effective(&config) => ConfigChangeEffect::Applied,
            _ => ConfigChangeEffect::Overridden,
        };
        Ok(ConfigChangeOutcome { config, effect })
    }
}

impl ValidatedChange {
    fn parse(config: &Config, change: ConfigChange) -> Result<Self, Error> {
        match change {
            ConfigChange::Reload => Ok(Self::Reload),
            ConfigChange::Model(model) => {
                if model.trim().is_empty() {
                    Err(Error::Config("model name must not be empty".into()))
                } else {
                    Ok(Self::Model(model))
                }
            }
            ConfigChange::MaxTokens(value) => {
                let tokens = value
                    .parse::<u32>()
                    .map_err(|_| Error::Config("max_tokens must be a positive integer".into()))?;
                if tokens == 0 {
                    Err(Error::Config(
                        "max_tokens must be a positive integer".into(),
                    ))
                } else {
                    Ok(Self::MaxTokens(tokens))
                }
            }
            ConfigChange::ReasoningEffort(value) => {
                let effective = match value.as_str() {
                    "default" | "off" => None,
                    "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Some(value.clone()),
                    _ => return Err(Error::Config("unsupported reasoning effort".into())),
                };
                Ok(Self::ReasoningEffort {
                    stored: value,
                    effective,
                })
            }
            ConfigChange::HideReasoning(value) => Ok(Self::HideReasoning(parse_on_off(&value)?)),
            ConfigChange::AutoCompact(value) => Ok(Self::AutoCompact(parse_on_off(&value)?)),
            ConfigChange::CompactThreshold(value) => {
                Ok(Self::CompactThreshold(parse_threshold(&value)?))
            }
            ConfigChange::ContextWindow { model, value } => {
                if model.trim().is_empty() {
                    return Err(Error::Config("model name must not be empty".into()));
                }
                let window = value.parse::<u64>().map_err(|_| {
                    Error::Config("context_window must be a positive integer".into())
                })?;
                if window == 0 {
                    return Err(Error::Config(
                        "context_window must be a positive integer".into(),
                    ));
                }
                Ok(Self::ContextWindow { model, window })
            }
            ConfigChange::SkillDirectory { action, path } => {
                if path.trim().is_empty() {
                    return Err(Error::Config("skill directory must not be empty".into()));
                }
                let path = expand_home_path(&path, &config.home_dir);
                let mut dirs = config.skill_dirs.clone();
                match action {
                    SkillDirectoryAction::Add if !dirs.contains(&path) => dirs.push(path),
                    SkillDirectoryAction::Add => {}
                    SkillDirectoryAction::Remove => {
                        let Some(index) = dirs.iter().position(|dir| dir == &path) else {
                            return Ok(Self::SkillDirectoryNotConfigured(path));
                        };
                        dirs.remove(index);
                    }
                }
                Ok(Self::SkillDirectories(dirs))
            }
            ConfigChange::Provider {
                name,
                base_url,
                api_key,
            } => {
                validate_provider_name(&name)?;
                if matches!(name.as_str(), "anthropic" | "openai") {
                    return Err(Error::Config(format!(
                        "'{name}' is built in; use anthropic_base_url or openai_base_url"
                    )));
                }
                validate_http_url(&base_url)?;
                Ok(Self::Provider {
                    name,
                    base_url,
                    api_key,
                })
            }
            ConfigChange::AnthropicBaseUrl(url) => {
                validate_http_url(&url)?;
                Ok(Self::AnthropicBaseUrl(url))
            }
            ConfigChange::OpenAiBaseUrl(url) => {
                validate_http_url(&url)?;
                Ok(Self::OpenAiBaseUrl(url))
            }
        }
    }

    fn persist(&self, config: &Config) -> Result<(), Error> {
        match self {
            Self::Reload => Ok(()),
            Self::Model(model) => insert_scalar(config, "model", json!(model)),
            Self::MaxTokens(tokens) => insert_scalar(config, "max_tokens", json!(tokens)),
            Self::ReasoningEffort { stored, .. } => {
                insert_scalar(config, "reasoning_effort", json!(stored))
            }
            Self::HideReasoning(hidden) => insert_scalar(config, "hide_reasoning", json!(hidden)),
            Self::AutoCompact(enabled) => insert_scalar(config, "auto_compact", json!(enabled)),
            Self::CompactThreshold(threshold) => {
                insert_scalar(config, "compact_threshold", json!(threshold))
            }
            Self::ContextWindow { model, window } => config.update_global_json(|root| {
                object_field(root, "context_windows")?.insert(model.clone(), json!(window));
                Ok(())
            }),
            Self::SkillDirectories(dirs) => {
                let home = config.home_dir.parent();
                let values = dirs
                    .iter()
                    .map(|dir| {
                        home.and_then(|home| dir.strip_prefix(home).ok())
                            .map_or_else(
                                || dir.display().to_string(),
                                |relative| format!("~/{}", relative.display()),
                            )
                    })
                    .collect::<Vec<_>>();
                insert_scalar(config, "skill_dirs", json!(values))
            }
            Self::SkillDirectoryNotConfigured(_) => Ok(()),
            Self::Provider {
                name,
                base_url,
                api_key,
            } => config.update_global_json(|root| {
                let providers = object_field(root, "providers")?;
                let provider = providers
                    .entry(name.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                let Value::Object(provider) = provider else {
                    return Err(Error::Config(format!(
                        "providers.{name} must be a JSON object"
                    )));
                };
                provider.insert("base_url".into(), json!(base_url));
                provider.insert("api".into(), json!(OPENAI_COMPLETIONS_API));
                provider.remove("baseUrl");
                if let Some(api_key) = api_key {
                    provider.remove("apiKey");
                    if api_key == "-" {
                        provider.remove("api_key");
                        provider.remove("auth_header");
                        provider.remove("authHeader");
                    } else {
                        provider.insert("api_key".into(), json!(api_key));
                        provider.insert("auth_header".into(), json!(true));
                    }
                }
                Ok(())
            }),
            Self::AnthropicBaseUrl(url) => insert_scalar(config, "anthropic_base_url", json!(url)),
            Self::OpenAiBaseUrl(url) => insert_scalar(config, "openai_base_url", json!(url)),
        }
    }

    fn is_effective(&self, config: &Config) -> bool {
        match self {
            Self::Reload => true,
            Self::Model(model) => config.model.as_deref() == Some(model),
            Self::MaxTokens(tokens) => config.max_tokens == *tokens,
            Self::ReasoningEffort { effective, .. } => config.reasoning_effort == *effective,
            Self::HideReasoning(hidden) => config.hide_reasoning == *hidden,
            Self::AutoCompact(enabled) => config.auto_compact == *enabled,
            Self::CompactThreshold(threshold) => config.compact_threshold == *threshold,
            Self::ContextWindow { model, window } => {
                config.context_windows.get(model) == Some(window)
            }
            Self::SkillDirectories(dirs) => config.skill_dirs == *dirs,
            Self::SkillDirectoryNotConfigured(_) => true,
            Self::Provider {
                name,
                base_url,
                api_key,
            } => config.providers.get(name).is_some_and(|provider| {
                provider.base_url == *base_url && api_key_is_effective(provider, api_key.as_deref())
            }),
            Self::AnthropicBaseUrl(url) => config.anthropic_base_url == *url,
            Self::OpenAiBaseUrl(url) => config.openai_base_url == *url,
        }
    }
}

fn insert_scalar(config: &Config, key: &str, value: Value) -> Result<(), Error> {
    config.update_global_json(|root| {
        root.insert(key.to_string(), value);
        Ok(())
    })
}

fn api_key_is_effective(provider: &ProviderConfig, requested: Option<&str>) -> bool {
    match requested {
        None => true,
        Some("-") => provider.api_key.is_none(),
        Some(value) => provider.api_key.as_deref() == Some(value),
    }
}

fn parse_on_off(value: &str) -> Result<bool, Error> {
    match value {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err(Error::Config("expected on or off".into())),
    }
}

fn parse_threshold(value: &str) -> Result<f64, Error> {
    let threshold = if let Some(percent) = value.strip_suffix('%') {
        percent
            .parse::<f64>()
            .map_err(|_| Error::Config("invalid compaction percentage".into()))?
            / 100.0
    } else {
        value
            .parse::<f64>()
            .map_err(|_| Error::Config("invalid compaction threshold".into()))?
    };
    if !threshold.is_finite() || !(0.1..=0.99).contains(&threshold) {
        return Err(Error::Config(
            "compact_threshold must be between 0.1 and 0.99".into(),
        ));
    }
    Ok(threshold)
}

fn validate_http_url(url: &str) -> Result<(), Error> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(Error::Config(
            "provider URL must start with http:// or https://".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirs {
        root: PathBuf,
        home: PathBuf,
        project: PathBuf,
    }

    impl TestDirs {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "yawl-config-change-{}-{nonce}-{name}",
                std::process::id()
            ));
            Self {
                home: root.join("home/.yawl"),
                project: root.join("project/.yawl"),
                root,
            }
        }

        fn config(&self) -> Config {
            Config::load_from(self.home.clone(), self.project.clone())
                .expect("test config should load")
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn change_preserves_unrelated_json_and_reloads_effective_value() {
        let dirs = TestDirs::new("preserve");
        fs::create_dir_all(&dirs.home).expect("home config directory should be created");
        fs::write(
            dirs.home.join("config.json"),
            r#"{"unknown":{"keep":true},"max_tokens":100}"#,
        )
        .expect("global config should be written");
        let config = dirs.config();

        let outcome = config
            .change_global(ConfigChange::MaxTokens("2048".into()))
            .expect("valid change should apply");

        assert_eq!(outcome.effect, ConfigChangeEffect::Applied);
        assert_eq!(outcome.config.max_tokens, 2048);
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved config should be readable"),
        )
        .expect("saved config should remain JSON");
        assert_eq!(saved["unknown"]["keep"], true);
    }

    #[test]
    fn change_reports_when_project_config_overrides_global_value() {
        let dirs = TestDirs::new("override");
        fs::create_dir_all(&dirs.project).expect("project config directory should be created");
        fs::write(dirs.project.join("config.json"), r#"{"max_tokens":4096}"#)
            .expect("project config should be written");
        let config = dirs.config();

        let outcome = config
            .change_global(ConfigChange::MaxTokens("2048".into()))
            .expect("global value should still be saved");

        assert_eq!(outcome.effect, ConfigChangeEffect::Overridden);
        assert_eq!(outcome.config.max_tokens, 4096);
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved global config should be readable"),
        )
        .expect("saved global config should remain JSON");
        assert_eq!(saved["max_tokens"], 2048);
    }

    #[test]
    fn invalid_change_does_not_create_global_config() {
        let dirs = TestDirs::new("invalid");
        let config = dirs.config();

        let error = config
            .change_global(ConfigChange::CompactThreshold("5%".into()))
            .err()
            .expect("invalid threshold should fail");

        assert!(error.to_string().contains("between 0.1 and 0.99"));
        assert!(!dirs.home.join("config.json").exists());
    }

    #[test]
    fn removing_an_unknown_skill_directory_is_a_non_persisting_outcome() {
        let dirs = TestDirs::new("unknown-skill-directory");
        let config = dirs.config();
        let missing = dirs.root.join("missing-skills");

        let outcome = config
            .change_global(ConfigChange::SkillDirectory {
                action: SkillDirectoryAction::Remove,
                path: missing.display().to_string(),
            })
            .expect("missing directory should be reported without failing");

        assert_eq!(
            outcome.effect,
            ConfigChangeEffect::SkillDirectoryNotConfigured(missing)
        );
        assert!(!dirs.home.join("config.json").exists());
    }
}
