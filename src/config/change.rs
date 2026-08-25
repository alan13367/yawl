use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::{
    Config, MAX_SUBAGENT_REQUEST_BUDGET, MAX_SUBAGENT_TIMEOUT_SECS, OPENAI_COMPLETIONS_API,
    ProviderConfig, UiColor, expand_home_path, object_field, parse_bounded, validate_bounded,
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
    AccentColor(String),
    ScrollBar(String),
    ScrollBarAutoHide(String),
    AutoCompact(String),
    CompactThreshold(String),
    Subagents(String),
    MaxSubagents(String),
    SubagentModel(String),
    SubagentRequestBudget(String),
    SubagentTimeoutSecs(String),
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
    AnthropicApiKey(String),
    OpenAiApiKey(String),
    SetupSkipped(bool),
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
    AccentColor(UiColor),
    ScrollBar(bool),
    ScrollBarAutoHide(bool),
    AutoCompact(bool),
    CompactThreshold(f64),
    Subagents(bool),
    MaxSubagents(usize),
    SubagentModel(String),
    SubagentRequestBudget(usize),
    SubagentTimeoutSecs(u64),
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
    AnthropicApiKey(Option<String>),
    OpenAiApiKey(Option<String>),
    SetupSkipped(bool),
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
            ConfigChange::AccentColor(value) => UiColor::parse(&value)
                .map(Self::AccentColor)
                .map_err(Error::Config),
            ConfigChange::ScrollBar(value) => Ok(Self::ScrollBar(parse_on_off(&value)?)),
            ConfigChange::ScrollBarAutoHide(value) => {
                Ok(Self::ScrollBarAutoHide(parse_on_off(&value)?))
            }
            ConfigChange::AutoCompact(value) => Ok(Self::AutoCompact(parse_on_off(&value)?)),
            ConfigChange::CompactThreshold(value) => {
                Ok(Self::CompactThreshold(parse_threshold(&value)?))
            }
            ConfigChange::Subagents(value) => Ok(Self::Subagents(parse_on_off(&value)?)),
            ConfigChange::MaxSubagents(value) => Ok(Self::MaxSubagents(parse_bounded(
                &value,
                1,
                16,
                "max_subagents",
            )?)),
            ConfigChange::SubagentModel(value) => {
                let value = value.trim();
                if value.is_empty() {
                    return Err(Error::Config("subagent_model must not be empty".into()));
                }
                Ok(Self::SubagentModel(value.to_string()))
            }
            ConfigChange::SubagentRequestBudget(value) => {
                Ok(Self::SubagentRequestBudget(parse_bounded(
                    &value,
                    0,
                    MAX_SUBAGENT_REQUEST_BUDGET,
                    "subagent_request_budget",
                )?))
            }
            ConfigChange::SubagentTimeoutSecs(value) => {
                Ok(Self::SubagentTimeoutSecs(parse_bounded(
                    &value,
                    0,
                    MAX_SUBAGENT_TIMEOUT_SECS,
                    "subagent_timeout_secs",
                )?))
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
            ConfigChange::AnthropicApiKey(value) => {
                Ok(Self::AnthropicApiKey(parse_builtin_api_key(&value)?))
            }
            ConfigChange::OpenAiApiKey(value) => {
                Ok(Self::OpenAiApiKey(parse_builtin_api_key(&value)?))
            }
            ConfigChange::SetupSkipped(skipped) => Ok(Self::SetupSkipped(skipped)),
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
            Self::AccentColor(color) => config.update_global_json(|root| {
                root.insert("accent_color".into(), json!(color.config_value()));
                root.remove("status_bar_color");
                root.remove("text_box_color");
                Ok(())
            }),
            Self::ScrollBar(enabled) => insert_scalar(config, "scroll_bar", json!(enabled)),
            Self::ScrollBarAutoHide(enabled) => {
                insert_scalar(config, "scroll_bar_auto_hide", json!(enabled))
            }
            Self::AutoCompact(enabled) => insert_scalar(config, "auto_compact", json!(enabled)),
            Self::CompactThreshold(threshold) => {
                insert_scalar(config, "compact_threshold", json!(threshold))
            }
            Self::Subagents(enabled) => insert_scalar(config, "subagents", json!(enabled)),
            Self::MaxSubagents(limit) => insert_scalar(config, "max_subagents", json!(limit)),
            Self::SubagentModel(model) => insert_scalar(config, "subagent_model", json!(model)),
            Self::SubagentRequestBudget(budget) => {
                insert_scalar(config, "subagent_request_budget", json!(budget))
            }
            Self::SubagentTimeoutSecs(timeout) => {
                insert_scalar(config, "subagent_timeout_secs", json!(timeout))
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
            Self::AnthropicApiKey(key) => match key {
                Some(key) => insert_scalar(config, "anthropic_api_key", json!(key)),
                None => remove_scalar(config, "anthropic_api_key"),
            },
            Self::OpenAiApiKey(key) => match key {
                Some(key) => insert_scalar(config, "openai_api_key", json!(key)),
                None => remove_scalar(config, "openai_api_key"),
            },
            Self::SetupSkipped(skipped) => {
                if *skipped {
                    insert_scalar(config, "setup", json!("skipped"))
                } else {
                    remove_scalar(config, "setup")
                }
            }
        }
    }

    fn is_effective(&self, config: &Config) -> bool {
        match self {
            Self::Reload => true,
            Self::Model(model) => config.model.as_deref() == Some(model),
            Self::MaxTokens(tokens) => config.max_tokens == *tokens,
            Self::ReasoningEffort { effective, .. } => config.reasoning_effort == *effective,
            Self::HideReasoning(hidden) => config.hide_reasoning == *hidden,
            Self::AccentColor(color) => config.accent_color == *color,
            Self::ScrollBar(enabled) => config.scroll_bar == *enabled,
            Self::ScrollBarAutoHide(enabled) => config.scroll_bar_auto_hide == *enabled,
            Self::AutoCompact(enabled) => config.auto_compact == *enabled,
            Self::CompactThreshold(threshold) => config.compact_threshold == *threshold,
            Self::Subagents(enabled) => config.subagents == *enabled,
            Self::MaxSubagents(limit) => config.max_subagents == *limit,
            Self::SubagentModel(model) => config.subagent_model == *model,
            Self::SubagentRequestBudget(budget) => config.subagent_request_budget == *budget,
            Self::SubagentTimeoutSecs(timeout) => config.subagent_timeout_secs == *timeout,
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
            Self::AnthropicApiKey(key) => config.anthropic_api_key == *key,
            Self::OpenAiApiKey(key) => config.openai_api_key == *key,
            Self::SetupSkipped(skipped) => config.setup_skipped == *skipped,
        }
    }
}

fn insert_scalar(config: &Config, key: &str, value: Value) -> Result<(), Error> {
    config.update_global_json(|root| {
        root.insert(key.to_string(), value);
        Ok(())
    })
}

fn remove_scalar(config: &Config, key: &str) -> Result<(), Error> {
    config.update_global_json(|root| {
        root.remove(key);
        Ok(())
    })
}

/// Validates a built-in API key. `-` removes the stored key; otherwise the
/// value may be a literal, a `$NAME` or `${NAME}` reference, or `-` to clear.
fn parse_builtin_api_key(value: &str) -> Result<Option<String>, Error> {
    if value == "-" {
        return Ok(None);
    }
    if value.trim().is_empty() {
        return Err(Error::Config(
            "API key must not be empty; use '-' to remove the stored key".into(),
        ));
    }
    if let Some(reference) = value.strip_prefix("${") {
        let name = reference
            .strip_suffix('}')
            .ok_or_else(|| Error::Config("unterminated environment variable reference".into()))?;
        ensure_environment_name(name)?;
    } else if let Some(name) = value.strip_prefix('$') {
        ensure_environment_name(name)?;
    } else if value.starts_with('!') {
        return Err(Error::Config(
            "API keys beginning with '!' are not supported; use an environment variable reference"
                .into(),
        ));
    }
    Ok(Some(value.to_string()))
}

fn ensure_environment_name(name: &str) -> Result<(), Error> {
    let mut bytes = name.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if valid_start && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        Ok(())
    } else {
        Err(Error::Config("invalid environment variable name".into()))
    }
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
    if !threshold.is_finite() {
        return Err(Error::Config(
            "compact_threshold must be between 0.1 and 0.99".into(),
        ));
    }
    validate_bounded(threshold, 0.1, 0.99, "compact_threshold")
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
    fn accent_color_change_is_validated_persisted_and_reloaded() {
        let dirs = TestDirs::new("color");
        fs::create_dir_all(&dirs.home).expect("home config directory should be created");
        fs::write(
            dirs.home.join("config.json"),
            r#"{"status_bar_color":"white","text_box_color":"blue"}"#,
        )
        .expect("legacy colors should be written");
        let config = dirs.config();

        let outcome = config
            .change_global(ConfigChange::AccentColor("#123abc".into()))
            .expect("valid RGB color should apply");

        assert_eq!(outcome.effect, ConfigChangeEffect::Applied);
        assert_eq!(outcome.config.accent_color, UiColor::new(0x12, 0x3a, 0xbc));
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved config should be readable"),
        )
        .expect("saved config should remain JSON");
        assert_eq!(saved["accent_color"], "#123abc");
        assert!(saved.get("status_bar_color").is_none());
        assert!(saved.get("text_box_color").is_none());

        assert!(
            outcome
                .config
                .change_global(ConfigChange::AccentColor("transparent".into()))
                .is_err()
        );
    }

    #[test]
    fn scroll_bar_change_is_validated_persisted_and_reloaded() {
        let dirs = TestDirs::new("scroll-bar");
        let config = dirs.config();
        assert!(config.scroll_bar);

        let outcome = config
            .change_global(ConfigChange::ScrollBar("off".into()))
            .expect("a valid on/off value should apply");

        assert_eq!(outcome.effect, ConfigChangeEffect::Applied);
        assert!(!outcome.config.scroll_bar);
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved config should be readable"),
        )
        .expect("saved config should remain JSON");
        assert_eq!(saved["scroll_bar"], false);

        let error = outcome
            .config
            .change_global(ConfigChange::ScrollBar("maybe".into()))
            .err()
            .expect("non-boolean values should fail validation");
        assert!(error.to_string().contains("on or off"));
    }

    #[test]
    fn scroll_bar_auto_hide_change_is_validated_persisted_and_reloaded() {
        let dirs = TestDirs::new("scroll-bar-auto-hide");
        let config = dirs.config();
        assert!(config.scroll_bar_auto_hide);

        let outcome = config
            .change_global(ConfigChange::ScrollBarAutoHide("off".into()))
            .expect("a valid on/off value should apply");

        assert_eq!(outcome.effect, ConfigChangeEffect::Applied);
        assert!(!outcome.config.scroll_bar_auto_hide);
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved config should be readable"),
        )
        .expect("saved config should remain JSON");
        assert_eq!(saved["scroll_bar_auto_hide"], false);

        let error = outcome
            .config
            .change_global(ConfigChange::ScrollBarAutoHide("maybe".into()))
            .err()
            .expect("non-boolean values should fail validation");
        assert!(error.to_string().contains("on or off"));
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

    #[test]
    fn builtin_key_changes_store_validate_and_reload() {
        let dirs = TestDirs::new("builtin-keys");
        let config = dirs.config();

        let stored = config
            .change_global(ConfigChange::AnthropicApiKey("sk-ant-test".into()))
            .expect("a literal anthropic key should apply");
        assert_eq!(stored.effect, ConfigChangeEffect::Applied);
        assert_eq!(
            stored.config.anthropic_api_key.as_deref(),
            Some("sk-ant-test")
        );

        let reference = stored
            .config
            .change_global(ConfigChange::OpenAiApiKey("$OPENAI_TEST_KEY".into()))
            .expect("an environment reference should apply");
        assert_eq!(
            reference.config.openai_api_key.as_deref(),
            Some("$OPENAI_TEST_KEY")
        );

        let removed = reference
            .config
            .change_global(ConfigChange::OpenAiApiKey("-".into()))
            .expect("removing the stored key should apply");
        assert_eq!(removed.config.openai_api_key, None);

        assert!(
            removed
                .config
                .change_global(ConfigChange::AnthropicApiKey("".into()))
                .is_err()
        );
        assert!(
            removed
                .config
                .change_global(ConfigChange::OpenAiApiKey("$2BAD".into()))
                .is_err()
        );
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved keys should be readable"),
        )
        .expect("saved config should be JSON");
        assert_eq!(saved["anthropic_api_key"], "sk-ant-test");
        assert!(saved.get("openai_api_key").is_none());
    }

    #[test]
    fn setup_marker_round_trips_and_clears() {
        let dirs = TestDirs::new("setup-marker");
        let config = dirs.config();
        assert!(!config.setup_skipped);

        let skipped = config
            .change_global(ConfigChange::SetupSkipped(true))
            .expect("skipping setup should persist");
        assert!(skipped.config.setup_skipped);

        let restored = skipped
            .config
            .change_global(ConfigChange::SetupSkipped(false))
            .expect("clearing the marker should persist");
        assert!(!restored.config.setup_skipped);

        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved marker config should be readable"),
        )
        .expect("saved config should be JSON");
        assert!(saved.get("setup").is_none());
    }

    #[test]
    fn subagent_changes_validate_persist_and_reload() {
        let dirs = TestDirs::new("subagents");
        let config = dirs.config();
        let enabled = config
            .change_global(ConfigChange::Subagents("on".into()))
            .expect("subagents should enable");
        let limited = enabled
            .config
            .change_global(ConfigChange::MaxSubagents("16".into()))
            .expect("maximum valid subagent limit should apply");
        let modeled = limited
            .config
            .change_global(ConfigChange::SubagentModel("inherit".into()))
            .expect("inherit should be persisted");
        let budgeted = modeled
            .config
            .change_global(ConfigChange::SubagentRequestBudget("50".into()))
            .expect("valid request budget should apply");
        let timed = budgeted
            .config
            .change_global(ConfigChange::SubagentTimeoutSecs("300".into()))
            .expect("valid timeout should apply");

        assert!(timed.config.subagents);
        assert_eq!(timed.config.max_subagents, 16);
        assert_eq!(timed.config.subagent_model, "inherit");
        assert_eq!(timed.config.subagent_request_budget, 50);
        assert_eq!(timed.config.subagent_timeout_secs, 300);
        assert!(
            timed
                .config
                .change_global(ConfigChange::MaxSubagents("17".into()))
                .is_err()
        );
        assert!(
            timed
                .config
                .change_global(ConfigChange::SubagentRequestBudget("1001".into()))
                .is_err()
        );
        assert!(
            timed
                .config
                .change_global(ConfigChange::SubagentTimeoutSecs("86401".into()))
                .is_err()
        );
        let saved: Value = serde_json::from_str(
            &fs::read_to_string(dirs.home.join("config.json"))
                .expect("saved subagent config should be readable"),
        )
        .expect("saved subagent config should be JSON");
        assert_eq!(saved["subagents"], true);
        assert_eq!(saved["max_subagents"], 16);
        assert_eq!(saved["subagent_model"], "inherit");
        assert_eq!(saved["subagent_request_budget"], 50);
        assert_eq!(saved["subagent_timeout_secs"], 300);
    }
}
